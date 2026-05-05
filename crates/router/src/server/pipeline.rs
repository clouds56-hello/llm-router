use super::error::ApiError;
use super::forward::{buffered_response, stream_response};
use super::{first_header, AppState, PROJECT_ID_HEADERS, REQUEST_ID_HEADERS, SESSION_ID_HEADERS};
use crate::pool::{AccountHandle, EndpointAcquire};
use crate::provider::{new_outbound_capture, Endpoint, RequestCtx};
use crate::route::RouteResolution;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use llm_config::RouteMode;
use llm_core::pipeline::{ParsedRequest, RequestMeta};
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
    let initiator = match headers.get("x-initiator").and_then(|v| v.to_str().ok()) {
      Some(v) => {
        let value = v.trim().to_ascii_lowercase();
        if value == "user" || value == "agent" {
          value
        } else {
          self.auto_classify_initiator(&body).to_string()
        }
      }
      None => self.auto_classify_initiator(&body).to_string(),
    };
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
        project_id,
        initiator,
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
  account: Arc<AccountHandle>,
  resp: reqwest::Response,
  outbound: Option<crate::db::OutboundSnapshot>,
  started: Instant,
}

trait RequestResolver: Send + Sync {
  fn resolve(&self, state: &AppState, parsed: ParsedRequest, attempt: usize) -> Result<ResolvedRequest, ApiError>;
}

trait RequestSender: Send + Sync {
  fn send<'a>(
    &'a self,
    state: &'a AppState,
    req: &'a PreparedRequest,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::provider::Result<reqwest::Response>> + Send + 'a>>;
}

trait OutputTransformer: Send + Sync {
  fn transform_result<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>>;

  fn transform_sse<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>>;
}

trait RequestReporter: Send + Sync {
  fn report_buffered<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>>;

  fn report_stream<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>>;
}

struct PoolResolver;
struct ProviderSender;
struct EndpointOutputTransformer;
struct DbReporter;

impl RequestResolver for PoolResolver {
  fn resolve(&self, state: &AppState, parsed: ParsedRequest, attempt: usize) -> Result<ResolvedRequest, ApiError> {
    resolve_request(state, parsed, attempt)
  }
}

impl RequestSender for ProviderSender {
  fn send<'a>(
    &'a self,
    state: &'a AppState,
    req: &'a PreparedRequest,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::provider::Result<reqwest::Response>> + Send + 'a>> {
    Box::pin(send_request(state, req))
  }
}

impl OutputTransformer for EndpointOutputTransformer {
  fn transform_result<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>> {
    Box::pin(buffered_response(
      state,
      upstream.account,
      upstream.resp,
      upstream.meta.endpoint,
      upstream.meta.upstream_endpoint,
      upstream.meta.model,
      upstream.meta.initiator,
      upstream.meta.session_id,
      upstream.meta.request_id,
      upstream.meta.project_id,
      upstream.meta.inbound_headers,
      upstream.inbound_body,
      upstream.outbound,
      upstream.started,
    ))
  }

  fn transform_sse<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>> {
    Box::pin(stream_response(
      state,
      upstream.account,
      upstream.resp,
      upstream.meta.endpoint,
      upstream.meta.upstream_endpoint,
      upstream.meta.model,
      upstream.meta.initiator,
      upstream.meta.session_id,
      upstream.meta.request_id,
      upstream.meta.project_id,
      upstream.meta.inbound_headers,
      upstream.inbound_body,
      upstream.outbound,
      upstream.started,
    ))
  }
}

impl RequestReporter for DbReporter {
  fn report_buffered<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>> {
    EndpointOutputTransformer.transform_result(state, upstream)
  }

  fn report_stream<'a>(
    &'a self,
    state: AppState,
    upstream: UpstreamResponse,
  ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send + 'a>> {
    EndpointOutputTransformer.transform_sse(state, upstream)
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
  let reporter = DbReporter;
  let parsed = parser.parse(headers, body);
  let started = Instant::now();

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
        last_err = Some((StatusCode::BAD_GATEWAY, e.to_string()));
        continue;
      }
    };

    let status = resp.status();
    attempt_span.record("status", status.as_u16());

    if status == StatusCode::UNAUTHORIZED {
      warn!(parent: &attempt_span, "401 from upstream; refreshing creds");
      prepared.account.invalidate_credentials();
      last_err = Some((status, "unauthorized".into()));
      continue;
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN || status.is_server_error() {
      let body_text = resp.text().await.unwrap_or_default();
      warn!(parent: &attempt_span, %status, body = %body_text, "upstream error; cooldown");
      prepared.account.mark_failure(state.pool.cooldown_base());
      last_err = Some((status, body_text));
      continue;
    }

    prepared.account.mark_success();
    if let Some(id) = prepared.meta.session_id.as_deref() {
      state.pool.record_session(id, &prepared.account.id());
    }

    let outbound = prepared.capture.get().cloned();
    let upstream = UpstreamResponse {
      meta: prepared.meta,
      inbound_body: prepared.inbound_body,
      account: prepared.account,
      resp,
      outbound,
      started,
    };
    return Ok(if parsed.meta.stream {
      reporter.report_stream(state.clone(), upstream).await
    } else {
      reporter.report_buffered(state.clone(), upstream).await
    });
  }

  let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
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
