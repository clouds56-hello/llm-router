use super::error::ApiError;
use super::AppState;
use crate::pipeline::{request_header_extract, ChatParser, MessagesParser, RequestParser, ResponsesParser};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use llm_accounts::routing::{route_mode_as_str, ResolveError};
use llm_core::request_event::{RecordEvent, RequestEvent, RequestEventPayload};
use llm_core::event::Event as CoreEvent;
use llm_requests::pipeline::error::RequestsError;
use llm_requests::Stage;
use smol_str::SmolStr;
use tracing::instrument;

async fn handle(
  state: AppState,
  parser: &dyn RequestParser,
  inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  let hx = request_header_extract(&inbound);
  let local_addr = inbound
    .get("x-llm-router-local-addr")
    .and_then(|v| v.to_str().ok())
    .map(str::to_string)
    .or_else(|| {
      inbound
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    });
  // Router-owned JSON endpoints run through llm-requests and skip legacy
  // lifecycle emission. The pipeline emits its own StageEvent/RecordEvent
  // stream which RequestEventHandler consumes; emitting
  // LegacyRequestEvent::Started here as well would cause DbEventHandler to
  // insert a row first, then RequestEventHandler's INSERT on
  // StageEvent::Started would hit a UNIQUE constraint and orphan
  // subsequent stage UPDATEs.
  let mode = state.route.resolve_mode(hx.route_mode_hint.as_deref()).ok();

  state.events.emit(CoreEvent::Requests(RequestEvent {
    request_id: SmolStr::new(&hx.request_id),
    attempt: 0,
    ts: llm_core::util::now_unix_ms(),
    payload: RequestEventPayload::Record(RecordEvent::InboundConnection {
      local_addr: local_addr.clone().map(SmolStr::from),
      peer_addr: None,
      mode: SmolStr::new(request_record_mode(mode)),
      method: SmolStr::new("requests"),
      inbound_method: SmolStr::new("POST"),
      url: None,
    }),
  }));
  let decoded = super::codec::decode_json_request(&inbound, body)?;
  let raw = llm_requests::RawInbound {
    endpoint: parser.endpoint(),
    headers: (&inbound).into(),
    raw_body: decoded.raw_body.clone(),
    decoded_body: decoded.decoded_body.clone(),
    body_json: decoded.value.clone(),
    request_id: Some(SmolStr::new(&hx.request_id)),
  };
  match state.request_pipeline.run(raw).await {
    Ok(converted) => Ok(super::response::converted_to_axum(converted)),
    Err(err) => Err(pipeline_error_to_api_error(err)),
  }
}

fn pipeline_error_to_api_error(err: llm_requests::PipelineError) -> ApiError {
  match err.inner() {
    RequestsError::Resolve { source: ResolveError::InvalidRouteMode { .. } }
    | RequestsError::Resolve { source: ResolveError::InvalidExactModel { .. } } => {
      ApiError::bad_request(err.message().to_owned())
    }
    RequestsError::SessionExpired { session_id } => ApiError::session_expired(session_id.to_string()),
    RequestsError::NoAccount { endpoint, model } => {
      ApiError::not_implemented(endpoint.to_string(), model.to_string())
    }
    RequestsError::UpstreamStatus { status, body } => match StatusCode::from_u16(*status) {
      Ok(status) => ApiError::upstream(status, body.clone()),
      Err(_) => ApiError::bad_gateway(body.clone()),
    },
    _ => ApiError::bad_gateway(err.message().into_owned()),
  }
}

fn request_record_mode(mode: Option<llm_config::RouteMode>) -> &'static str {
  match mode {
    Some(mode) => route_mode_as_str(mode),
    None => "route",
  }
}

/// Inject route mode from path prefix into headers, overriding any existing value.
fn inject_mode(mode: &str, headers: &mut HeaderMap) -> Result<(), ApiError> {
  super::validate_path_mode(mode)?;
  headers.insert(
    axum::http::HeaderName::from_static("x-route-mode"),
    axum::http::HeaderValue::from_str(mode).unwrap(),
  );
  Ok(())
}

#[instrument(
  name = "chat_completions",
  skip_all,
  fields(
    endpoint = %crate::provider::Endpoint::ChatCompletions,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn chat_completions(
  State(state): State<AppState>,
  inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  handle(state, &ChatParser, inbound, body).await
}

#[instrument(
  name = "responses",
  skip_all,
  fields(
    endpoint = %crate::provider::Endpoint::Responses,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn responses(State(state): State<AppState>, inbound: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
  handle(state, &ResponsesParser, inbound, body).await
}

#[instrument(
  name = "messages",
  skip_all,
  fields(
    endpoint = %crate::provider::Endpoint::Messages,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn messages(State(state): State<AppState>, inbound: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
  handle(state, &MessagesParser, inbound, body).await
}

// --- Mode-prefixed variants ---

pub async fn chat_completions_with_mode(
  State(state): State<AppState>,
  Path(mode): Path<String>,
  mut inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  inject_mode(&mode, &mut inbound)?;
  handle(state, &ChatParser, inbound, body).await
}

pub async fn responses_with_mode(
  State(state): State<AppState>,
  Path(mode): Path<String>,
  mut inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  inject_mode(&mode, &mut inbound)?;
  handle(state, &ResponsesParser, inbound, body).await
}

pub async fn messages_with_mode(
  State(state): State<AppState>,
  Path(mode): Path<String>,
  mut inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  inject_mode(&mode, &mut inbound)?;
  handle(state, &MessagesParser, inbound, body).await
}
