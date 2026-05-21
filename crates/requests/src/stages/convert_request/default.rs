//! Default production ConvertRequest stage.
//!
//! Mirrors the legacy `crates/router/src/pipeline/request.rs::prepare_request`
//! algorithm, decomposed into a single stage:
//!
//! 1. **Model rewrite** — overwrite `body.model` with the upstream model
//!    selected by Resolve.
//! 2. **Cross-endpoint convert** — when the inbound endpoint differs
//!    from the account's upstream endpoint, run `tokn_convert` to
//!    translate the JSON shape (e.g. Responses → Chat). Pass-through is
//!    free when both endpoints match.
//! 3. **Provider [`InputTransformer`]** — give the provider a final
//!    say (e.g. inject the `thinking` block for `glm-4.6`).
//! 4. **Serialize + re-encode** — produce `debug_outbound_body` (the
//!    uncompressed JSON, useful for logs and tests) and
//!    `upstream_wire_body` (re-compressed with the same codec the
//!    inbound used, when any). When the body hasn't changed and an
//!    encoding was present, we keep the original wire bytes to avoid
//!    a needless de/re-compress round-trip.
//!
//! Failures map to permanent [`PipelineError`]s — the upstream body
//! shape isn't going to change between retries.

use crate::event::Stage;
use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::{PipelineError, RequestsError};
use crate::pipeline::stages::{ConvertRequestStage, ConvertedRequest, Extracted, Resolved};
use crate::utils::codec::{encode_body_bytes, ContentEncodingKind};
use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;

pub struct DefaultConvertRequest;

#[async_trait]
impl ConvertRequestStage for DefaultConvertRequest {
  async fn convert_request(
    &self,
    ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
  ) -> Result<ConvertedRequest, PipelineError> {
    let mut upstream_body = rewrite_model(&extracted.body_json, resolved.upstream_model.as_str());

    if resolved.upstream_endpoint != ctx.endpoint {
      upstream_body = tokn_convert::convert_request(ctx.endpoint, resolved.upstream_endpoint, &upstream_body)
        .map_err(|source| perm(RequestsError::RequestConversion { source }))?;
    }

    if let Some(transformer) = resolved.account_handle.provider.input_transformer() {
      upstream_body = transformer
        .transform_input(resolved.upstream_endpoint, upstream_body)
        .map_err(|source| perm(RequestsError::ProviderInputTransformer { source }))?;
    }

    let debug_outbound_body = Bytes::from(
      serde_json::to_vec(&upstream_body).map_err(|source| perm(RequestsError::SerializeUpstreamBody { source }))?,
    );

    let unchanged = upstream_body == *extracted.body_json;
    let upstream_wire_body = if unchanged {
      // Reuse the original wire payload — preserves byte-for-byte
      // parity with whatever the client sent (including its
      // content-encoding) and avoids a needless re-compress.
      extracted.raw_body.clone()
    } else {
      maybe_encode(&debug_outbound_body, extracted.content_encoding)?
    };

    Ok(ConvertedRequest {
      upstream_body: Arc::new(upstream_body),
      upstream_wire_body,
      debug_outbound_body,
      content_encoding: extracted.content_encoding,
    })
  }
}

fn maybe_encode(body: &Bytes, encoding: Option<ContentEncodingKind>) -> Result<Bytes, PipelineError> {
  encode_body_bytes(body.as_ref(), encoding).map_err(|source| perm(RequestsError::ReencodeOutboundBody { source }))
}

fn rewrite_model(body: &Value, model: &str) -> Value {
  let mut body = body.clone();
  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".into(), Value::String(model.to_string()));
  }
  body
}

