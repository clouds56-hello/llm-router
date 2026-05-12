pub(crate) mod completion;
pub(crate) mod parse;
mod request;
mod transformer;

pub(crate) use parse::{
  request_body_extract, request_header_extract, BodyExtract, ChatParser, HeaderExtract, MessagesParser, RequestParser,
  ResponsesParser,
};

use crate::api::{error::ApiError, AppState};
#[cfg(test)]
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::Response;
use llm_core::pipeline::ParsedRequest;
use llm_core::pipeline::{OutputTransformer, RequestResolver, RequestSender};
use request::{prepare_request, PoolResolver, ProviderSender};
use std::time::Instant;
use tracing::{debug, info_span, warn, Instrument};
use transformer::{EndpointOutputTransformer, UpstreamResponse};

const MAX_RETRIES: usize = 2;

pub(crate) async fn handle_endpoint(
  state: AppState,
  parsed: ParsedRequest,
  decoded: crate::api::codec::DecodedJsonRequest,
  request_id: String,
  started: Instant,
) -> Result<Response, ApiError> {
  let resolver = PoolResolver;
  let sender = ProviderSender;
  let transformer = EndpointOutputTransformer;
  let mut completion = completion::CompletionGuard::new(state.events.clone(), request_id.clone(), started);

  let span = tracing::Span::current();
  span.record("model", parsed.meta.model.as_str());
  span.record("stream", parsed.meta.stream);
  span.record("initiator", parsed.meta.initiator.as_str());
  span.record(
    "behave_as",
    tracing::field::display(crate::util::redact::BehaveAs(parsed.meta.behave_as.as_deref())),
  );

  let mut last_err: Option<(StatusCode, String)> = None;

  for attempt in 0..=MAX_RETRIES {
    let attempt_u32 = attempt as u32;
    completion.attempt(attempt_u32);

    let mut resolved = match resolver.resolve(&state, parsed.clone(), attempt) {
      Ok(resolved) => resolved,
      Err(e) => {
        completion.failure(Some(e.status().as_u16()), e.to_string());
        return Err(e);
      }
    };
    resolved.raw_body = decoded.raw_body.clone();
    resolved.decoded_body = decoded.decoded_body.clone();
    resolved.content_encoding = decoded.encoding;
    let account_id = resolved.account.id();
    crate::api::record_last_account(&account_id);

    let attempt_span = info_span!(
      "attempt",
      attempt,
      account = %account_id,
      provider = %resolved.account.provider.info().id,
      endpoint = %resolved.meta.endpoint,
      upstream_endpoint = %resolved.meta.upstream_endpoint,
      model = %resolved.route.requested_model,
      upstream_model = %resolved.meta.upstream_model,
      status = tracing::field::Empty,
    );

    let prepared = match prepare_request(resolved) {
      Ok(prepared) => prepared,
      Err(e) => {
        completion.failure(Some(StatusCode::BAD_GATEWAY.as_u16()), e.to_string());
        return Err(ApiError::bad_gateway(e.to_string()));
      }
    };

    state.events.emit(llm_core::event::Event::RequestParsed {
      request_id: request_id.clone(),
      attempt: attempt_u32,
      account_id: prepared.account.id(),
      provider_id: prepared.account.provider.info().id.clone(),
      model: prepared.meta.model.clone(),
      stream: prepared.meta.stream,
      initiator: prepared.meta.initiator.clone(),
      inbound_body: prepared.inbound_body_bytes.clone(),
    });

    let send_result = async {
      debug!("sending upstream request");
      sender.send(&state, &prepared).await
    }
    .instrument(attempt_span.clone())
    .await;

    let resp = match send_result {
      Ok(resp) => resp,
      Err(e) => {
        warn!(parent: &attempt_span, error = %e, "provider request failed");
        prepared.account.mark_failure(state.pool.cooldown_base());
        state.events.emit(llm_core::event::Event::RequestRetry {
          request_id: request_id.clone(),
          attempt: attempt_u32,
          error: e.to_string(),
        });
        last_err = Some((StatusCode::BAD_GATEWAY, e.to_string()));
        completion.failure(Some(StatusCode::BAD_GATEWAY.as_u16()), e.to_string());
        continue;
      }
    };

    let status = resp.status();
    attempt_span.record("status", status.as_u16());

    if status == StatusCode::UNAUTHORIZED {
      warn!(parent: &attempt_span, "401 from upstream; refreshing creds");
      prepared.account.invalidate_credentials();
      state.events.emit(llm_core::event::Event::RequestRetry {
        request_id: request_id.clone(),
        attempt: attempt_u32,
        error: "unauthorized".into(),
      });
      last_err = Some((status, "unauthorized".into()));
      completion.failure(Some(status.as_u16()), "unauthorized");
      continue;
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN || status.is_server_error() {
      let body_text = resp.text().await.unwrap_or_default();
      warn!(parent: &attempt_span, %status, body = %body_text, "upstream error; cooldown");
      prepared.account.mark_failure(state.pool.cooldown_base());
      state.events.emit(llm_core::event::Event::RequestRetry {
        request_id: request_id.clone(),
        attempt: attempt_u32,
        error: body_text.clone(),
      });
      completion.failure(Some(status.as_u16()), body_text.clone());
      last_err = Some((status, body_text));
      continue;
    }
    // Surface other 4xx (bad request, payload too large, etc.) verbatim so
    // clients see the upstream message instead of an empty SSE body. The
    // account is healthy in this case, so we do not cool it down or retry.
    if status.is_client_error() {
      let body_text = resp.text().await.unwrap_or_default();
      warn!(parent: &attempt_span, %status, body = %body_text, "upstream client error; surfacing verbatim");
      let msg = if body_text.trim().is_empty() {
        crate::api::error::fallback_upstream_message(status)
      } else {
        body_text
      };
      completion.failure(Some(status.as_u16()), msg.clone());
      return Err(ApiError::upstream(status, msg));
    }

    prepared.account.mark_success();
    if let Some(id) = prepared.meta.session_id.as_deref() {
      state.pool.record_session(id, &prepared.account.id());
    }

    // Emit RequestResponded with upstream status. Routed mode: outbound request snapshot
    // is now available via OutboundCapture (populated by the provider during send).
    let captured_outbound = prepared.capture.get().cloned();
    let (out_method, out_url, out_headers) = match captured_outbound.as_ref() {
      Some(snap) => (snap.method.clone(), snap.url.clone(), Some(snap.req_headers.clone())),
      None => (None, None, None),
    };
    // Body sent upstream: prefer the debug body (decoded) so subscribers see plain JSON;
    // fall back to the captured (post-encoding) body if debug is empty.
    let out_body = if !prepared.debug_outbound_body.is_empty() {
      Some(prepared.debug_outbound_body.clone())
    } else {
      captured_outbound.as_ref().map(|s| s.req_body.clone())
    };
    state.events.emit(llm_core::event::Event::RequestResponded {
      request_id: request_id.clone(),
      attempt: attempt_u32,
      outbound_status: status.as_u16(),
      latency_ms: started.elapsed().as_millis() as u64,
      outbound_resp_headers: resp.headers().clone(),
      outbound_req_method: out_method,
      outbound_req_url: out_url,
      outbound_req_headers: out_headers,
      outbound_req_body: out_body,
    });

    // Pass base request ID and attempt number into forward context via meta
    let mut meta = prepared.meta;
    meta.request_id = Some(request_id.clone());
    meta.attempt = attempt_u32;

    let upstream = UpstreamResponse {
      meta,
      inbound_body: prepared.inbound_body.clone(),
      resp,
      started,
    };
    let response = if parsed.meta.stream {
      // For streaming, RequestCompleted is emitted by the background stream recorder
      completion.disarm();
      transformer.transform_sse(state.clone(), upstream).await
    } else {
      let resp = transformer.transform_result(state.clone(), upstream).await;
      // Buffered: emit terminal RequestCompleted
      state.events.emit(llm_core::event::Event::RequestCompleted {
        request_id: request_id.clone(),
        success: true,
        total_attempts: attempt_u32 + 1,
        final_status: Some(status.as_u16()),
        total_latency_ms: started.elapsed().as_millis() as u64,
        error: None,
      });
      completion.disarm();
      resp
    };

    return Ok(response);
  }

  // All attempts failed
  let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
  state.events.emit(llm_core::event::Event::RequestCompleted {
    request_id: request_id.clone(),
    success: false,
    total_attempts: (MAX_RETRIES as u32) + 1,
    final_status: Some(status.as_u16()),
    total_latency_ms: started.elapsed().as_millis() as u64,
    error: Some(msg.clone()),
  });
  completion.disarm();
  Err(ApiError::upstream(status, msg))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::api::build_state;
  use crate::config::{Account as AccountCfg, Config};
  use crate::pipeline::request::ResolvedRequest;
  use crate::provider::{Endpoint, Provider};
  use crate::util::secret::Secret;
  use axum::http::HeaderValue;
  use bytes::Bytes;
  use llm_core::event::EventBus;
  use llm_core::pipeline::{InputTransformer, RequestMeta};
  use serde_json::json;
  use std::sync::Arc;

  fn zai_account() -> AccountCfg {
    AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: None,
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  #[test]
  fn chat_parser_reads_request_metadata() {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", HeaderValue::from_static("session-1"));
    headers.insert("x-request-id", HeaderValue::from_static("request-1"));
    headers.insert("x-opencode-project", HeaderValue::from_static("project-1"));
    headers.insert("x-behave-as", HeaderValue::from_static("architect"));
    let body = json!({
      "model": "gpt-4.1",
      "stream": true,
      "messages": [{"role": "user", "content": "hi"}]
    });

    let parsed = ChatParser.parse(headers.clone(), body.clone());

    assert_eq!(parsed.meta.endpoint, Endpoint::ChatCompletions);
    assert_eq!(parsed.meta.upstream_endpoint, Endpoint::ChatCompletions);
    assert_eq!(parsed.meta.model, "gpt-4.1");
    assert_eq!(parsed.meta.upstream_model, "gpt-4.1");
    assert!(parsed.meta.stream);
    assert_eq!(parsed.meta.session_id.as_deref(), Some("session-1"));
    assert_eq!(parsed.meta.request_id.as_deref(), Some("request-1"));
    assert_eq!(parsed.meta.project_id.as_deref(), Some("project-1"));
    assert_eq!(parsed.meta.behave_as.as_deref(), Some("architect"));
    assert_eq!(parsed.meta.initiator, "user");
    assert_eq!(parsed.body, body);
    assert_eq!(
      parsed.meta.inbound_headers.get("x-session-id"),
      headers.get("x-session-id")
    );
  }

  #[test]
  fn infers_stream_from_accept_header_when_body_omits_it() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::ACCEPT,
      HeaderValue::from_static("text/event-stream"),
    );
    let body = json!({
      "model": "gpt-5",
      "input": "hi"
    });

    let parsed = ResponsesParser.parse(headers, body);
    assert!(parsed.meta.stream);
  }

  #[test]
  fn explicit_stream_flag_overrides_accept_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::ACCEPT,
      HeaderValue::from_static("text/event-stream"),
    );
    let body = json!({
      "model": "gpt-5",
      "stream": false,
      "input": "hi"
    });

    let parsed = ResponsesParser.parse(headers, body);
    assert!(!parsed.meta.stream);
  }

  #[test]
  fn request_body_extract_prefers_header_initiator_and_body_stream_flag() {
    let mut headers = HeaderMap::new();
    headers.insert("x-initiator", HeaderValue::from_static("agent"));
    headers.insert(
      axum::http::header::ACCEPT,
      HeaderValue::from_static("text/event-stream"),
    );
    let body = json!({
      "model": "gpt-5",
      "stream": false,
      "messages": [{"role": "user", "content": "hi"}]
    });

    let body_meta = request_body_extract(&headers, &body);

    assert_eq!(body_meta.model, "gpt-5");
    assert!(!body_meta.stream);
    assert_eq!(body_meta.initiator, "agent");
    assert_eq!(body_meta.header_initiator.as_deref(), Some("agent"));
  }

  #[test]
  fn request_body_extract_falls_back_to_responses_body_classifier() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::ACCEPT,
      HeaderValue::from_static("text/event-stream"),
    );
    let body = json!({
      "input": [
        { "role": "user", "content": "x" },
        { "type": "function_call_output", "output": "42" }
      ]
    });

    let body_meta = request_body_extract(&headers, &body);

    assert_eq!(body_meta.model, "unknown");
    assert!(body_meta.stream);
    assert_eq!(body_meta.initiator, "agent");
    assert_eq!(body_meta.header_initiator, None);
  }

  #[test]
  fn prepare_request_converts_endpoint_and_applies_provider_transform() {
    let mut cfg = Config::default();
    cfg.accounts.push(zai_account());
    let state = build_state(&cfg, Arc::new(EventBus::noop())).unwrap();
    let account = state.pool.all()[0].clone();
    let route = state.route.resolve("glm-4.6", None).unwrap();
    let req = ResolvedRequest {
      meta: RequestMeta {
        endpoint: Endpoint::Responses,
        upstream_endpoint: Endpoint::ChatCompletions,
        model: "glm-4.6".into(),
        upstream_model: "glm-4.6".into(),
        stream: false,
        session_id: Some("session-1".into()),
        request_id: Some("request-1".into()),
        attempt: 0,
        project_id: Some("project-1".into()),
        initiator: "user".into(),
        header_initiator: None,
        behave_as: None,
        inbound_headers: HeaderMap::new(),
      },
      body: json!({
        "model": "glm-4.6",
        "input": "hi"
      }),
      raw_body: Bytes::from_static(br#"{"model":"glm-4.6","input":"hi"}"#),
      decoded_body: Bytes::from_static(br#"{"model":"glm-4.6","input":"hi"}"#),
      content_encoding: None,
      route,
      account,
    };

    let prepared = prepare_request(req).unwrap();

    assert_eq!(prepared.meta.endpoint, Endpoint::Responses);
    assert_eq!(prepared.meta.upstream_endpoint, Endpoint::ChatCompletions);
    assert_eq!(prepared.inbound_body["input"], json!("hi"));
    assert_eq!(prepared.upstream_body["model"], json!("glm-4.6"));
    assert!(
      prepared.upstream_body.get("messages").is_some(),
      "converted body missing messages"
    );
    assert_eq!(
      prepared
        .upstream_body
        .get("thinking")
        .and_then(|v| v.get("type"))
        .and_then(|v| v.as_str()),
      Some("enabled")
    );
  }

  #[test]
  fn copilot_transformer_is_identity() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "github-copilot".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: None,
      username: None,
      api_key: None,
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: Some(Secret::new("refresh-token".into())),
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let state = build_state(&cfg, Arc::new(EventBus::noop())).unwrap();
    let provider: &dyn Provider = state.pool.all()[0].provider.as_ref();
    let transformer: &dyn InputTransformer = provider.input_transformer().expect("copilot transformer");
    let body = json!({"model": "gpt-4.1", "messages": [{"role": "user", "content": "hi"}]});
    let meta = RequestMeta {
      endpoint: Endpoint::ChatCompletions,
      upstream_endpoint: Endpoint::ChatCompletions,
      model: "gpt-4.1".into(),
      upstream_model: "gpt-4.1".into(),
      stream: false,
      session_id: None,
      request_id: None,
      attempt: 0,
      project_id: None,
      initiator: "user".into(),
      header_initiator: None,
      behave_as: None,
      inbound_headers: HeaderMap::new(),
    };

    let transformed = transformer.transform_input(&meta, body.clone()).unwrap();
    assert_eq!(transformed, body);
  }
}
