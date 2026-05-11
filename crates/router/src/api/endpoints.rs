use super::error::ApiError;
use super::AppState;
use crate::pipeline::{
  handle_endpoint, request_header_extract, ChatParser, MessagesParser, RequestParser, ResponsesParser,
};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use std::time::Instant;
use tracing::instrument;

async fn handle(
  state: AppState,
  parser: &dyn RequestParser,
  inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  let started = Instant::now();
  let ts = unix_ts();
  let hx = request_header_extract(&inbound);
  let endpoint_hint = parser.endpoint().as_str().to_string();

  state.events.emit(llm_core::event::Event::RequestStarted {
    request_id: hx.request_id.clone(),
    ts,
    endpoint: endpoint_hint.clone(),
    session_id: hx.session_id.clone(),
    ip: None,
    port: None,
    method: "POST".to_string(),
    url: None,
  });
  state.events.emit(llm_core::event::Event::RequestHeaders {
    request_id: hx.request_id.clone(),
    ts,
    endpoint_hint: Some(endpoint_hint),
    path: None,
    session_id: hx.session_id.clone(),
    project_id: hx.project_id.clone(),
    header_initiator: hx.header_initiator.clone(),
    route_mode_hint: hx.route_mode_hint.clone(),
    inbound_headers: inbound.clone(),
  });

  let decoded = match super::codec::decode_json_request(&inbound, body) {
    Ok(decoded) => decoded,
    Err(err) => {
      state.events.emit(llm_core::event::Event::RequestCompleted {
        request_id: hx.request_id.clone(),
        success: false,
        total_attempts: 1,
        final_status: Some(err.status().as_u16()),
        total_latency_ms: started.elapsed().as_millis() as u64,
        error: Some(err.to_string()),
      });
      return Err(err);
    }
  };
  let parsed = parser.parse(inbound, decoded.value.clone());
  handle_endpoint(state, parsed, decoded, hx.request_id, started).await
}

fn unix_ts() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs() as i64
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
