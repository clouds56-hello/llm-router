pub mod chat;
pub mod dispatch;
pub mod error;
pub mod forward;
pub mod messages;
pub mod models;
pub mod responses;

use crate::config::Config;
use crate::pool::AccountPool;
use crate::usage::UsageDb;
use anyhow::Result;
use axum::http::HeaderName;
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
  pub pool: Arc<AccountPool>,
  pub http: reqwest::Client,
  pub usage: Option<Arc<UsageDb>>,
  pub usage_enabled: bool,
}

/// Header name used for request ids. Honors inbound `x-request-id` if present.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

pub fn router(state: AppState) -> Router {
  let request_id_header = HeaderName::from_static(REQUEST_ID_HEADER);
  Router::new()
    .route("/v1/models", get(models::list_models))
    .route("/v1/chat/completions", post(chat::chat_completions))
    .route("/v1/responses", post(responses::responses))
    .route("/v1/messages", post(messages::messages))
    .route("/healthz", get(health))
    .with_state(state)
    // Layers run outermost-first on request, innermost-first on response.
    // SetRequestIdLayer with MakeRequestUuid only assigns a fresh UUID when
    // the inbound request lacks the header, so client-supplied ids pass
    // through unchanged. PropagateRequestIdLayer copies it onto the
    // response so clients can correlate.
    .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
    .layer(TraceLayer::new_for_http())
    .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
}

async fn health() -> &'static str {
  "ok"
}

pub fn build_state(cfg: &Config) -> Result<AppState> {
  let pool = AccountPool::from_config(cfg)?;
  let http = crate::util::http::build_client(&cfg.proxy)?;
  let usage = if cfg.usage.enabled {
    let path = cfg
      .usage
      .db_path
      .clone()
      .map(Ok)
      .unwrap_or_else(crate::config::paths::default_usage_db)?;
    Some(Arc::new(UsageDb::open(&path)?))
  } else {
    None
  };
  Ok(AppState {
    pool,
    http,
    usage,
    usage_enabled: cfg.usage.enabled,
  })
}
