use super::recording::record_call;
use super::usage::parse_usage_any_value;
use crate::db::OutboundSnapshot;
use crate::pool::AccountHandle;
use crate::provider::Endpoint;
use crate::server::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_response(
  s: AppState,
  acct: Arc<AccountHandle>,
  resp: reqwest::Response,
  endpoint: Endpoint,
  upstream_endpoint: Endpoint,
  model: String,
  initiator: String,
  session_id: Option<String>,
  request_id: Option<String>,
  project_id: Option<String>,
  req_headers: HeaderMap,
  req_body: Value,
  outbound: Option<OutboundSnapshot>,
  started: Instant,
) -> Response {
  let status = resp.status();
  let resp_headers = resp.headers().clone();

  let usage_holder = Arc::new(parking_lot::Mutex::new((None::<u64>, None::<u64>)));
  let usage_for_stream = usage_holder.clone();
  let resp_body_holder = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
  let resp_body_for_stream = resp_body_holder.clone();
  let max_body = s.db.as_ref().map(|db| db.body_max_bytes()).unwrap_or(0);
  let acct_id = acct.id();
  let provider_id = acct.provider.info().id.clone();
  let model_clone = model.clone();
  let initiator_clone = initiator.clone();
  let session_id_clone = session_id.clone();
  let request_id_clone = request_id.clone();
  let project_id_clone = project_id.clone();
  let req_headers_clone = req_headers.clone();
  let req_body_clone = req_body.clone();
  let s_clone = s.clone();

  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("text/event-stream"),
  );
  if let Some(id) = session_id.as_deref() {
    if let Ok(value) = HeaderValue::from_str(id) {
      headers.insert(crate::server::SESSION_ID_HEADER, value);
    }
  }
  headers.insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  headers.insert(axum::http::header::CONNECTION, HeaderValue::from_static("keep-alive"));
  let inbound_resp_headers = headers.clone();

  let upstream: Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>> = if upstream_endpoint == endpoint {
    Box::pin(resp.bytes_stream().map(|r| r.map_err(std::io::Error::other)))
  } else {
    crate::convert::sse::translate_stream(upstream_endpoint, endpoint, resp)
  };
  let mut buffer = Vec::<u8>::new();

  let mapped = upstream.map(move |chunk| match chunk {
    Ok(b) => {
      if max_body > 0 {
        let mut captured = resp_body_for_stream.lock();
        let remaining = max_body.saturating_sub(captured.len());
        if remaining > 0 {
          captured.extend_from_slice(&b[..b.len().min(remaining)]);
        }
      }
      buffer.extend_from_slice(&b);
      while let Some(pos) = buffer.iter().position(|&c| c == b'\n') {
        let line: Vec<u8> = buffer.drain(..=pos).collect();
        let s = String::from_utf8_lossy(&line);
        let trimmed = s.trim_start();
        let Some(rest) = trimmed.strip_prefix("data:") else {
          continue;
        };
        let payload = rest.trim();
        if payload.is_empty() || payload == "[DONE]" {
          continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
          continue;
        };
        let (pt, ct) = parse_usage_any_value(&v);
        if pt.is_some() || ct.is_some() {
          let mut g = usage_for_stream.lock();
          if pt.is_some() {
            g.0 = pt;
          }
          if ct.is_some() {
            g.1 = ct;
          }
        }
      }
      Ok::<Bytes, std::io::Error>(b)
    }
    Err(e) => Err(std::io::Error::other(e)),
  });

  let recorded = Arc::new(parking_lot::Mutex::new(false));
  let recorded_clone = recorded.clone();
  let outbound_for_end = parking_lot::Mutex::new(outbound);
  let on_end = move || {
    if *recorded_clone.lock() {
      return;
    }
    *recorded_clone.lock() = true;
    let (pt, ct) = *usage_holder.lock();
    let captured = bytes::Bytes::from(resp_body_holder.lock().clone());
    let outbound_taken = outbound_for_end.lock().take();
    record_call(
      &s_clone,
      &acct_id,
      &provider_id,
      endpoint,
      &model_clone,
      &initiator_clone,
      session_id_clone.as_deref(),
      request_id_clone.as_deref(),
      project_id_clone.as_deref(),
      &req_headers_clone,
      &req_body_clone,
      Some(&resp_headers),
      Some(&captured),
      &inbound_resp_headers,
      outbound_taken,
      pt,
      ct,
      started,
      status.as_u16(),
      true,
    );
  };

  let stream = StreamWithFinalizer::new(mapped, on_end);
  let body = Body::from_stream(stream);

  (status, headers, body).into_response()
}

struct StreamWithFinalizer<S, F: FnOnce() + Send + 'static> {
  inner: S,
  fin: Option<F>,
}

impl<S, F: FnOnce() + Send + 'static> StreamWithFinalizer<S, F> {
  fn new(inner: S, f: F) -> Self {
    Self { inner, fin: Some(f) }
  }
}

impl<S, F> Stream for StreamWithFinalizer<S, F>
where
  S: Stream + Unpin,
  F: FnOnce() + Send + 'static + Unpin,
{
  type Item = S::Item;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    let p = Pin::new(&mut self.inner).poll_next(cx);
    if let Poll::Ready(None) = &p {
      if let Some(f) = self.fin.take() {
        f();
      }
    }
    p
  }
}

impl<S, F: FnOnce() + Send + 'static> Drop for StreamWithFinalizer<S, F> {
  fn drop(&mut self) {
    if let Some(f) = self.fin.take() {
      f();
    }
  }
}