fn perm(source: RequestsError) -> PipelineError {
  PipelineError::permanent(Stage::ConvertRequest, source)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use crate::pipeline::ctx::PipelineCtx;
  use crate::pipeline::stages::{Extracted, Resolved};
  use crate::test_support::{mock_handle, mock_handle_with_provider, MockProvider};
  use crate::utils::codec::{decode_body_bytes, encode_body_bytes, ContentEncodingKind};
  use std::sync::Arc;
  use tokn_core::pipeline::InputTransformer;
  use tokn_core::provider::{Endpoint, Result as ProviderResult};
  use tokn_headers::HeaderMap;

  fn ctx_at(endpoint: Endpoint) -> PipelineCtx {
    PipelineCtx::new("req-cr", endpoint, Arc::new(EventBus::new(64)))
  }

  fn ctx() -> PipelineCtx {
    ctx_at(Endpoint::ChatCompletions)
  }

  fn extracted_with(body: Value, encoding: Option<ContentEncodingKind>, wire: Bytes) -> Extracted {
    Extracted {
      agent_id: None,
      model: smol_str::SmolStr::new("input-model"),
      stream: false,
      session_id: None,
      project_id: None,
      initiator: smol_str::SmolStr::new("user"),
      header_initiator: None,
      route_mode_hint: None,
      headers: HeaderMap::new(),
      raw_body: wire.clone(),
      decoded_body: Bytes::from(serde_json::to_vec(&body).unwrap()),
      body_json: Arc::new(body),
      content_encoding: encoding,
    }
  }

  fn resolved_with(
    handle: Arc<tokn_accounts::AccountHandle>,
    upstream_endpoint: Endpoint,
    upstream_model: &str,
  ) -> Resolved {
    Resolved {
      agent_id: None,
      model: smol_str::SmolStr::new("input-model"),
      upstream_model: smol_str::SmolStr::new(upstream_model),
      upstream_endpoint,
      account_id: smol_str::SmolStr::new("acct-1"),
      provider_id: smol_str::SmolStr::from(handle.provider.id()),
      account_handle: handle,
    }
  }

  #[tokio::test]
  async fn passthrough_when_endpoints_match_and_no_transformer() {
    let body = serde_json::json!({"model": "input-model", "messages": [{"role":"user","content":"hi"}]});
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let ex = extracted_with(body.clone(), None, raw.clone());
    let res = resolved_with(mock_handle("acct", "mock"), Endpoint::ChatCompletions, "input-model");

    let out = DefaultConvertRequest.convert_request(&ctx(), &ex, &res).await.unwrap();
    assert_eq!(*out.upstream_body, body);
    // Body unchanged → original wire bytes reused verbatim.
    assert_eq!(out.upstream_wire_body, raw);
    assert!(out.content_encoding.is_none());
  }

  #[tokio::test]
  async fn rewrites_model_field() {
    let body = serde_json::json!({"model": "input-model", "messages": []});
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let ex = extracted_with(body, None, raw);
    let res = resolved_with(
      mock_handle("acct", "mock"),
      Endpoint::ChatCompletions,
      "upstream-model-7",
    );

    let out = DefaultConvertRequest.convert_request(&ctx(), &ex, &res).await.unwrap();
    assert_eq!(out.upstream_body["model"], "upstream-model-7");
  }

  #[tokio::test]
  async fn runs_provider_input_transformer() {
    struct Stamp;
    impl InputTransformer for Stamp {
      fn transform_input(&self, _endpoint: Endpoint, mut body: Value) -> ProviderResult<Value> {
        if let Some(obj) = body.as_object_mut() {
          obj.insert("stamped".into(), Value::Bool(true));
        }
        Ok(body)
      }
    }
    let body = serde_json::json!({"model": "input-model"});
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let ex = extracted_with(body, None, raw);
    let handle = mock_handle_with_provider("acct", MockProvider::new("mock").with_transformer(Stamp));
    let res = resolved_with(handle, Endpoint::ChatCompletions, "input-model");

    let out = DefaultConvertRequest.convert_request(&ctx(), &ex, &res).await.unwrap();
    assert_eq!(out.upstream_body["stamped"], true);
  }

  #[tokio::test]
  async fn cross_endpoint_convert_runs_when_endpoints_differ() {
    // Responses → ChatCompletions. We don't assert on the exact shape
    // (that's `tokn_convert`'s responsibility) — just that the body
    // mutated and was re-serialized into `debug_outbound_body`.
    let body = serde_json::json!({
      "model": "input-model",
      "input": [{"role": "user", "content": "hi"}]
    });
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let ex = extracted_with(body.clone(), None, raw);
    let res = resolved_with(mock_handle("acct", "mock"), Endpoint::ChatCompletions, "input-model");

    let out = DefaultConvertRequest
      .convert_request(&ctx_at(Endpoint::Responses), &ex, &res)
      .await
      .unwrap();
    assert_ne!(
      *out.upstream_body, body,
      "expected cross-endpoint conversion to mutate body"
    );
    // wire body was re-serialized (not the original raw bytes).
    assert_eq!(out.upstream_wire_body, out.debug_outbound_body);
  }

  #[tokio::test]
  async fn gzip_round_trips_when_body_changes() {
    let body = serde_json::json!({"model": "input-model", "messages": []});
    let compressed = encode_body_bytes(
      serde_json::to_vec(&body).unwrap().as_slice(),
      Some(ContentEncodingKind::Gzip),
    )
    .unwrap();
    let ex = extracted_with(body, Some(ContentEncodingKind::Gzip), compressed);
    // Different upstream model → body mutates → we must re-compress.
    let res = resolved_with(
      mock_handle("acct", "mock"),
      Endpoint::ChatCompletions,
      "upstream-model-2",
    );

    let out = DefaultConvertRequest.convert_request(&ctx(), &ex, &res).await.unwrap();
    assert_eq!(out.content_encoding, Some(ContentEncodingKind::Gzip));
    let decoded = decode_body_bytes(out.upstream_wire_body.clone(), out.content_encoding).unwrap();
    let v: Value = serde_json::from_slice(&decoded).unwrap();
    assert_eq!(v["model"], "upstream-model-2");
  }

  #[tokio::test]
  async fn content_encoding_propagates_to_output() {
    let body = serde_json::json!({"model": "input-model"});
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let ex = extracted_with(body, Some(ContentEncodingKind::Zstd), raw);
    let res = resolved_with(mock_handle("acct", "mock"), Endpoint::ChatCompletions, "input-model");

    let out = DefaultConvertRequest.convert_request(&ctx(), &ex, &res).await.unwrap();
    assert_eq!(out.content_encoding, Some(ContentEncodingKind::Zstd));
  }

  #[tokio::test]
  async fn transformer_failure_is_permanent_stage_error() {
    struct Boom;
    impl InputTransformer for Boom {
      fn transform_input(&self, _endpoint: Endpoint, _body: Value) -> ProviderResult<Value> {
        Err(tokn_core::provider::error::Error::Profiles { message: "boom".into() })
      }
    }
    let body = serde_json::json!({"model": "input-model"});
    let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
    let ex = extracted_with(body, None, raw);
    let handle = mock_handle_with_provider("acct", MockProvider::new("mock").with_transformer(Boom));
    let res = resolved_with(handle, Endpoint::ChatCompletions, "input-model");

    let err = DefaultConvertRequest
      .convert_request(&ctx(), &ex, &res)
      .await
      .unwrap_err();
    assert_eq!(err.stage, Stage::ConvertRequest);
    assert!(!err.recoverable);
    assert!(err.message().contains("boom"));
  }
}
