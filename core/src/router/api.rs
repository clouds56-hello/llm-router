use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::app_state::AppState;
use crate::db::logging::LogQuery;

use super::helpers::json_error;

pub(super) async fn health() -> Json<Value> {
  Json(json!({ "ok": true }))
}

pub(super) async fn provider_status(State(state): State<Arc<AppState>>) -> Json<Value> {
  let loaded = state.config().get();
  Json(json!({ "providers": state.providers().provider_status(&loaded) }))
}

pub(super) async fn model_list(State(state): State<Arc<AppState>>) -> Json<Value> {
  let loaded = state.config().get();
  let models: Vec<Value> = loaded
    .models
    .models
    .iter()
    .map(|m| {
      json!({
          "name": m.openai_name,
          "provider": m.provider,
          "provider_model": m.provider_model,
          "is_default": m.is_default,
          "enabled": loaded.is_model_enabled(&m.openai_name),
      })
    })
    .collect();

  Json(json!({ "models": models }))
}

pub(super) async fn active_config(State(state): State<Arc<AppState>>) -> Json<Value> {
  let loaded = state.config().get();
  Json(json!({
      "providers": loaded.providers,
      "models": loaded.models,
      "credentials": loaded.credentials,
      "last_error": state.config().last_error(),
  }))
}

#[derive(Debug, Deserialize)]
pub(super) struct RequestLogsQuery {
  limit: Option<usize>,
  level: Option<String>,
  request_id: Option<String>,
}

pub(super) async fn request_logs(
  State(state): State<Arc<AppState>>,
  Query(query): Query<RequestLogsQuery>,
) -> Response {
  let filter = LogQuery {
    limit: query.limit,
    level: query.level,
    request_id: query.request_id,
  };
  match state.logs().query(filter) {
    Ok(logs) => Json(json!({ "logs": logs })).into_response(),
    Err(err) => json_error(
      StatusCode::INTERNAL_SERVER_ERROR,
      &format!("failed to query logs: {err}"),
    ),
  }
}
