//! Default Extract stage — turns a [`RawInbound`] into normalized
//! [`Extracted`] state.
//!
//! Behavior is a clean reimplementation of the legacy
//! `crates/router/src/pipeline/parse.rs::{request_header_extract,
//! request_body_extract, RequestParser::parse}`, with all small strings stored
//! as [`SmolStr`].
//!
//! Header name lists are duplicated here intentionally to keep requests free
//! of any dependency on the legacy `crates/router` crate. PR2 will move the
//! canonical constants to a shared location.

pub mod passthrough;
pub use passthrough::PassthroughExtract;

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ExtractStage, Extracted, RawInbound};
use crate::utils::codec::request_content_encoding;
use async_trait::async_trait;
use serde_json::Value;
use smol_str::SmolStr;
use std::sync::Arc;
use tokn_headers::HeaderMap;

const SESSION_ID_HEADERS: &[&str] = &[
  "x-session-id",
  "x-client-session-id",
  "session_id",
  "x-session-affinity",
  "x-opencode-session",
];
#[allow(dead_code)] // request_id propagation lives in the runner; PR2 may consume this.
const REQUEST_ID_HEADERS: &[&str] = &["x-request-id", "x-interaction-id", "x-opencode-request"];
const PROJECT_ID_HEADERS: &[&str] = &["x-opencode-project"];

pub struct DefaultExtract;

#[async_trait]
impl ExtractStage for DefaultExtract {
  async fn extract(&self, _ctx: &PipelineCtx, raw: RawInbound) -> Result<Extracted, PipelineError> {
    let RawInbound {
      endpoint: _,
      headers,
      raw_body,
      decoded_body,
      body_json,
      request_id: _,
    } = raw;

    let model = body_json
      .get("model")
      .and_then(Value::as_str)
      .filter(|s| !s.is_empty())
      .map(SmolStr::new)
      .unwrap_or_else(|| SmolStr::new("unknown"));

    let stream = infer_stream(&headers, &body_json);

    let header_initiator = header_str(&headers, "x-initiator")
      .map(|s| s.trim().to_ascii_lowercase())
      .filter(|s| s == "user" || s == "agent")
      .map(SmolStr::new);

    let initiator = header_initiator
      .clone()
      .unwrap_or_else(|| SmolStr::new(classify_initiator(&body_json)));

    let session_id = first_header(&headers, SESSION_ID_HEADERS).map(SmolStr::new);
    let project_id = first_header(&headers, PROJECT_ID_HEADERS).map(SmolStr::new);

    let route_mode_hint = header_str(&headers, "x-route-mode")
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(SmolStr::new);

    // Parsing failures here are recoverable for the codec layer
    // (which would have failed loudly at the transport boundary
    // before we got here) but not for ConvertRequest. We treat a
    // parse error as `None` so downstream stages just emit an
    // uncompressed body; the legacy router behaviour was identical.
    let content_encoding = request_content_encoding(&headers).ok().flatten();

    Ok(Extracted {
      client_id: None,
      model,
      stream,
      session_id,
      project_id,
      initiator,
      header_initiator,
      route_mode_hint,
      headers,
      raw_body,
      decoded_body,
      body_json: Arc::new(body_json),
      content_encoding,
    })
  }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
  headers.get(name).map(|v| v.as_str())
}

fn first_header<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
  names
    .iter()
    .find_map(|name| header_str(headers, name).map(str::trim).filter(|s| !s.is_empty()))
}

fn infer_stream(headers: &HeaderMap, body: &Value) -> bool {
  if let Some(stream) = body.get("stream").and_then(Value::as_bool) {
    return stream;
  }
  header_str(headers, "accept")
    .map(|v| {
      v.split(',')
        .any(|part| part.split(';').next().map(str::trim) == Some("text/event-stream"))
    })
    .unwrap_or(false)
}

