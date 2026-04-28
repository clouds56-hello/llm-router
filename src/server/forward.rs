//! Format-agnostic upstream-response forwarders.
//!
//! Both buffered and streaming variants forward upstream bytes verbatim — no
//! cross-format translation. Only token-usage extraction is format-aware: we
//! parse `usage` from the upstream payload (OpenAI Chat Completions, OpenAI
//! Responses, or Anthropic Messages) so the existing usage-logging pipeline
//! in `crate::usage` keeps working uniformly across all three endpoints.

use super::error::ApiError;
use super::AppState;
use crate::pool::Account;
use crate::usage::Record;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

/// Buffer the upstream response, parse usage, record it, and return a
/// JSON `Response` to the client (status + content-type preserved).
pub(crate) async fn buffered_response(
  s: AppState,
  acct: Arc<Account>,
  resp: reqwest::Response,
  model: String,
  initiator: String,
  started: Instant,
) -> Response {
  let status = resp.status();
  let bytes = match resp.bytes().await {
    Ok(b) => b,
    Err(e) => {
      return ApiError::upstream(StatusCode::BAD_GATEWAY, e.to_string()).into_response();
    }
  };

  let (pt, ct) = parse_usage_any_json(&bytes);
  record_usage(
    &s,
    &acct.id,
    &model,
    &initiator,
    pt,
    ct,
    started,
    status.as_u16(),
    false,
  );

  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("application/json"),
  );
  (status, headers, bytes).into_response()
}

/// Stream the upstream response back as `text/event-stream`, scanning each
/// `data:` SSE frame for a `usage` block to flush to the usage db when the
/// stream terminates.
pub(crate) async fn stream_response(
  s: AppState,
  acct: Arc<Account>,
  resp: reqwest::Response,
  model: String,
  initiator: String,
  started: Instant,
) -> Response {
  let status = resp.status();

  let usage_holder = Arc::new(parking_lot::Mutex::new((None::<u64>, None::<u64>)));
  let usage_for_stream = usage_holder.clone();
  let acct_id = acct.id.clone();
  let model_clone = model.clone();
  let initiator_clone = initiator.clone();
  let s_clone = s.clone();

  let upstream = resp.bytes_stream();
  let mut buffer = Vec::<u8>::new();

  let mapped = upstream.map(move |chunk| match chunk {
    Ok(b) => {
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
        // Search the SSE frame for usage in any of the known shapes.
        // Anthropic ships partial usage on `message_start`
        // (input_tokens) and on `message_delta` (output_tokens) so
        // we merge non-None updates rather than overwriting.
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
  let on_end = move || {
    if *recorded_clone.lock() {
      return;
    }
    *recorded_clone.lock() = true;
    let (pt, ct) = *usage_holder.lock();
    record_usage(
      &s_clone,
      &acct_id,
      &model_clone,
      &initiator_clone,
      pt,
      ct,
      started,
      status.as_u16(),
      true,
    );
  };

  let stream = StreamWithFinalizer::new(mapped, on_end);
  let body = Body::from_stream(stream);

  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("text/event-stream"),
  );
  headers.insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  headers.insert(axum::http::header::CONNECTION, HeaderValue::from_static("keep-alive"));
  (status, headers, body).into_response()
}

/// Parse `usage.{prompt,completion}_tokens` (OpenAI Chat Completions),
/// `usage.{input,output}_tokens` (OpenAI Responses & Anthropic Messages),
/// or the Anthropic streaming variant where `usage` lives nested under
/// `message.usage` (on `message_start` events).
pub(crate) fn parse_usage_any_value(v: &Value) -> (Option<u64>, Option<u64>) {
  // Direct `usage` block.
  if let Some(u) = v.get("usage") {
    let pt = u
      .get("prompt_tokens")
      .or_else(|| u.get("input_tokens"))
      .and_then(|x| x.as_u64());
    let ct = u
      .get("completion_tokens")
      .or_else(|| u.get("output_tokens"))
      .and_then(|x| x.as_u64());
    if pt.is_some() || ct.is_some() {
      return (pt, ct);
    }
  }
  // Anthropic `message_start` shape: `{ type: "message_start",
  // message: { ..., usage: { input_tokens, output_tokens } } }`.
  if let Some(m) = v.get("message").and_then(|m| m.get("usage")) {
    let pt = m.get("input_tokens").and_then(|x| x.as_u64());
    let ct = m.get("output_tokens").and_then(|x| x.as_u64());
    return (pt, ct);
  }
  // OpenAI Responses streaming `response.completed` shape:
  // `{ type: "response.completed", response: { usage: { ... } } }`.
  if let Some(u) = v.get("response").and_then(|r| r.get("usage")) {
    let pt = u
      .get("input_tokens")
      .or_else(|| u.get("prompt_tokens"))
      .and_then(|x| x.as_u64());
    let ct = u
      .get("output_tokens")
      .or_else(|| u.get("completion_tokens"))
      .and_then(|x| x.as_u64());
    return (pt, ct);
  }
  (None, None)
}

fn parse_usage_any_json(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
  let v: Value = match serde_json::from_slice(bytes) {
    Ok(v) => v,
    Err(_) => return (None, None),
  };
  parse_usage_any_value(&v)
}

#[allow(clippy::too_many_arguments)]
fn record_usage(
  s: &AppState,
  account_id: &str,
  model: &str,
  initiator: &str,
  pt: Option<u64>,
  ct: Option<u64>,
  started: Instant,
  status: u16,
  stream: bool,
) {
  if !s.usage_enabled {
    return;
  }
  let Some(db) = s.usage.as_ref() else { return };
  let latency_ms = started.elapsed().as_millis() as u64;
  if let Err(e) = db.record(Record {
    account_id,
    model,
    initiator,
    prompt_tokens: pt,
    completion_tokens: ct,
    latency_ms,
    status,
    stream,
  }) {
    tracing::warn!(error = %e, "failed to write usage row");
  }
}

// --- Stream wrapper that runs a closure when polled to completion or dropped.

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

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn parses_openai_chat_usage() {
    let v = json!({ "usage": { "prompt_tokens": 11, "completion_tokens": 22 }});
    assert_eq!(parse_usage_any_value(&v), (Some(11), Some(22)));
  }

  #[test]
  fn parses_responses_usage_shape() {
    let v = json!({ "usage": { "input_tokens": 5, "output_tokens": 7 }});
    assert_eq!(parse_usage_any_value(&v), (Some(5), Some(7)));
  }

  #[test]
  fn parses_anthropic_message_start_nested_usage() {
    let v = json!({
        "type": "message_start",
        "message": { "usage": { "input_tokens": 9, "output_tokens": 1 }}
    });
    assert_eq!(parse_usage_any_value(&v), (Some(9), Some(1)));
  }

  #[test]
  fn parses_responses_response_completed_nested_usage() {
    let v = json!({
        "type": "response.completed",
        "response": { "usage": { "input_tokens": 3, "output_tokens": 4 }}
    });
    assert_eq!(parse_usage_any_value(&v), (Some(3), Some(4)));
  }
}
