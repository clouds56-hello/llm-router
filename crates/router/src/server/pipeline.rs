use super::error::ApiError;
use super::forward::{buffered_response, stream_response, ForwardContext};
use super::{first_header, AppState, PROJECT_ID_HEADERS, REQUEST_ID_HEADERS, SESSION_ID_HEADERS};
use crate::pool::{AccountHandle, EndpointAcquire};
use crate::provider::{new_outbound_capture, Endpoint, RequestCtx};
use crate::route::RouteResolution;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use llm_config::RouteMode;
use llm_core::pipeline::{
  OutputTransformer, ParsedRequest, RequestMeta, RequestResolver, RequestSender,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info_span, warn, Instrument};

const MAX_RETRIES: usize = 2;

pub(crate) trait RequestParser: Send + Sync {
  fn endpoint(&self) -> Endpoint;

  fn auto_classify_initiator(&self, body: &Value) -> &'static str;

  fn parse(&self, headers: HeaderMap, body: Value) -> ParsedRequest {
    let model = body
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("unknown")
      .to_string();
    let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let session_id = first_header(&headers, SESSION_ID_HEADERS).map(str::to_string);
    let request_id = first_header(&headers, REQUEST_ID_HEADERS).map(str::to_string);
    let project_id = first_header(&headers, PROJECT_ID_HEADERS).map(str::to_string);
    let header_initiator = headers.get("x-initiator")
      .and_then(|v| v.to_str().ok())
      .map(|v| v.trim().to_ascii_lowercase())
      .filter(|v| v == "user" || v == "agent");
    let initiator = header_initiator.clone()
      .unwrap_or_else(|| self.auto_classify_initiator(&body).to_string());
    let behave_as = headers
      .get("x-behave-as")
      .and_then(|v| v.to_str().ok())
      .map(|s| s.trim().to_string())
      .filter(|s| !s.is_empty());

    ParsedRequest {
      meta: RequestMeta {
        endpoint: self.endpoint(),
        upstream_endpoint: self.endpoint(),
        model: model.clone(),
        upstream_model: model,
        stream,
        session_id,
        request_id,
        attempt: 0,
        project_id,
        initiator,
        header_initiator,
        behave_as,
        inbound_headers: headers,
      },
      body,
    }
  }
}

pub(crate) struct ChatParser;
pub(crate) struct ResponsesParser;
pub(crate) struct MessagesParser;

impl RequestParser for ChatParser {
  fn endpoint(&self) -> Endpoint {
    Endpoint::ChatCompletions
  }

  fn auto_classify_initiator(&self, body: &Value) -> &'static str {
    crate::util::initiator::classify_initiator(body)
  }
}

impl RequestParser for ResponsesParser {
  fn endpoint(&self) -> Endpoint {
    Endpoint::Responses
  }

  fn auto_classify_initiator(&self, body: &Value) -> &'static str {
    crate::util::initiator::classify_initiator_responses(body)
  }
}

impl RequestParser for MessagesParser {
  fn endpoint(&self) -> Endpoint {
    Endpoint::Messages
  }

  fn auto_classify_initiator(&self, body: &Value) -> &'static str {
    crate::util::initiator::classify_initiator(body)
  }
}

#[derive(Clone)]
struct ResolvedRequest {
  meta: RequestMeta,
  body: Value,
  route: RouteResolution,
  account: Arc<AccountHandle>,
}

struct PreparedRequest {
  meta: RequestMeta,
  inbound_body: Value,
  upstream_body: Value,
  account: Arc<AccountHandle>,
  capture: crate::provider::OutboundCapture,
}

struct UpstreamResponse {
  meta: RequestMeta,
  inbound_body: Value,
  resp: reqwest::Response,
  started: Instant,
}

struct PoolResolver;
struct ProviderSender;
struct EndpointOutputTransformer;

impl RequestResolver for PoolResolver {
  type State = AppState;
  type Resolved = ResolvedRequest;
  type Error = ApiError;

  fn resolve(&self, state: &AppState, parsed: ParsedRequest, attempt: usize) -> Result<ResolvedRequest, ApiError> {
    resolve_request(state, parsed, attempt)
  }
}

