pub(crate) mod completion;
pub(crate) mod parse;
pub(crate) mod request;
mod transformer;

pub(crate) use parse::{
  request_body_extract, request_header_extract, BodyExtract, ChatParser, HeaderExtract, MessagesParser, RequestParser,
  ResponsesParser,
};

use crate::api::{error::ApiError, AppState};
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::response::Response;
use bytes::Bytes;
use tokn_core::db::{SessionSource, Usage};
use tokn_core::event::{Event, LegacyRequestEvent};
use tokn_core::pipeline::ParsedRequest;
use tokn_core::pipeline::{OutputTransformer, RequestResolver, RequestSender};
use request::{prepare_request, PoolResolver, PreparedRequest, ProviderSender};

pub use request::{dry_run_request, DryRunEndpoint, DryRunOutput};
use std::time::Instant;
use tracing::{debug, info_span, warn, Instrument};
use transformer::{EndpointOutputTransformer, UpstreamResponse};

const MAX_RETRIES: usize = 2;

/// JSON error envelope content-type used by `ApiError::IntoResponse`.
fn json_envelope_headers() -> HeaderMap {
  let mut h = HeaderMap::new();
  h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
  h
}

/// Emit `RequestResponded` carrying upstream response status/headers and the
/// outbound request snapshot captured during `provider.send`. Mirrors the
/// success-path emission so DB rows on error branches still receive
/// `outbound_*` metadata.
fn emit_request_responded(
  state: &AppState,
  request_id: &str,
  attempt: u32,
  prepared: &PreparedRequest,
  status: StatusCode,
  resp_headers: &HeaderMap,
  started: Instant,
) {
  let captured_outbound = prepared.capture.get().cloned();
  let (out_method, out_url, out_headers) = match captured_outbound.as_ref() {
    Some(snap) => (snap.method.clone(), snap.url.clone(), Some(snap.req_headers.clone())),
    None => (None, None, None),
  };
  let out_body = if !prepared.debug_outbound_body.is_empty() {
    Some(prepared.debug_outbound_body.clone())
  } else {
    captured_outbound.as_ref().map(|s| s.req_body.clone())
  };
  state.events.emit(Event::LegacyRequest(LegacyRequestEvent::Responded {
    request_id: request_id.to_string(),
    attempt,
    outbound_status: status.as_u16(),
    latency_ms: started.elapsed().as_millis() as u64,
    outbound_resp_headers: resp_headers.into(),
    outbound_req_method: out_method,
    outbound_req_url: out_url,
    outbound_req_headers: out_headers,
    outbound_req_body: out_body,
  }));
}

/// Build the `RequestResult` event for a terminal failure path. The downstream
/// JSON envelope (`inbound_resp_*`) is synthesised from `ApiError` so DB rows
/// match the bytes/headers actually returned to the client. `upstream_body` is
/// the upstream response body when one was received (`None` for transport
/// failures before any upstream reply).
pub(crate) fn build_failure_result_event(
  request_id: String,
  attempt: u32,
  started: Instant,
  status: StatusCode,
  error_msg: String,
  upstream_body: Option<Bytes>,
) -> Event {
  let api_err = ApiError::upstream(status, error_msg.clone());
  build_failure_result_event_from_api_err(request_id, attempt, started, &api_err, upstream_body)
}

/// Same as [`build_failure_result_event`] but uses the exact `ApiError`
/// supplied so the persisted envelope (status, `type`, message) matches what
/// the client actually receives — including non-upstream kinds like
/// `not_implemented_error` and `bad_gateway`.
pub(crate) fn build_failure_result_event_from_api_err(
  request_id: String,
  attempt: u32,
  started: Instant,
  api_err: &ApiError,
  upstream_body: Option<Bytes>,
) -> Event {
  Event::LegacyRequest(LegacyRequestEvent::Result {
    request_id,
    attempt,
    session_source: SessionSource::Auto,
    latency_ms: started.elapsed().as_millis() as u64,
    inbound_status: api_err.status().as_u16(),
    usage: Usage::default(),
    request_error: Some(api_err.to_string()),
    inbound_resp_headers: (&json_envelope_headers()).into(),
    inbound_resp_body: api_err.body_bytes(),
    outbound_resp_body: upstream_body,
    messages: Vec::new(),
  })
}

