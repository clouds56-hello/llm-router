//! Adapter helpers for materializing a [`tokn_requests::ConvertedResponse`]
//! into an axum `Response` for the router's default `tokn-requests` path.
//!
//! The router rebuilds response headers instead of forwarding upstream
//! headers verbatim. A fresh header map is built containing only what the
//! client needs (content-type for
//! map is built containing only what the client needs (content-type for
//! buffered JSON, SSE headers for streams). This avoids leaking
//! `content-encoding: gzip` from the upstream while reqwest has already
//! decoded the body — which would otherwise make clients try to gunzip
//! plain text and fail.

use axum::body::Body;
use axum::http::{header, HeaderMap, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use tokn_requests::pipeline::stages::{ConvertedBody, ConvertedResponse};

/// Convert a [`ConvertedResponse`] into an axum response.
///
/// Status code is preserved from upstream. Headers are rebuilt from
/// scratch (see module-level docs for rationale). Body comes through
/// either as a single `Bytes` (Buffered) or a `BoxStream` (Stream).
pub(crate) fn converted_to_axum(c: ConvertedResponse) -> Response<Body> {
  let status = to_status(c.status);
  match c.body {
    ConvertedBody::Buffered { body_bytes, .. } => {
      let headers = buffered_headers();
      (status, headers, Body::from(body_bytes)).into_response()
    }
    ConvertedBody::Stream { body } => {
      let headers = sse_headers();
      (status, headers, Body::from_stream(body)).into_response()
    }
  }
}

fn to_status(status: u16) -> StatusCode {
  StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn buffered_headers() -> HeaderMap {
  let mut h = HeaderMap::new();
  h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
  h
}

fn sse_headers() -> HeaderMap {
  let mut h = HeaderMap::new();
  h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
  h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  h.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
  h
}
