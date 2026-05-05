use super::error::ApiError;
use super::pipeline::{handle_endpoint, MessagesParser};
use super::AppState;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use serde_json::Value;
use tracing::instrument;

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
  State(s): State<AppState>,
  inbound: HeaderMap,
  Json(body): Json<Value>,
) -> Result<Response, ApiError> {
  handle_endpoint(s, &MessagesParser, inbound, body).await
}
