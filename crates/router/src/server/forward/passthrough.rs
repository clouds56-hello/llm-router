use super::recording::CallRecordBuilder;
use super::usage::{parse_usage_any_json, parse_usage_any_value};
use crate::db::HttpSnapshot;
use crate::provider::Endpoint;
use crate::server::AppState;
use crate::util::initiator::{classify_initiator, classify_initiator_responses};
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, std::io::Error>> + Send>>;

pub(crate) fn is_sse_response(headers: &HeaderMap) -> bool {
  headers
    .get(CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.split(';').next())
    .map(str::trim)
    .is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_passthrough_call(
  s: &AppState,
  host: &str,
  method: &Method,
  path_and_query: &str,
  req_headers: &reqwest::header::HeaderMap,
  req_body: &Bytes,
  outbound_req_headers: &reqwest::header::HeaderMap,
  resp_headers: &reqwest::header::HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
) {
  let Some(db) = s.db.as_ref() else { return };
  let (prompt_tokens, completion_tokens) = parse_usage_any_json(resp_body);
  let record = passthrough_record_builder(
    db.body_max_bytes(),
    host,
    method,
    path_and_query,
    req_headers,
    req_body,
    outbound_req_headers,
    resp_headers,
    resp_body,
    status,
    started,
    None,
  )
  .with_usage(prompt_tokens, completion_tokens)
  .build();
  db.record(record);
}

#[allow(clippy::too_many_arguments)]
fn record_passthrough_stream(
  s: &AppState,
  host: &str,
  method: &Method,
  path_and_query: &str,
  req_headers: &reqwest::header::HeaderMap,
  req_body: &Bytes,
  outbound_req_headers: &reqwest::header::HeaderMap,
  resp_headers: &reqwest::header::HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
  usage: (Option<u64>, Option<u64>),
  request_error: Option<&str>,
) {
  let Some(db) = s.db.as_ref() else { return };
  let record = passthrough_record_builder(
    db.body_max_bytes(),
    host,
    method,
    path_and_query,
    req_headers,
    req_body,
    outbound_req_headers,
    resp_headers,
    resp_body,
    status,
    started,
    request_error,
  )
  .with_usage(usage.0, usage.1)
  .build();
  db.record(record);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn passthrough_streaming_response(
  state: AppState,
  host: String,
  method: Method,
  path_and_query: String,
  req_headers: HeaderMap,
  req_body: Bytes,
  outbound_req_headers: HeaderMap,
  resp: reqwest::Response,
  started: Instant,
) -> Response {
  let status = resp.status();
  let headers = resp.headers().clone();
  let record_headers = headers.clone();
  let usage_holder = Arc::new(parking_lot::Mutex::new((None::<u64>, None::<u64>)));
  let resp_body_holder = Arc::new(parking_lot::Mutex::new(Vec::<u8>::new()));
  let max_body = state.db.as_ref().map(|db| db.body_max_bytes()).unwrap_or(0);
  let stream_failed = Arc::new(parking_lot::Mutex::new(false));
  let mut accumulator = PassthroughStreamAccumulator::new(
    max_body,
    usage_holder.clone(),
    resp_body_holder.clone(),
    stream_failed.clone(),
  );
  let mapped = passthrough_upstream_stream(resp).map(move |chunk| match chunk {
    Ok(bytes) => {
      accumulator.on_chunk(&bytes);
      Ok::<Bytes, std::io::Error>(bytes)
    }
    Err(err) => {
      accumulator.on_error();
      Err(std::io::Error::other(err))
    }
  });

  let recorded = Arc::new(parking_lot::Mutex::new(false));
  let recorded_clone = recorded.clone();
  let on_end = move || {
    if *recorded_clone.lock() {
      return;
    }
    *recorded_clone.lock() = true;
    let request_error = if *stream_failed.lock() {
      Some("stream terminated before completion")
    } else {
      None
    };
    let usage = *usage_holder.lock();
    let captured = Bytes::from(resp_body_holder.lock().clone());
    record_passthrough_stream(
      &state,
      &host,
      &method,
      &path_and_query,
      &req_headers,
      &req_body,
      &outbound_req_headers,
      &record_headers,
      &captured,
      status.as_u16(),
      started,
      usage,
      request_error,
    );
  };

  response_with_body(
    status,
    &headers,
    Body::from_stream(StreamWithFinalizer::new(mapped, on_end)),
  )
}

#[allow(clippy::too_many_arguments)]
fn passthrough_record_builder(
  max_body: usize,
  host: &str,
  method: &Method,
  path_and_query: &str,
  req_headers: &reqwest::header::HeaderMap,
  req_body: &Bytes,
  outbound_req_headers: &reqwest::header::HeaderMap,
  resp_headers: &reqwest::header::HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
  request_error: Option<&str>,
) -> CallRecordBuilder {
  let path = path_and_query.split('?').next().unwrap_or(path_and_query);
  let logical_path = crate::proxy::rewrite_target(host, path, method).unwrap_or(path);
  let endpoint = passthrough_endpoint(method, logical_path);
  let req_body_json = serde_json::from_slice::<Value>(req_body).unwrap_or(Value::Null);
  let model = req_body_json
    .get("model")
    .and_then(|v| v.as_str())
    .unwrap_or("unknown")
    .to_string();
  let stream = req_body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  let initiator = classify_passthrough_initiator(req_headers, endpoint, &req_body_json).to_string();

  CallRecordBuilder::for_path(
    max_body,
    "passthrough",
    host,
    endpoint.map(Endpoint::as_str).unwrap_or(logical_path),
    endpoint,
    &model,
    &initiator,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: resp_headers.clone(),
      body: resp_body.clone(),
    },
    started,
    status,
    stream,
  )
  .with_ids(
    crate::server::first_header(req_headers, crate::server::SESSION_ID_HEADERS),
    crate::server::first_header(req_headers, crate::server::REQUEST_ID_HEADERS),
    request_error,
    crate::server::first_header(req_headers, crate::server::PROJECT_ID_HEADERS),
  )
  .with_request_snapshot(
    req_body.clone(),
    Some(HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: req_headers.clone(),
      body: req_body.clone(),
    }),
  )
  .with_outbound_request(Some(HttpSnapshot {
    method: Some(method.to_string()),
    url: Some(format!("https://{host}{path_and_query}")),
    status: None,
    headers: outbound_req_headers.clone(),
    body: req_body.clone(),
  }))
  .with_response_snapshot(Some(HttpSnapshot {
    method: None,
    url: None,
    status: Some(status),
    headers: resp_headers.clone(),
    body: resp_body.clone(),
  }))
}

