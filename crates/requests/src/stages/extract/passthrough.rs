//! Zero-parse Extract stage for the passthrough pipeline.
//!
//! The contract differs from [`DefaultExtract`](super::DefaultExtract) on
//! one axis: we must **not** treat the inbound JSON body as authoritative
//! and we must **not** keep it around as `Arc<Value>` for downstream
//! stages to re-serialize. The body bytes are forwarded verbatim by
//! [`PassthroughConvertRequest`](crate::stages::PassthroughConvertRequest)
//! and the only thing Resolve needs is the *model name*.
//!
//! Strategy: do a single cheap `serde_json::from_slice::<ModelPeek>(...)`
//! to pull `model` (and `stream`) out of the body, then discard the parsed
//! value. The full body bytes remain in `raw_body` / `decoded_body` and
//! `body_json` is set to `Value::Null` to signal "do not consult".
//!
//! Header extraction (session, project, route mode, initiator, etc.)
//! mirrors `DefaultExtract` since BuildHeaders and Send both depend on
//! those values.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ExtractStage, Extracted, RawInbound};
use crate::utils::codec::request_content_encoding;
use async_trait::async_trait;
use serde::Deserialize;
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
const PROJECT_ID_HEADERS: &[&str] = &["x-opencode-project"];

/// Minimal field set peeled off the inbound JSON body. Everything else
/// is intentionally ignored — the body bytes are forwarded verbatim
/// without re-serialization.
#[derive(Debug, Default, Deserialize)]
struct ModelPeek {
  #[serde(default)]
  model: Option<SmolStr>,
  #[serde(default)]
  stream: Option<bool>,
}

pub struct PassthroughExtract;

#[async_trait]
impl ExtractStage for PassthroughExtract {
  async fn extract(&self, _ctx: &PipelineCtx, raw: RawInbound) -> Result<Extracted, PipelineError> {
    let RawInbound {
      endpoint: _,
      headers,
      raw_body,
      decoded_body,
      body_json: _,
      request_id: _,
    } = raw;

    // Cheap peek for routing-relevant fields. We deliberately ignore
    // parse errors — passthrough must remain best-effort for routing
    // while still forwarding the original bytes verbatim.
    let peek: ModelPeek = serde_json::from_slice(&decoded_body).unwrap_or_default();

    let model = peek
      .model
      .filter(|s| !s.is_empty())
      .unwrap_or_else(|| SmolStr::new("unknown"));

    let stream = peek.stream.unwrap_or_else(|| accept_is_sse(&headers));

    let header_initiator = header_str(&headers, "x-initiator")
      .map(|s| s.trim().to_ascii_lowercase())
      .filter(|s| s == "user" || s == "agent")
      .map(SmolStr::new);

    // Passthrough has no body-shape classifier — fall back to "user"
    // when no header initiator is provided.
    let initiator = header_initiator.clone().unwrap_or_else(|| SmolStr::new("user"));

    let session_id = first_header(&headers, SESSION_ID_HEADERS).map(SmolStr::new);
    let project_id = first_header(&headers, PROJECT_ID_HEADERS).map(SmolStr::new);

    let route_mode_hint = header_str(&headers, "x-route-mode")
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(SmolStr::new);

    let content_encoding = request_content_encoding(&headers).ok().flatten();

    Ok(Extracted {
      agent_id: None,
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
      // Sentinel: passthrough downstream stages must not read body JSON.
      body_json: Arc::new(Value::Null),
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

fn accept_is_sse(headers: &HeaderMap) -> bool {
  header_str(headers, "accept")
    .map(|v| {
      v.split(',')
        .any(|part| part.split(';').next().map(str::trim) == Some("text/event-stream"))
    })
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use bytes::Bytes;
  use std::sync::Arc;
  use tokn_core::provider::Endpoint;

  fn ctx() -> PipelineCtx {
    PipelineCtx::new(
      "req-passthrough",
      Endpoint::ChatCompletions,
      Arc::new(EventBus::new(64)),
    )
  }

  fn raw_with_body(body_bytes: Bytes, headers: HeaderMap) -> RawInbound {
    RawInbound {
      endpoint: Endpoint::ChatCompletions,
      headers,
      raw_body: body_bytes.clone(),
      decoded_body: body_bytes,
      // Pretend the transport did decode the JSON for the legacy path;
      // PassthroughExtract must NOT consult this.
      body_json: serde_json::json!({"sentinel": "should-not-be-read"}),
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
  async fn peeks_model_from_body_without_keeping_value() {
    let body = Bytes::from(r#"{"model":"gpt-4o","stream":true,"messages":[]}"#);
    let ex = PassthroughExtract
      .extract(&ctx(), raw_with_body(body.clone(), HeaderMap::new()))
      .await
      .expect("extract should succeed");
    assert_eq!(ex.model, "gpt-4o");
    assert!(ex.stream);
    assert_eq!(*ex.body_json, Value::Null, "body_json must be null sentinel");
    assert_eq!(ex.raw_body, body, "raw bytes preserved verbatim");
  }

  #[tokio::test]
  async fn unparseable_body_yields_unknown_model() {
    let body = Bytes::from_static(&[0xff, 0xfe, 0xfd]);
    let ex = PassthroughExtract
      .extract(&ctx(), raw_with_body(body.clone(), HeaderMap::new()))
      .await
      .unwrap();
    assert_eq!(ex.model, "unknown");
    assert!(!ex.stream);
    assert_eq!(ex.raw_body, body);
  }

  #[tokio::test]
  async fn stream_falls_back_to_accept_when_body_silent() {
    let body = Bytes::from(r#"{"model":"m"}"#);
    let headers = header_map(&[("accept", "text/event-stream, application/json")]);
    let ex = PassthroughExtract
      .extract(&ctx(), raw_with_body(body, headers))
      .await
      .unwrap();
    assert!(ex.stream);
  }

  #[tokio::test]
  async fn headers_extracted_like_default() {
    let body = Bytes::from(r#"{"model":"m"}"#);
    let headers = header_map(&[
      ("x-session-id", "sess-1"),
      ("x-opencode-project", "/p"),
      ("x-route-mode", "passthrough"),
      ("x-behave-as", "codex"),
      ("x-initiator", "agent"),
    ]);
    let ex = PassthroughExtract
      .extract(&ctx(), raw_with_body(body, headers))
      .await
      .unwrap();
    assert_eq!(ex.session_id.as_deref(), Some("sess-1"));
    assert_eq!(ex.project_id.as_deref(), Some("/p"));
    assert_eq!(ex.route_mode_hint.as_deref(), Some("passthrough"));
    assert!(ex.agent_id.is_none());
    assert_eq!(ex.initiator, "agent");
    assert_eq!(ex.header_initiator.as_deref(), Some("agent"));
  }
}
