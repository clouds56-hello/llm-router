use crate::provider::Endpoint;
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

}
