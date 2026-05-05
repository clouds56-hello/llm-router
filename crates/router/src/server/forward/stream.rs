use super::recording::CallRecordBuilder;
use super::usage::parse_usage_any_value;
use crate::db::{HttpSnapshot, OutboundSnapshot};
use crate::pool::AccountHandle;
use crate::provider::Endpoint;
use crate::server::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use llm_core::pipeline::RequestReporter;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

struct StreamAccumulator {
  max_body: usize,
  usage: Arc<parking_lot::Mutex<(Option<u64>, Option<u64>)>>,
  captured_body: Arc<parking_lot::Mutex<Vec<u8>>>,
  failed: Arc<parking_lot::Mutex<bool>>,
  buffer: Vec<u8>,
}

impl StreamAccumulator {
  fn new(
    max_body: usize,
    usage: Arc<parking_lot::Mutex<(Option<u64>, Option<u64>)>>,
    captured_body: Arc<parking_lot::Mutex<Vec<u8>>>,
    failed: Arc<parking_lot::Mutex<bool>>,
  ) -> Self {
    Self {
      max_body,
      usage,
      captured_body,
      failed,
      buffer: Vec::new(),
    }
  }

  fn on_chunk(&mut self, chunk: &Bytes) {
    self.capture_body(chunk);
    self.buffer.extend_from_slice(chunk);
    while let Some(pos) = self.buffer.iter().position(|&c| c == b'\n') {
      let line: Vec<u8> = self.buffer.drain(..=pos).collect();
      self.capture_usage_line(&line);
    }
  }

  fn on_error(&self) {
    *self.failed.lock() = true;
  }

  fn capture_body(&self, chunk: &Bytes) {
    if self.max_body == 0 {
      return;
    }
    let mut captured = self.captured_body.lock();
    let remaining = self.max_body.saturating_sub(captured.len());
    if remaining > 0 {
      captured.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
    }
  }

  fn capture_usage_line(&self, line: &[u8]) {
    let s = String::from_utf8_lossy(line);
    let trimmed = s.trim_start();
    let Some(rest) = trimmed.strip_prefix("data:") else {
      return;
    };
    let payload = rest.trim();
    if payload.is_empty() || payload == "[DONE]" {
      return;
    }
    let Ok(v) = serde_json::from_str::<Value>(payload) else {
      return;
    };
    let (pt, ct) = parse_usage_any_value(&v);
    if pt.is_some() || ct.is_some() {
      let mut usage = self.usage.lock();
      if pt.is_some() {
        usage.0 = pt;
      }
      if ct.is_some() {
        usage.1 = ct;
      }
    }
  }
}

struct StreamRecordContext {
  max_body: usize,
  acct_id: String,
  provider_id: String,
  endpoint: Endpoint,
  model: String,
  initiator: String,
  session_id: Option<String>,
  request_id: Option<String>,
  project_id: Option<String>,
  req_headers: HeaderMap,
  req_body: Value,
  resp_headers: HeaderMap,
  inbound_resp_headers: HeaderMap,
  outbound: parking_lot::Mutex<Option<OutboundSnapshot>>,
  started: Instant,
  status: u16,
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_response(
  s: AppState,
  reporter: Arc<dyn RequestReporter>,
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
  let resp_body_holder = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
  let max_body = s.db.as_ref().map(|db| db.body_max_bytes()).unwrap_or(0);
  let headers = sse_headers(session_id.as_deref());
  let inbound_resp_headers = headers.clone();
  let stream_failed = Arc::new(parking_lot::Mutex::new(false));
  let mut accumulator = StreamAccumulator::new(
    max_body,
    usage_holder.clone(),
    resp_body_holder.clone(),
    stream_failed.clone(),
  );
  let mapped = upstream_stream(resp, upstream_endpoint, endpoint).map(move |chunk| match chunk {
    Ok(b) => {
      accumulator.on_chunk(&b);
      Ok::<Bytes, std::io::Error>(b)
    }
    Err(e) => {
      accumulator.on_error();
      Err(std::io::Error::other(e))
    }
  });

  let recorded = Arc::new(parking_lot::Mutex::new(false));
  let recorded_clone = recorded.clone();
  let reporter_clone = reporter.clone();
  let record_ctx = StreamRecordContext {
    max_body,
    acct_id: acct.id(),
    provider_id: acct.provider.info().id.clone(),
    endpoint,
    model,
    initiator,
    session_id,
    request_id,
    project_id,
    req_headers,
    req_body,
    resp_headers,
    inbound_resp_headers,
    outbound: parking_lot::Mutex::new(outbound),
    started,
    status: status.as_u16(),
  };
  let on_end = move || {
    if *recorded_clone.lock() {
      return;
    }
    *recorded_clone.lock() = true;
    report_stream_completion(
      &reporter_clone,
      record_ctx,
      &usage_holder,
      &resp_body_holder,
      &stream_failed,
    );
  };

  let stream = StreamWithFinalizer::new(mapped, on_end);
  let body = Body::from_stream(stream);

  (status, headers, body).into_response()
}

fn sse_headers(session_id: Option<&str>) -> HeaderMap {
  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("text/event-stream"),
  );
  if let Some(id) = session_id {
    if let Ok(value) = HeaderValue::from_str(id) {
      headers.insert(crate::server::SESSION_ID_HEADER, value);
    }
  }
  headers.insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  headers.insert(axum::http::header::CONNECTION, HeaderValue::from_static("keep-alive"));
  headers
}

fn upstream_stream(resp: reqwest::Response, upstream_endpoint: Endpoint, endpoint: Endpoint) -> ByteStream {
  if upstream_endpoint == endpoint {
    Box::pin(resp.bytes_stream().map(|r| r.map_err(std::io::Error::other)))
  } else {
    crate::convert::sse::translate_stream(upstream_endpoint, endpoint, resp)
  }
}

fn report_stream_completion(
  reporter: &Arc<dyn RequestReporter>,
  ctx: StreamRecordContext,
  usage_holder: &Arc<parking_lot::Mutex<(Option<u64>, Option<u64>)>>,
  resp_body_holder: &Arc<parking_lot::Mutex<Vec<u8>>>,
  stream_failed: &Arc<parking_lot::Mutex<bool>>,
) {
  let (pt, ct) = *usage_holder.lock();
  let captured = bytes::Bytes::from(resp_body_holder.lock().clone());
  let request_error = if *stream_failed.lock() {
    Some("stream terminated before completion")
  } else {
    None
  };
  let record = CallRecordBuilder::for_endpoint(
    ctx.max_body,
    &ctx.acct_id,
    &ctx.provider_id,
    ctx.endpoint,
    &ctx.model,
    &ctx.initiator,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(ctx.status),
      headers: ctx.inbound_resp_headers,
      body: captured.clone(),
    },
    ctx.started,
    ctx.status,
    true,
  )
  .with_ids(
    ctx.session_id.as_deref(),
    ctx.request_id.as_deref(),
    request_error,
    ctx.project_id.as_deref(),
  )
  .with_request_json(&ctx.req_headers, &ctx.req_body)
  .with_outbound_request(ctx.outbound.into_inner())
  .with_outbound_response(Some(&ctx.resp_headers), Some(&captured))
  .with_usage(pt, ct)
  .build();
  reporter.report(record);
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
