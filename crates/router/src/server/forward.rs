//! Format-agnostic upstream-response forwarders.
//!
//! Both buffered and streaming variants forward upstream bytes verbatim — no
//! cross-format translation. Only token-usage extraction is format-aware: we
//! parse `usage` from the upstream payload (OpenAI Chat Completions, OpenAI
//! Responses, or Anthropic Messages) so the existing usage-logging pipeline
//! in `crate::db` keeps working uniformly across all three endpoints.

use super::error::ApiError;
use super::AppState;
use crate::db::{CallRecord, HttpSnapshot, MessageRecord, OutboundSnapshot, PartRecord, SessionSource};
use crate::pool::AccountHandle;
use crate::provider::Endpoint;
use crate::util::initiator::{classify_initiator, classify_initiator_responses};
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue, Method};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt;
use serde_json::Value;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;
use uuid::Uuid;

/// Buffer the upstream response, parse usage, record it, and return a
/// JSON `Response` to the client (status + content-type preserved).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn buffered_response(
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
  let bytes = match resp.bytes().await {
    Ok(b) => b,
    Err(e) => {
      return ApiError::bad_gateway(format!("reading upstream body: {e}")).into_response();
    }
  };

  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("application/json"),
  );
  if let Some(id) = session_id.as_deref() {
    if let Ok(value) = HeaderValue::from_str(id) {
      headers.insert(super::SESSION_ID_HEADER, value);
    }
  }

  let bytes = if upstream_endpoint == endpoint {
    bytes
  } else {
    match serde_json::from_slice::<Value>(&bytes)
      .map_err(|e| e.to_string())
      .and_then(|v| crate::convert::convert_response(upstream_endpoint, endpoint, &v).map_err(|e| e.to_string()))
      .and_then(|v| serde_json::to_vec(&v).map(Bytes::from).map_err(|e| e.to_string()))
    {
      Ok(bytes) => bytes,
      Err(e) => return ApiError::bad_gateway(format!("response conversion failed: {e}")).into_response(),
    }
  };

  let (pt, ct) = parse_usage_any_json(&bytes);
  record_call(
    &s,
    &acct.id(),
    acct.provider.info().id.as_str(),
    endpoint,
    &model,
    &initiator,
    session_id.as_deref(),
    request_id.as_deref(),
    project_id.as_deref(),
    &req_headers,
    &req_body,
    Some(&resp_headers),
    Some(&bytes),
    &headers,
    outbound,
    pt,
    ct,
    started,
    status.as_u16(),
    false,
  );

  (status, headers, bytes).into_response()
}

/// Stream the upstream response back as `text/event-stream`, scanning each
/// `data:` SSE frame for a `usage` block to flush to the usage db when the
/// stream terminates.
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
      headers.insert(super::SESSION_ID_HEADER, value);
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
pub(crate) fn record_passthrough_call(
  s: &AppState,
  host: &str,
  method: &Method,
  path_and_query: &str,
  req_headers: &HeaderMap,
  req_body: &Bytes,
  outbound_req_headers: &HeaderMap,
  resp_headers: &HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
) {
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

  record_call_with_snapshots(
    s,
    "passthrough",
    host,
    endpoint.map(Endpoint::as_str).unwrap_or(logical_path),
    &model,
    &initiator,
    super::first_header(req_headers, super::SESSION_ID_HEADERS),
    super::first_header(req_headers, super::REQUEST_ID_HEADERS),
    super::first_header(req_headers, super::PROJECT_ID_HEADERS),
    req_body,
    endpoint,
    Some(HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: req_headers.clone(),
      body: req_body.clone(),
    }),
    Some(HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: outbound_req_headers.clone(),
      body: req_body.clone(),
    }),
    Some(HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: resp_headers.clone(),
      body: resp_body.clone(),
    }),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: resp_headers.clone(),
      body: resp_body.clone(),
    },
    parse_usage_any_json(resp_body),
    started,
    status,
    stream,
  );
}

fn passthrough_endpoint(method: &Method, path: &str) -> Option<Endpoint> {
  match (method, path) {
    (&Method::POST, "/v1/chat/completions") => Some(Endpoint::ChatCompletions),
    (&Method::POST, "/v1/responses") => Some(Endpoint::Responses),
    (&Method::POST, "/v1/messages") => Some(Endpoint::Messages),
    _ => None,
  }
}

