use super::error::ApiError;
use super::AppState;
use crate::pipeline::{handle_endpoint, ChatParser, MessagesParser, RequestParser, ResponsesParser};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use tracing::instrument;

async fn handle(
  state: AppState,
  parser: &dyn RequestParser,
  inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  let decoded = super::codec::decode_json_request(&inbound, body)?;
  handle_endpoint(state, parser, inbound, decoded).await
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