/// Conservative heuristic mirroring `crates/router::util::initiator`. We
/// classify based purely on body shape since requests doesn't yet depend on
/// the legacy crate's helpers. The detail is "agent" iff a `tools` array is
/// present and non-empty (common signal of agent-style calls); otherwise
/// "user". This is intentionally simpler than the legacy implementation —
/// PR2 will port the full classifier when we extract it to a shared crate.
fn classify_initiator(body: &Value) -> &'static str {
  let has_tools = body
    .get("tools")
    .and_then(Value::as_array)
    .map(|a| !a.is_empty())
    .unwrap_or(false);
  if has_tools {
    "agent"
  } else {
    "user"
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use bytes::Bytes;
  use std::sync::Arc;
  use tokn_core::provider::Endpoint;

  fn ctx() -> PipelineCtx {
    PipelineCtx::new("req-test", Endpoint::ChatCompletions, Arc::new(EventBus::new(64)))
  }

  fn raw(headers: HeaderMap, body: Value) -> RawInbound {
    let decoded = Bytes::from(serde_json::to_vec(&body).unwrap());
    RawInbound {
      endpoint: Endpoint::ChatCompletions,
      headers,
      raw_body: decoded.clone(),
      decoded_body: decoded,
      body_json: body,
      request_id: None,
    }
  }

  fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
      h.insert(
        tokn_headers::HeaderName::new(*k),
        tokn_headers::HeaderValue::from_string((*v).to_string()),
      );
    }
    h
  }

  #[tokio::test]
  async fn extracts_model_and_default_initiator() {
    let body = serde_json::json!({"model": "gpt-x", "messages": []});
    let ex = DefaultExtract
      .extract(&ctx(), raw(HeaderMap::new(), body))
      .await
      .expect("extract should succeed");
    assert_eq!(ex.model, "gpt-x");
    assert_eq!(ex.initiator, "user");
    assert!(!ex.stream);
    assert!(ex.client_id.is_none());
  }

  #[tokio::test]
  async fn x_behave_as_is_ignored() {
    let body = serde_json::json!({"model": "m"});
    let headers = header_map(&[("x-behave-as", "  codex  ")]);
    let ex = DefaultExtract.extract(&ctx(), raw(headers, body)).await.unwrap();
    assert!(ex.client_id.is_none());
  }

  #[tokio::test]
  async fn stream_from_body_takes_precedence() {
    let body = serde_json::json!({"model": "m", "stream": true});
    let headers = header_map(&[("accept", "application/json")]);
    let ex = DefaultExtract.extract(&ctx(), raw(headers, body)).await.unwrap();
    assert!(ex.stream);
  }

  #[tokio::test]
  async fn stream_inferred_from_accept_sse() {
    let body = serde_json::json!({"model": "m"});
    let headers = header_map(&[("accept", "text/event-stream, application/json")]);
    let ex = DefaultExtract.extract(&ctx(), raw(headers, body)).await.unwrap();
    assert!(ex.stream);
  }

  #[tokio::test]
  async fn agent_initiator_when_tools_present() {
    let body = serde_json::json!({"model": "m", "tools": [{"type":"function"}]});
    let ex = DefaultExtract
      .extract(&ctx(), raw(HeaderMap::new(), body))
      .await
      .unwrap();
    assert_eq!(ex.initiator, "agent");
  }

  #[tokio::test]
  async fn header_initiator_overrides_body_classification() {
    let body = serde_json::json!({"model": "m", "tools": [{"type":"function"}]});
    let headers = header_map(&[("x-initiator", "user")]);
    let ex = DefaultExtract.extract(&ctx(), raw(headers, body)).await.unwrap();
    assert_eq!(ex.initiator, "user");
    assert_eq!(ex.header_initiator.as_deref(), Some("user"));
  }

  #[tokio::test]
  async fn session_request_project_ids_extracted_with_priority() {
    let body = serde_json::json!({"model": "m"});
    let headers = header_map(&[
      ("x-session-id", "   "),
      ("x-client-session-id", " sess-2 "),
      ("x-opencode-project", "proj-9"),
    ]);
    let ex = DefaultExtract.extract(&ctx(), raw(headers, body)).await.unwrap();
    assert_eq!(ex.session_id.as_deref(), Some("sess-2"));
    assert_eq!(ex.project_id.as_deref(), Some("proj-9"));
  }
}