fn classify_passthrough_initiator(headers: &HeaderMap, endpoint: Option<Endpoint>, body: &Value) -> &'static str {
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

#[allow(clippy::too_many_arguments)]
fn record_call(
  s: &AppState,
  account_id: &str,
  provider_id: &str,
  endpoint: Endpoint,
  model: &str,
  initiator: &str,
  session_id: Option<&str>,
  request_id: Option<&str>,
  project_id: Option<&str>,
  req_headers: &HeaderMap,
  req_body: &Value,
  resp_headers: Option<&HeaderMap>,
  resp_body: Option<&bytes::Bytes>,
  inbound_resp_headers: &HeaderMap,
  outbound: Option<OutboundSnapshot>,
  pt: Option<u64>,
  ct: Option<u64>,
  started: Instant,
  status: u16,
  stream: bool,
) {
  let req_body_bytes = serde_json::to_vec(req_body).unwrap_or_default();
  record_call_with_snapshots(
    s,
    account_id,
    provider_id,
    endpoint.as_str(),
    model,
    initiator,
    session_id,
    request_id,
    project_id,
    &req_body_bytes,
    Some(endpoint),
    Some(HttpSnapshot {
      method: None,
      url: None,
      status: None,
      headers: req_headers.clone(),
      body: Bytes::from(req_body_bytes.clone()),
    }),
    outbound,
    resp_headers.map(|headers| HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: headers.clone(),
      body: resp_body.cloned().unwrap_or_default(),
    }),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: inbound_resp_headers.clone(),
      body: resp_body.cloned().unwrap_or_default(),
    },
    (pt, ct),
    started,
    status,
    stream,
  );
}

#[allow(clippy::too_many_arguments)]
fn record_call_with_snapshots(
  s: &AppState,
  account_id: &str,
  provider_id: &str,
  endpoint: &str,
  model: &str,
  initiator: &str,
  session_id: Option<&str>,
  request_id: Option<&str>,
  project_id: Option<&str>,
  req_body: &[u8],
  message_endpoint: Option<Endpoint>,
  inbound_req: Option<HttpSnapshot>,
  outbound_req: Option<OutboundSnapshot>,
  outbound_resp: Option<HttpSnapshot>,
  inbound_resp: HttpSnapshot,
  usage: (Option<u64>, Option<u64>),
  started: Instant,
  status: u16,
  stream: bool,
) {
  let Some(db) = s.db.as_ref() else { return };
  let latency_ms = started.elapsed().as_millis() as u64;
  let max = db.body_max_bytes();
  let req_body_json = serde_json::from_slice::<Value>(req_body).unwrap_or(Value::Null);
  let mut messages = message_endpoint
    .map(|endpoint| extract_request_messages(&req_body_json, endpoint, max))
    .unwrap_or_default();
  if !inbound_resp.body.is_empty() && message_endpoint.is_some() {
    messages.push(MessageRecord {
      role: "assistant".into(),
      status: Some(status),
      parts: vec![PartRecord {
        part_type: "raw".into(),
        content: clip_body(&inbound_resp.body, max),
      }],
    });
  }
  let (effective_id, source) = match session_id {
    Some(id) => (id.to_string(), SessionSource::Header),
    None => (Uuid::new_v4().to_string(), SessionSource::Auto),
  };
  db.record(CallRecord {
    ts: time::OffsetDateTime::now_utc().unix_timestamp(),
    session_id: effective_id,
    session_source: source,
    request_id: request_id.map(str::to_string),
    project_id: project_id.map(str::to_string),
    endpoint: endpoint.to_string(),
    account_id: account_id.to_string(),
    provider_id: provider_id.to_string(),
    model: model.to_string(),
    initiator: initiator.to_string(),
    status,
    stream,
    latency_ms,
    prompt_tokens: usage.0,
    completion_tokens: usage.1,
    inbound_req: inbound_req.map(|mut snap| {
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    }).unwrap_or_default(),
    outbound_req: outbound_req.map(|mut snap| {
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    }),
    outbound_resp: outbound_resp.map(|mut snap| {
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    }),
    inbound_resp: {
      let mut snap = inbound_resp;
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    },
    messages,
  });
}

fn clip_body(body: &[u8], max: usize) -> bytes::Bytes {
  if body.len() <= max {
    return bytes::Bytes::copy_from_slice(body);
  }
  serde_json::json!({ "_truncated": true, "size": body.len() })
    .to_string()
    .into_bytes()
    .into()
}

