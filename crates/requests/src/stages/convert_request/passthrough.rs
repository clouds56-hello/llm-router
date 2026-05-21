//! Zero-parse ConvertRequest stage.
//!
//! Forwards the inbound body **verbatim** to the upstream:
//! `upstream_wire_body = extracted.raw_body.clone()` (bytes still in their
//! original on-wire encoding). No JSON parse, no model rewrite, no
//! cross-endpoint translation, no provider input transformer.
//!
//! `upstream_body` is set to `Value::Null` because no observer should
//! consume it; subscribers that care about request bodies must read the
//! `Bytes` (`debug_outbound_body` / wire body) instead.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ConvertRequestStage, ConvertedRequest, Extracted, Resolved};
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub struct PassthroughConvertRequest;

#[async_trait]
impl ConvertRequestStage for PassthroughConvertRequest {
  async fn convert_request(
    &self,
    _ctx: &PipelineCtx,
    extracted: &Extracted,
    _resolved: &Resolved,
  ) -> Result<ConvertedRequest, PipelineError> {
    Ok(ConvertedRequest {
      // Sentinel: the body was never parsed. Observers must not treat
      // this as a real upstream body.
      upstream_body: Arc::new(Value::Null),
      upstream_wire_body: extracted.raw_body.clone(),
      debug_outbound_body: extracted.decoded_body.clone(),
      content_encoding: extracted.content_encoding,
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use bytes::Bytes;
  use serde_json::json;
  use smol_str::SmolStr;
  use std::sync::Arc;
  use tokn_core::provider::Endpoint;
  use tokn_headers::HeaderMap;

  fn ctx() -> PipelineCtx {
    PipelineCtx::new("req", Endpoint::ChatCompletions, Arc::new(EventBus::new(16)))
  }

  fn extracted(raw: Bytes, decoded: Bytes) -> Extracted {
    Extracted {
      agent_id: None,
      model: SmolStr::new("m"),
      stream: false,
      session_id: None,
      project_id: None,
      initiator: SmolStr::new("user"),
      header_initiator: None,
      route_mode_hint: None,
      headers: HeaderMap::new(),
      raw_body: raw,
      decoded_body: decoded,
      body_json: Arc::new(json!(null)),
      content_encoding: None,
    }
  }

  fn resolved() -> Resolved {
    Resolved {
      agent_id: None,
      model: SmolStr::new("m"),
      upstream_model: SmolStr::new("m"),
      upstream_endpoint: Endpoint::ChatCompletions,
      account_id: SmolStr::new("a"),
      provider_id: SmolStr::new("openai"),
      account_handle: crate::test_support::mock_handle("a", "openai"),
    }
  }

  #[tokio::test]
  async fn forwards_bytes_verbatim() {
    let raw = Bytes::from_static(b"\x1f\x8b\x08\x00not-json-just-bytes");
    let decoded = Bytes::from_static(b"{\"model\":\"m\"}");
    let out = PassthroughConvertRequest
      .convert_request(&ctx(), &extracted(raw.clone(), decoded.clone()), &resolved())
      .await
      .unwrap();
    assert_eq!(out.upstream_wire_body, raw);
    assert_eq!(out.debug_outbound_body, decoded);
    assert_eq!(*out.upstream_body, Value::Null, "upstream_body must be null sentinel");
  }
}