impl RequestSender for ProviderSender {
  type State = AppState;
  type Request = PreparedRequest;
  type Response = reqwest::Response;
  type Error = crate::provider::error::Error;

  fn send<'a>(
    &'a self,
    state: &'a AppState,
    req: &'a PreparedRequest,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::provider::Result<reqwest::Response>> + Send + 'a>> {
    Box::pin(send_request(state, req))
  }
}

#[async_trait]
impl OutputTransformer for EndpointOutputTransformer {
  type State = AppState;
  type Upstream = UpstreamResponse;
  type Output = Response;

  async fn transform_result(&self, state: AppState, upstream: UpstreamResponse) -> Response {
    let ctx = ForwardContext::from_pipeline(
      upstream.meta.endpoint,
      upstream.meta.upstream_endpoint,
      upstream.meta.model,
      upstream.meta.session_id,
      upstream.meta.request_id.unwrap_or_default(),
      upstream.meta.attempt,
      upstream.started,
    );
    buffered_response(state, upstream.resp, ctx, &upstream.inbound_body).await
  }

  async fn transform_sse(
    &self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> Response {
    let ctx = ForwardContext::from_pipeline(
      upstream.meta.endpoint,
      upstream.meta.upstream_endpoint,
      upstream.meta.model,
      upstream.meta.session_id,
      upstream.meta.request_id.unwrap_or_default(),
      upstream.meta.attempt,
      upstream.started,
    );
    stream_response(state, upstream.resp, ctx, &upstream.inbound_body).await
  }
}

pub(crate) async fn handle_endpoint(
  state: AppState,
  parser: &dyn RequestParser,
  headers: HeaderMap,
  body: Value,
) -> Result<Response, ApiError> {
  let resolver = PoolResolver;
  let sender = ProviderSender;
  let transformer = EndpointOutputTransformer;
  let parsed = parser.parse(headers, body);
  let started = Instant::now();

  // Generate request_id if not provided
  let request_id = parsed.meta.request_id.clone()
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

  state.events.emit(llm_core::event::Event::RequestStarted {
    request_id: request_id.clone(),
    ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64,
    endpoint: parser.endpoint().as_str().to_string(),
    initiator: parsed.meta.header_initiator.clone(),    session_id: parsed.meta.session_id.clone(),
    project_id: parsed.meta.project_id.clone(),
    inbound_req: llm_core::db::HttpSnapshot::default(),
  });

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

    let resolved = resolver.resolve(&state, parsed.clone(), attempt)?;
    let account_id = resolved.account.id();
    super::record_last_account(&account_id);

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

    let prepared = prepare_request(resolved).map_err(|e| ApiError::bad_gateway(e.to_string()))?;

    // Emit RequestParsed per attempt
    state.events.emit(llm_core::event::Event::RequestParsed {
      request_id: request_id.clone(),
      attempt: attempt_u32,
      account_id: prepared.account.id(),
      provider_id: prepared.account.provider.info().id.clone(),
      model: prepared.meta.model.clone(),
      stream: prepared.meta.stream,
      initiator: prepared.meta.initiator.clone(),
      outbound_req: prepared.capture.get().cloned(),
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
      last_err = Some((status, body_text));
      continue;
    }

    prepared.account.mark_success();
    if let Some(id) = prepared.meta.session_id.as_deref() {
      state.pool.record_session(id, &prepared.account.id());
    }

    // Emit RequestResponded with upstream status
    state.events.emit(llm_core::event::Event::RequestResponded {
      request_id: request_id.clone(),
      status: status.as_u16(),
      resp_headers: resp.headers().clone(),
    });

    // Pass base request ID and attempt number into forward context via meta
    let mut meta = prepared.meta;
    meta.request_id = Some(request_id.clone());
    meta.attempt = attempt_u32;

    let upstream = UpstreamResponse {
      meta,
      inbound_body: prepared.inbound_body,
      resp,
      started,
    };
    let response = if parsed.meta.stream {
      // For streaming, RequestCompleted is emitted by the background stream recorder
      transformer
        .transform_sse(state.clone(), upstream)
        .await
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
    final_status: None,
    total_latency_ms: started.elapsed().as_millis() as u64,
    error: Some(msg.clone()),
  });
  Err(ApiError::upstream(status, msg))
}

fn resolve_request(state: &AppState, parsed: ParsedRequest, attempt: usize) -> Result<ResolvedRequest, ApiError> {
  let route = state
    .route
    .resolve(
      &parsed.meta.model,
      parsed
        .meta
        .inbound_headers
        .get(crate::route::RouteResolver::mode_header())
        .and_then(|v| v.to_str().ok()),
    )
    .map_err(|e| ApiError::bad_request(e.to_string()))?;
  if route.mode == RouteMode::Passthrough {
    return Err(ApiError::bad_request("passthrough mode only applies in proxy mode"));
  }
  let (account, upstream_endpoint) =
    match state
      .pool
      .acquire_for_route(parsed.meta.session_id.as_deref(), &route, parsed.meta.endpoint)
    {
      EndpointAcquire::Account { acct, endpoint } => (acct, endpoint),
      EndpointAcquire::SessionExpired => {
        let id = parsed.meta.session_id.clone().unwrap_or_default();
        warn!(%parsed.meta.endpoint, model = %parsed.meta.model, session_id = %id, attempt, "session expired");
        return Err(ApiError::session_expired(id));
      }
      EndpointAcquire::None => {
        warn!(%parsed.meta.endpoint, model = %parsed.meta.model, attempt, "no account supports endpoint/model");
        return Err(ApiError::not_implemented(
          parsed.meta.endpoint.to_string(),
          parsed.meta.model.clone(),
        ));
      }
    };

  let mut meta = parsed.meta;
  meta.upstream_endpoint = upstream_endpoint;
  meta.upstream_model = route.upstream_model.clone();
  Ok(ResolvedRequest {
    meta,
    body: parsed.body,
    route,
    account,
  })
}

fn prepare_request(req: ResolvedRequest) -> crate::provider::Result<PreparedRequest> {
  let mut upstream_body = rewrite_model(&req.body, &req.meta.upstream_model);
  if req.meta.upstream_endpoint != req.meta.endpoint {
    upstream_body = crate::convert::convert_request(req.meta.endpoint, req.meta.upstream_endpoint, &upstream_body)
      .map_err(|source| crate::provider::error::Error::Profiles {
        message: format!("request conversion failed: {source}"),
      })?;
  }
  if let Some(transformer) = req.account.provider.input_transformer() {
    upstream_body = transformer.transform_input(&req.meta, upstream_body)?;
  }
  Ok(PreparedRequest {
    meta: req.meta,
    inbound_body: req.body,
    upstream_body,
    account: req.account,
    capture: new_outbound_capture(),
  })
}

async fn send_request(state: &AppState, req: &PreparedRequest) -> crate::provider::Result<reqwest::Response> {
  let ctx = RequestCtx {
    endpoint: req.meta.upstream_endpoint,
    http: &state.http,
    body: &req.upstream_body,
    stream: req.meta.stream,
    initiator: req.meta.initiator.as_str(),
    inbound_headers: &req.meta.inbound_headers,
    behave_as: req.meta.behave_as.as_deref(),
    outbound: Some(req.capture.clone()),
  };
  match req.meta.upstream_endpoint {
    Endpoint::ChatCompletions => req.account.provider.chat(ctx).await,
    Endpoint::Responses => req.account.provider.responses(ctx).await,
    Endpoint::Messages => req.account.provider.messages(ctx).await,
  }
}

fn rewrite_model(body: &Value, model: &str) -> Value {
  let mut body = body.clone();
  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".into(), Value::String(model.to_string()));
  }
  body
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::{Account as AccountCfg, Config};
  use crate::provider::{Endpoint, Provider};
  use crate::server::build_state;
  use llm_core::event::EventBus;
  use crate::util::secret::Secret;
  use axum::http::HeaderValue;
  use llm_core::pipeline::InputTransformer;
  use serde_json::json;

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
