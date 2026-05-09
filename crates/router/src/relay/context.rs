use crate::provider::Endpoint;
use crate::relay::passthrough::passthrough_endpoint;
use axum::http::Method;
use serde_json::Value;
use std::time::Instant;

/// Bundled request metadata needed by forward/response functions.
/// Does not include the request body — that is passed separately.
pub(crate) struct ForwardContext {
  /// Base request ID (no retry suffix).
  pub request_id: String,
  /// Retry attempt number (0 = first attempt).
  pub attempt: u32,
  pub session_id: Option<String>,
  pub endpoint: Option<Endpoint>,
  pub upstream_endpoint: Endpoint,
  pub downstream_headers: reqwest::header::HeaderMap,
  pub model: String,
  pub started: Instant,
}

impl ForwardContext {
  /// Build a ForwardContext from pipeline metadata (routed requests).
  pub fn from_pipeline(
    endpoint: Endpoint,
    upstream_endpoint: Endpoint,
    model: String,
    session_id: Option<String>,
    request_id: String,
    attempt: u32,
    started: Instant,
  ) -> Self {
    Self {
      request_id,
      attempt,
      session_id,
      endpoint: Some(endpoint),
      upstream_endpoint,
      downstream_headers: reqwest::header::HeaderMap::new(),
      model,
      started,
    }
  }

  /// Build a ForwardContext from passthrough request data.
  pub fn from_passthrough(
    method: &Method,
    path: &str,
    req_headers: &reqwest::header::HeaderMap,
    req_body: &Value,
    started: Instant,
  ) -> Self {
    let endpoint = passthrough_endpoint(method, path);
    let model = req_body
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("unknown")
      .to_string();
    let request_id = crate::api::first_header(req_headers, crate::api::REQUEST_ID_HEADERS)
      .map(|s| s.to_string())
      .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let session_id = crate::api::first_header(req_headers, crate::api::SESSION_ID_HEADERS).map(|s| s.to_string());
    // For passthrough, upstream_endpoint == endpoint (no translation)
    let upstream_endpoint = endpoint.unwrap_or(Endpoint::ChatCompletions);

    Self {
      request_id,
      attempt: 0,
      session_id,
      endpoint,
      upstream_endpoint,
      downstream_headers: req_headers.clone(),
      model,
      started,
    }
  }
}