fn passthrough_upstream_stream(resp: reqwest::Response) -> ByteStream {
  Box::pin(resp.bytes_stream().map(|result| result.map_err(std::io::Error::other)))
}

fn response_with_body(status: reqwest::StatusCode, headers: &HeaderMap, body: Body) -> Response {
  let mut builder = Response::builder().status(status);
  for (name, value) in headers {
    builder = builder.header(name, value);
  }
  builder.body(body).unwrap_or_else(|_| Response::new(Body::empty()))
}

struct PassthroughStreamAccumulator {
  max_body: usize,
  usage: Arc<parking_lot::Mutex<(Option<u64>, Option<u64>)>>,
  captured_body: Arc<parking_lot::Mutex<Vec<u8>>>,
  failed: Arc<parking_lot::Mutex<bool>>,
  buffer: Vec<u8>,
}

impl PassthroughStreamAccumulator {
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
    let Ok(value) = serde_json::from_str::<Value>(payload) else {
      return;
    };
    let (prompt_tokens, completion_tokens) = parse_usage_any_value(&value);
    if prompt_tokens.is_some() || completion_tokens.is_some() {
      let mut usage = self.usage.lock();
      if prompt_tokens.is_some() {
        usage.0 = prompt_tokens;
      }
      if completion_tokens.is_some() {
        usage.1 = completion_tokens;
      }
    }
  }
}

struct StreamWithFinalizer<S, F: FnOnce() + Send + 'static> {
  inner: S,
  fin: Option<F>,
}

impl<S, F: FnOnce() + Send + 'static> StreamWithFinalizer<S, F> {
  fn new(inner: S, fin: F) -> Self {
    Self { inner, fin: Some(fin) }
  }
}

impl<S, F> Stream for StreamWithFinalizer<S, F>
where
  S: Stream + Unpin,
  F: FnOnce() + Send + 'static + Unpin,
{
  type Item = S::Item;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    let poll = Pin::new(&mut self.inner).poll_next(cx);
    if let Poll::Ready(None) = &poll {
      if let Some(fin) = self.fin.take() {
        fin();
      }
    }
    poll
  }
}

impl<S, F: FnOnce() + Send + 'static> Drop for StreamWithFinalizer<S, F> {
  fn drop(&mut self) {
    if let Some(fin) = self.fin.take() {
      fin();
    }
  }
}

fn passthrough_endpoint(method: &Method, path: &str) -> Option<Endpoint> {
  match (method, path) {
    (&Method::POST, "/v1/chat/completions") => Some(Endpoint::ChatCompletions),
    (&Method::POST, "/v1/responses") => Some(Endpoint::Responses),
    (&Method::POST, "/v1/messages") => Some(Endpoint::Messages),
    _ => None,
  }
}

fn classify_passthrough_initiator(
  headers: &reqwest::header::HeaderMap,
  endpoint: Option<Endpoint>,
  body: &Value,
) -> &'static str {
  if let Some(value) = headers.get("x-initiator").and_then(|v| v.to_str().ok()) {
    match value.trim().to_ascii_lowercase().as_str() {
      "user" => return "user",
      "agent" => return "agent",
      _ => {}
    }
  }
  match endpoint {
    Some(Endpoint::Responses) => classify_initiator_responses(body),
    Some(Endpoint::ChatCompletions | Endpoint::Messages) => classify_initiator(body),
    None => "user",
  }
}