/// Build per-message [`MessageRecord`]s for `sessions.db`. For each inbound
/// message we emit one record whose `parts` contains one `PartRecord` per
/// content element (string content collapses to a single `text` part).
fn extract_request_messages(body: &Value, endpoint: Endpoint, max: usize) -> Vec<MessageRecord> {
  let mut out = Vec::new();
  match endpoint {
    Endpoint::ChatCompletions | Endpoint::Messages => {
      if endpoint == Endpoint::Messages {
        if let Some(system) = body.get("system") {
          out.push(MessageRecord {
            role: "system".into(),
            status: None,
            parts: parts_from_content(system, max),
          });
        }
      }
      if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
          let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user").to_string();
          let parts = match msg.get("content") {
            Some(content) => parts_from_content(content, max),
            None => vec![PartRecord {
              part_type: "raw".into(),
              content: clip_body(&serde_json::to_vec(msg).unwrap_or_default(), max),
            }],
          };
          out.push(MessageRecord {
            role,
            status: None,
            parts,
          });
        }
      }
    }
    Endpoint::Responses => {
      let input = body.get("input").unwrap_or(body);
      if let Some(items) = input.as_array() {
        for item in items {
          let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user").to_string();
          let parts = match item.get("content") {
            Some(content) => parts_from_content(content, max),
            None => vec![PartRecord {
              part_type: "raw".into(),
              content: clip_body(&serde_json::to_vec(item).unwrap_or_default(), max),
            }],
          };
          out.push(MessageRecord {
            role,
            status: None,
            parts,
          });
        }
      } else if let Some(text) = input.as_str() {
        out.push(MessageRecord {
          role: "user".into(),
          status: None,
          parts: vec![PartRecord {
            part_type: "text".into(),
            content: clip_body(text.as_bytes(), max),
          }],
        });
      } else {
        out.push(MessageRecord {
          role: "user".into(),
          status: None,
          parts: vec![PartRecord {
            part_type: "raw".into(),
            content: clip_body(&serde_json::to_vec(input).unwrap_or_default(), max),
          }],
        });
      }
    }
  }
  out
}