/// Emit the synthetic `RequestParsed` + `RequestResult` pair used by
/// early-failure paths in [`handle_endpoint`] (resolver/prepare failures that
/// abort before any upstream send). This ensures DB rows still get the
/// requested `model`, the inbound request body, and the rendered JSON error
/// envelope even though no account was ever selected.
///
/// `account_id`/`provider_id` are persisted as the sentinel `"none"` because
/// route resolution did not yield one.
fn emit_early_failure(
  state: &AppState,
  request_id: &str,
  attempt: u32,
  started: Instant,
  parsed_meta: &tokn_core::pipeline::RequestMeta,
  inbound_body: Bytes,
  api_err: &ApiError,
) {
  state.events.emit(Event::LegacyRequest(LegacyRequestEvent::Parsed {
    request_id: request_id.to_string(),
    attempt,
    account_id: "none".to_string(),
    provider_id: "none".to_string(),
    model: parsed_meta.model.clone(),
    stream: parsed_meta.stream,
    initiator: parsed_meta.initiator.clone(),
    behave_as: parsed_meta.behave_as.clone(),
    inbound_body,
  }));
  state.events.emit(build_failure_result_event_from_api_err(
    request_id.to_string(),
    attempt,
    started,
    api_err,
    None,
  ));
}

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
        emit_early_failure(
          &state,
          &request_id,
          attempt_u32,
          started,
          &parsed.meta,
          decoded.decoded_body.clone(),
          &e,
        );
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
        let api_err = ApiError::bad_gateway(e.to_string());
        emit_early_failure(
          &state,
          &request_id,
          attempt_u32,
          started,
          &parsed.meta,
          decoded.decoded_body.clone(),
          &api_err,
        );
        completion.failure(Some(api_err.status().as_u16()), api_err.to_string());
        return Err(api_err);
      }
    };

    state.events.emit(tokn_core::event::Event::LegacyRequest(
      tokn_core::event::LegacyRequestEvent::Parsed {
        request_id: request_id.clone(),
        attempt: attempt_u32,
        account_id: prepared.account.id(),
        provider_id: prepared.account.provider.info().id.clone(),
        model: prepared.meta.model.clone(),
        stream: prepared.meta.stream,
        initiator: prepared.meta.initiator.clone(),
        behave_as: prepared.meta.behave_as.clone(),
        inbound_body: prepared.inbound_body_bytes.clone(),
      },
    ));

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
        state.events.emit(tokn_core::event::Event::LegacyRequest(
          tokn_core::event::LegacyRequestEvent::Retry {
            request_id: request_id.clone(),
            attempt: attempt_u32,
            error: e.to_string(),
          },
        ));
        last_err = Some((StatusCode::BAD_GATEWAY, e.to_string()));
        completion.failure(Some(StatusCode::BAD_GATEWAY.as_u16()), e.to_string());
        continue;
      }
    };

    let status = resp.status();
    attempt_span.record("status", status.as_u16());

    if status == StatusCode::UNAUTHORIZED {
      warn!(parent: &attempt_span, "401 from upstream; refreshing creds");
      let resp_headers = resp.headers().clone();
      let body_text = resp.text().await.unwrap_or_default();
      emit_request_responded(
        &state,
        &request_id,
        attempt_u32,
        &prepared,
        status,
        &resp_headers,
        started,
      );
      prepared.account.invalidate_credentials();
      state.events.emit(tokn_core::event::Event::LegacyRequest(
        tokn_core::event::LegacyRequestEvent::Retry {
          request_id: request_id.clone(),
          attempt: attempt_u32,
          error: "unauthorized".into(),
        },
      ));
      let err_msg = if body_text.trim().is_empty() {
        "unauthorized".to_string()
      } else {
        body_text
      };
      last_err = Some((status, err_msg.clone()));
      completion.failure(Some(status.as_u16()), err_msg);
      continue;
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN || status.is_server_error() {
      let resp_headers = resp.headers().clone();
      let body_text = resp.text().await.unwrap_or_default();
      warn!(parent: &attempt_span, %status, body = %body_text, "upstream error; cooldown");
      emit_request_responded(
        &state,
        &request_id,
        attempt_u32,
        &prepared,
        status,
        &resp_headers,
        started,
      );
      prepared.account.mark_failure(state.pool.cooldown_base());
      state.events.emit(tokn_core::event::Event::LegacyRequest(
        tokn_core::event::LegacyRequestEvent::Retry {
          request_id: request_id.clone(),
          attempt: attempt_u32,
          error: body_text.clone(),
        },
      ));
      completion.failure(Some(status.as_u16()), body_text.clone());
      last_err = Some((status, body_text));
      continue;
    }
    // Surface other 4xx (bad request, payload too large, etc.) verbatim so
    // clients see the upstream message instead of an empty SSE body. The
    // account is healthy in this case, so we do not cool it down or retry.
    if status.is_client_error() {
      let resp_headers = resp.headers().clone();
      let body_text = resp.text().await.unwrap_or_default();
      warn!(parent: &attempt_span, %status, body = %body_text, "upstream client error; surfacing verbatim");
      emit_request_responded(
        &state,
        &request_id,
        attempt_u32,
        &prepared,
        status,
        &resp_headers,
        started,
      );
      let upstream_body_bytes = if body_text.is_empty() {
        None
      } else {
        Some(Bytes::copy_from_slice(body_text.as_bytes()))
      };
      let msg = if body_text.trim().is_empty() {
        crate::api::error::fallback_upstream_message(status)
      } else {
        body_text
      };
      state.events.emit(build_failure_result_event(
        request_id.clone(),
        attempt_u32,
        started,
        status,
        msg.clone(),
        upstream_body_bytes,
      ));
      completion.failure(Some(status.as_u16()), msg.clone());
      return Err(ApiError::upstream(status, msg));
    }

    prepared.account.mark_success();
    if let Some(id) = prepared.meta.session_id.as_deref() {
      state.pool.record_session(id, &prepared.account.id());
    }

    // Emit RequestResponded with upstream status. Routed mode: outbound request snapshot
    // is now available via OutboundCapture (populated by the provider during send).
    let resp_headers = resp.headers().clone();
    emit_request_responded(
      &state,
      &request_id,
      attempt_u32,
      &prepared,
      status,
      &resp_headers,
      started,
    );

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
      state.events.emit(tokn_core::event::Event::LegacyRequest(
        tokn_core::event::LegacyRequestEvent::Completed {
          request_id: request_id.clone(),
          success: true,
          total_attempts: attempt_u32 + 1,
          final_status: Some(status.as_u16()),
          total_latency_ms: started.elapsed().as_millis() as u64,
          error: None,
        },
      ));
      completion.disarm();
      resp
    };

    return Ok(response);
  }

  // All attempts failed
  let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
  state.events.emit(build_failure_result_event(
    request_id.clone(),
    MAX_RETRIES as u32,
    started,
    status,
    msg.clone(),
    None,
  ));
  state.events.emit(tokn_core::event::Event::LegacyRequest(
    tokn_core::event::LegacyRequestEvent::Completed {
      request_id: request_id.clone(),
      success: false,
      total_attempts: (MAX_RETRIES as u32) + 1,
      final_status: Some(status.as_u16()),
      total_latency_ms: started.elapsed().as_millis() as u64,
      error: Some(msg.clone()),
    },
  ));
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
  use tokn_core::event::EventBus;
  use tokn_core::pipeline::{InputTransformer, RequestMeta};
  use serde_json::json;
  use std::sync::Arc;

  fn zai_account() -> AccountCfg {
    AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tier: tokn_core::account::AccountTier::Active,
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
      provider_account_id: None,
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
      parsed.meta.inbound_headers.get("x-session-id").map(|v| v.as_str()),
      headers.get("x-session-id").and_then(|v| v.to_str().ok())
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
    let cfg = Config::default();
    let accounts = vec![zai_account()];
    let state = build_state(&cfg, &accounts, Arc::new(EventBus::noop())).unwrap();
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
        inbound_headers: tokn_headers::HeaderMap::new(),
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
  fn prepare_request_builds_profile_headers_from_inbound_templates() {
    let cfg = Config::default();
    let accounts = vec![zai_account()];
    let state = build_state(&cfg, &accounts, Arc::new(EventBus::noop())).unwrap();
    let account = state.pool.all()[0].clone();
    let route = state.route.resolve("glm-4.6", None).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-session-affinity", HeaderValue::from_static("ses_test"));
    headers.insert("x-initiator", HeaderValue::from_static("agent"));
    headers.insert("authorization", HeaderValue::from_static("Bearer inbound"));
    let req = ResolvedRequest {
      meta: RequestMeta {
        endpoint: Endpoint::ChatCompletions,
        upstream_endpoint: Endpoint::ChatCompletions,
        model: "glm-4.6".into(),
        upstream_model: "glm-4.6".into(),
        stream: false,
        session_id: Some("ses_test".into()),
        request_id: Some("request-1".into()),
        attempt: 0,
        project_id: None,
        initiator: "agent".into(),
        header_initiator: Some("agent".into()),
        behave_as: Some("opencode".into()),
        inbound_headers: (&headers).into(),
      },
      body: json!({
        "model": "glm-4.6",
        "messages": [{"role": "user", "content": "hi"}]
      }),
      raw_body: Bytes::new(),
      decoded_body: Bytes::new(),
      content_encoding: None,
      route,
      account,
    };

    let prepared = prepare_request(req).unwrap();
    let h = prepared.profile_headers.expect("profile headers");

    assert_eq!(
      h.get("x-session-affinity").and_then(|v| v.to_str().ok()),
      Some("ses_test")
    );
    assert_eq!(h.get("x-initiator").and_then(|v| v.to_str().ok()), Some("agent"));
    assert_eq!(
      h.get("user-agent").and_then(|v| v.to_str().ok()),
      Some("opencode/1.14.28 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13")
    );
    assert!(h.get("authorization").is_none());
  }

  #[test]
  fn build_failure_result_event_synthesises_inbound_envelope() {
    let event = build_failure_result_event(
      "req-1".into(),
      0,
      Instant::now(),
      StatusCode::BAD_REQUEST,
      "tools.0.function.name empty".into(),
      Some(Bytes::from_static(b"{\"error\":\"upstream body\"}")),
    );
    let Event::LegacyRequest(LegacyRequestEvent::Result {
      inbound_status,
      inbound_resp_body,
      outbound_resp_body,
      request_error,
      inbound_resp_headers,
      ..
    }) = event
    else {
      panic!("wrong variant");
    };
    assert_eq!(inbound_status, 400);
    assert!(request_error.as_deref().unwrap().contains("tools.0.function.name"));
    assert_eq!(
      inbound_resp_headers.get("content-type").unwrap().as_str(),
      "application/json"
    );
    let envelope: serde_json::Value = serde_json::from_slice(&inbound_resp_body).unwrap();
    assert_eq!(envelope["error"]["code"], 400);
    assert_eq!(envelope["error"]["type"], "upstream_error");
    assert!(envelope["error"]["message"]
      .as_str()
      .unwrap()
      .contains("tools.0.function.name"));
    assert_eq!(outbound_resp_body.unwrap().as_ref(), b"{\"error\":\"upstream body\"}");
  }

  #[test]
  fn build_failure_result_event_no_upstream_body() {
    let event = build_failure_result_event(
      "req-2".into(),
      2,
      Instant::now(),
      StatusCode::BAD_GATEWAY,
      "token exchange: HTTP request failed".into(),
      None,
    );
    let Event::LegacyRequest(LegacyRequestEvent::Result {
      inbound_status,
      outbound_resp_body,
      attempt,
      ..
    }) = event
    else {
      panic!("wrong variant");
    };
    assert_eq!(inbound_status, 502);
    assert_eq!(attempt, 2);
    assert!(outbound_resp_body.is_none());
  }

  #[test]
  fn build_failure_result_event_from_api_err_preserves_not_implemented_envelope() {
    let api_err = ApiError::not_implemented("messages", "claude-sonnet-4-6");
    let event = build_failure_result_event_from_api_err("req-3".into(), 0, Instant::now(), &api_err, None);
    let Event::LegacyRequest(LegacyRequestEvent::Result {
      inbound_status,
      inbound_resp_body,
      request_error,
      ..
    }) = event
    else {
      panic!("wrong variant");
    };
    assert_eq!(inbound_status, 501);
    let err_msg = request_error.expect("request_error populated");
    assert!(err_msg.contains("messages"));
    assert!(err_msg.contains("claude-sonnet-4-6"));
    let envelope: serde_json::Value = serde_json::from_slice(&inbound_resp_body).unwrap();
    assert_eq!(envelope["error"]["code"], 501);
    assert_eq!(envelope["error"]["type"], "not_implemented_error");
    assert!(envelope["error"]["message"]
      .as_str()
      .unwrap()
      .contains("claude-sonnet-4-6"));
  }

  #[test]
  fn copilot_transformer_is_identity() {
    let cfg = Config::default();
    let accounts = vec![AccountCfg {
      id: "acct".into(),
      provider: "github-copilot".into(),
      enabled: true,
      tier: tokn_core::account::AccountTier::Active,
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
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }];
    let state = build_state(&cfg, &accounts, Arc::new(EventBus::noop())).unwrap();
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
      inbound_headers: tokn_headers::HeaderMap::new(),
    };

    let transformed = transformer
      .transform_input(meta.upstream_endpoint, body.clone())
      .unwrap();
    assert_eq!(transformed, body);
  }
}
