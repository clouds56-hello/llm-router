use super::error::ApiError;
use super::pipeline::{handle_endpoint, ChatParser, MessagesParser, RequestParser, ResponsesParser};
use super::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use serde_json::Value;
use tracing::instrument;

async fn handle(
  state: AppState,
  parser: &dyn RequestParser,
  inbound: HeaderMap,
  body: Value,
) -> Result<Response, ApiError> {
  handle_endpoint(state, parser, inbound, body).await
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
  Json(body): Json<Value>,
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
pub async fn responses(
  State(state): State<AppState>,
  inbound: HeaderMap,
  Json(body): Json<Value>,
) -> Result<Response, ApiError> {
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
pub async fn messages(
  State(state): State<AppState>,
  inbound: HeaderMap,
  Json(body): Json<Value>,
) -> Result<Response, ApiError> {
  handle(state, &MessagesParser, inbound, body).await
}
