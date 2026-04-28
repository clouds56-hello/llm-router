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
use axum::http::{HeaderName, Request, Response};
use axum::routing::{get, post};
use axum::Router;
use std::sync::Arc;
use std::time::Duration;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{Level, Span};

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

  // TraceLayer is customised so the per-request span carries `request_id`
  // (set by SetRequestIdLayer below) and emits a single info-level summary
  // line at the response edge with status + latency. Per-step debug events
  // come from the handlers themselves and inherit this span.
  let trace = TraceLayer::new_for_http()
    .make_span_with(|req: &Request<_>| {
      let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
      tracing::info_span!(
        "http",
        method = %req.method(),
        uri = %req.uri(),
        request_id = %request_id,
        status = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
      )
    })
    .on_request(|req: &Request<_>, _span: &Span| {
      let len = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
      tracing::debug!(content_length = %len, "request started");
    })
    .on_response(|resp: &Response<_>, latency: Duration, span: &Span| {
      let status = resp.status();
      let ms = latency.as_millis() as u64;
      span.record("status", status.as_u16());
      span.record("latency_ms", ms);
      if status.is_server_error() || status.is_client_error() {
        tracing::event!(Level::WARN, status = %status, latency_ms = ms, "request finished with error");
      } else {
        tracing::event!(Level::INFO, status = %status, latency_ms = ms, "request finished");
      }
    })
    .on_failure(
      |err: tower_http::classify::ServerErrorsFailureClass, latency: Duration, _span: &Span| {
        tracing::warn!(error = %err, latency_ms = latency.as_millis() as u64, "request failed");
      },
    );

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
    .layer(trace)
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

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::Body;
  use axum::http::{Request, StatusCode};
  use axum::routing::get;
  use tower::ServiceExt;

  /// Build the same layer stack the real router uses, around a stub handler.
  /// This isolates the request-id middleware from `AppState` construction.
  fn test_router() -> Router {
    let header = HeaderName::from_static(REQUEST_ID_HEADER);
    Router::new()
      .route("/probe", get(|| async { "ok" }))
      .layer(PropagateRequestIdLayer::new(header.clone()))
      .layer(SetRequestIdLayer::new(header, MakeRequestUuid))
  }

  #[tokio::test]
  async fn inbound_request_id_passes_through() {
    let app = test_router();
    let req = Request::builder()
      .uri("/probe")
      .header(REQUEST_ID_HEADER, "client-supplied-123")
      .body(Body::empty())
      .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let echoed = resp
      .headers()
      .get(REQUEST_ID_HEADER)
      .expect("response missing x-request-id")
      .to_str()
      .unwrap();
    assert_eq!(echoed, "client-supplied-123");
  }

  #[tokio::test]
  async fn missing_request_id_is_generated() {
    let app = test_router();
    let req = Request::builder().uri("/probe").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let id = resp
      .headers()
      .get(REQUEST_ID_HEADER)
      .expect("response missing generated x-request-id")
      .to_str()
      .unwrap();
    // MakeRequestUuid emits a hyphenated uuid v4.
    assert!(uuid::Uuid::parse_str(id).is_ok(), "not a uuid: {id}");
  }
}