/// Convert a message `content` value (either a string or an array of typed
/// parts) into one or more [`PartRecord`]s. `text`/`input_text` parts are
/// stored as utf-8; structured parts (`image_url`, `tool_use`,
/// `tool_result`, …) are stored as their JSON serialisation so the original
/// shape is recoverable.
fn parts_from_content(content: &Value, max: usize) -> Vec<PartRecord> {
  if let Some(text) = content.as_str() {
    return vec![PartRecord {
      part_type: "text".into(),
      content: clip_body(text.as_bytes(), max),
    }];
  }
  if let Some(items) = content.as_array() {
    if items.is_empty() {
      return vec![PartRecord {
        part_type: "raw".into(),
        content: Bytes::from_static(b"[]"),
      }];
    }
    return items
      .iter()
      .map(|item| {
        let part_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("raw").to_string();
        let content_bytes = if matches!(part_type.as_str(), "text" | "input_text" | "output_text") {
          if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            clip_body(text.as_bytes(), max)
          } else {
            clip_body(&serde_json::to_vec(item).unwrap_or_default(), max)
          }
        } else {
          clip_body(&serde_json::to_vec(item).unwrap_or_default(), max)
        };
        PartRecord {
          part_type,
          content: content_bytes,
        }
      })
      .collect();
  }
  vec![PartRecord {
    part_type: "raw".into(),
    content: clip_body(&serde_json::to_vec(content).unwrap_or_default(), max),
  }]
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
  use crate::config::{Account as AccountCfg, AuthType, Config};
  use crate::server::build_state;
  use crate::util::secret::Secret;
  use serde_json::json;
  use std::sync::{Arc, Mutex};

  #[derive(Default)]
  struct FakeDb {
    records: Mutex<Vec<CallRecord>>,
  }

  impl crate::db::DbStore for FakeDb {
    fn body_max_bytes(&self) -> usize {
      1024 * 1024
    }

    fn record(&self, record: CallRecord) {
      self.records.lock().unwrap().push(record);
    }
  }

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

  #[test]
  fn chat_array_content_becomes_multiple_parts() {
    let body = json!({
      "messages": [{
        "role": "user",
        "content": [
          { "type": "text", "text": "hello" },
          { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc" } }
        ]
      }]
    });
    let messages = extract_request_messages(&body, Endpoint::ChatCompletions, 1024);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].parts.len(), 2);
    assert_eq!(messages[0].parts[0].part_type, "text");
    assert_eq!(messages[0].parts[0].content.as_ref(), b"hello");
    assert_eq!(messages[0].parts[1].part_type, "image_url");
    assert!(std::str::from_utf8(messages[0].parts[1].content.as_ref())
      .unwrap()
      .contains("image_url"));
  }

  #[tokio::test]
  async fn record_call_generates_auto_session_and_assistant_raw_part() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let db = Arc::new(FakeDb::default());
    let state = build_state(&cfg, Some(db.clone())).unwrap();
    let req_body = json!({ "model": "glm-4.6", "messages": [{ "role": "user", "content": "hi" }] });
    let resp_body = Bytes::from_static(br#"{"id":"r1"}"#);
    record_call(
      &state,
      "acct",
      "zai-coding-plan",
      Endpoint::ChatCompletions,
      "glm-4.6",
      "user",
      None,
      None,
      None,
      &HeaderMap::new(),
      &req_body,
      Some(&HeaderMap::new()),
      Some(&resp_body),
      &HeaderMap::new(),
      None,
      None,
      None,
      Instant::now(),
      200,
      false,
    );
    let records = db.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].session_source, SessionSource::Auto);
    Uuid::parse_str(&records[0].session_id).unwrap();
    assert!(records[0]
      .messages
      .iter()
      .flat_map(|m| &m.parts)
      .any(|p| p.part_type == "raw" && p.content.as_ref() == resp_body.as_ref()));
  }

  #[tokio::test]
  async fn record_call_persists_header_session_request_and_project_ids() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let db = Arc::new(FakeDb::default());
    let state = build_state(&cfg, Some(db.clone())).unwrap();
    let req_body = json!({ "model": "glm-4.6", "messages": [] });

    record_call(
      &state,
      "acct",
      "zai-coding-plan",
      Endpoint::ChatCompletions,
      "glm-4.6",
      "user",
      Some("client-session"),
      Some("request-123"),
      Some("project-456"),
      &HeaderMap::new(),
      &req_body,
      None,
      None,
      &HeaderMap::new(),
      None,
      None,
      None,
      Instant::now(),
      200,
      false,
    );

    let records = db.records.lock().unwrap();
    assert_eq!(records[0].session_id, "client-session");
    assert_eq!(records[0].session_source, SessionSource::Header);
    assert_eq!(records[0].request_id.as_deref(), Some("request-123"));
    assert_eq!(records[0].project_id.as_deref(), Some("project-456"));
  }

  #[tokio::test]
  async fn record_passthrough_call_persists_requests_row_shape() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let db = Arc::new(FakeDb::default());
    let state = build_state(&cfg, Some(db.clone())).unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert("x-session-id", "client-session".parse().unwrap());
    let mut outbound_req_headers = HeaderMap::new();
    outbound_req_headers.insert(axum::http::header::HOST, "api.openai.com".parse().unwrap());
    let req_body = Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#);
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().unwrap());
    let resp_body = Bytes::from_static(br#"{"usage":{"prompt_tokens":1,"completion_tokens":2}}"#);

    record_passthrough_call(
      &state,
      "api.openai.com",
      &Method::POST,
      "/v1/chat/completions",
      &req_headers,
      &req_body,
      &outbound_req_headers,
      &resp_headers,
      &resp_body,
      200,
      Instant::now(),
    );

    let records = db.records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].endpoint, "chat_completions");
    assert_eq!(records[0].account_id, "passthrough");
    assert_eq!(records[0].provider_id, "api.openai.com");
    assert_eq!(records[0].model, "gpt-4.1");
    assert_eq!(records[0].session_id, "client-session");
    assert_eq!(records[0].stream, true);
    assert_eq!(records[0].prompt_tokens, Some(1));
    assert_eq!(records[0].completion_tokens, Some(2));
    assert_eq!(records[0].inbound_req.method.as_deref(), Some("POST"));
    assert_eq!(
      records[0].inbound_req.url.as_deref(),
      Some("https://api.openai.com/v1/chat/completions")
    );
    assert_eq!(
      records[0].outbound_req.as_ref().and_then(|s| s.url.as_deref()),
      Some("https://api.openai.com/v1/chat/completions")
    );
  }
}
