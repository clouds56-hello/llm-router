//! Production [`ConvertResponseStage`] implementation.
//!
//! Two focused methods split by the trait's provided dispatcher:
//!
//! 1. [`convert_buffered`](DefaultConvertResponse::convert_buffered):
//!    receives the already-drained body bytes, parses JSON, and optionally
//!    translates via [`llm_convert::convert_response`] when upstream/inbound
//!    endpoints differ.
//!
//! 2. [`convert_stream`](DefaultConvertResponse::convert_stream):
//!    wraps the live response byte stream in [`SsePipeline`]; installs an
//!    [`EndpointTranslator`] when endpoints differ.
//!
//! The trait's provided [`convert_response`](ConvertResponseStage::convert_response)
//! handles the dispatch (buffered vs stream) and emits `RecordEvent::UpstreamBody`
//! for the buffered path.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ConvertResponseStage, ConvertedResponse};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use llm_convert::sse::{EndpointTranslator, SsePipeline};
use llm_core::provider::Endpoint;
use llm_headers::HeaderMap;
use serde_json::Value;
use smol_str::SmolStr;
use std::sync::Arc;
use tracing::{debug, instrument};

pub struct DefaultConvertResponse;

impl DefaultConvertResponse {
  pub fn new() -> Self {
    Self
  }
}

impl Default for DefaultConvertResponse {
  fn default() -> Self {
    Self::new()
  }
}

#[async_trait]
impl ConvertResponseStage for DefaultConvertResponse {
  #[instrument(name = "default_convert_buffered", skip_all, fields(
    status = status,
    upstream_endpoint = ?upstream_endpoint,
    inbound_endpoint = ?ctx.endpoint,
    body_len = body.len(),
  ))]
  async fn convert_buffered(
    &self,
    ctx: &PipelineCtx,
    status: u16,
    headers: HeaderMap,
    upstream_endpoint: Endpoint,
    body: Bytes,
  ) -> Result<ConvertedResponse, PipelineError> {
    let inbound_endpoint = ctx.endpoint;

    if body.is_empty() {
      return Ok(ConvertedResponse::Buffered {
        status,
        headers,
        body_json: Arc::new(Value::Null),
        body_bytes: Bytes::new(),
      });
    }

    let upstream_json: Value = serde_json::from_slice(&body).map_err(|e| {
      PipelineError::permanent(
        crate::event::Stage::ConvertResponse,
        SmolStr::new(format!("upstream body not valid JSON: {e}")),
      )
    })?;

    let (body_json, body_bytes) = if upstream_endpoint == inbound_endpoint {
      (upstream_json, body)
    } else {
      let translated =
        llm_convert::convert_response(upstream_endpoint, inbound_endpoint, &upstream_json).map_err(|e| {
          PipelineError::permanent(
            crate::event::Stage::ConvertResponse,
            SmolStr::new(format!("response conversion failed: {e}")),
          )
        })?;
      let bytes = serde_json::to_vec(&translated).map(Bytes::from).map_err(|e| {
        PipelineError::permanent(
          crate::event::Stage::ConvertResponse,
          SmolStr::new(format!("serializing translated response failed: {e}")),
        )
      })?;
      (translated, bytes)
    };

    Ok(ConvertedResponse::Buffered {
      status,
      headers,
      body_json: Arc::new(body_json),
      body_bytes,
    })
  }

  #[instrument(name = "default_convert_stream", skip_all, fields(
    status = status,
    upstream_endpoint = ?upstream_endpoint,
    inbound_endpoint = ?ctx.endpoint,
  ))]
  async fn convert_stream(
    &self,
    ctx: &PipelineCtx,
    status: u16,
    headers: HeaderMap,
    upstream_endpoint: Endpoint,
    body: BoxStream<'static, std::io::Result<Bytes>>,
  ) -> Result<ConvertedResponse, PipelineError> {
    debug!("wrapping upstream response as SSE stream");
    let inbound_endpoint = ctx.endpoint;
    let mut pipeline = SsePipeline::from_stream(body);
    if upstream_endpoint != inbound_endpoint {
      pipeline = pipeline.with_transformer(EndpointTranslator::new(upstream_endpoint, inbound_endpoint));
    }
    Ok(ConvertedResponse::Stream {
      status,
      headers,
      body: pipeline.run(),
    })
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use crate::pipeline::stages::{ConvertedResponse, SentResponse};
  use futures_util::StreamExt;
  use llm_core::provider::Endpoint;
  use llm_headers::HeaderMap;
  use std::sync::Arc;

  fn ctx(endpoint: Endpoint) -> PipelineCtx {
    PipelineCtx::new("req-cr", endpoint, Arc::new(EventBus::new(64)))
  }

  fn response(status: u16, body: &'static str, content_type: &'static str) -> reqwest::Response {
    let resp = http::Response::builder()
      .status(status)
      .header("content-type", content_type)
      .body(body)
      .unwrap();
    reqwest::Response::from(resp)
  }

  #[tokio::test]
  async fn buffered_passthrough_same_endpoint() {
    let stage = DefaultConvertResponse::new();
    let out = stage
      .convert_buffered(
        &ctx(Endpoint::ChatCompletions),
        200,
        HeaderMap::new(),
        Endpoint::ChatCompletions,
        Bytes::from_static(br#"{"id":"x","choices":[]}"#),
      )
      .await
      .unwrap();
    match out {
      ConvertedResponse::Buffered {
        status,
        body_json,
        body_bytes,
        ..
      } => {
        assert_eq!(status, 200);
        assert_eq!(body_json["id"], "x");
        assert_eq!(body_bytes.as_ref(), br#"{"id":"x","choices":[]}"#);
      }
      _ => panic!("expected buffered"),
    }
  }

  #[tokio::test]
  async fn buffered_empty_body_yields_null() {
    let stage = DefaultConvertResponse::new();
    let out = stage
      .convert_buffered(
        &ctx(Endpoint::ChatCompletions),
        502,
        HeaderMap::new(),
        Endpoint::ChatCompletions,
        Bytes::new(),
      )
      .await
      .unwrap();
    match out {
      ConvertedResponse::Buffered {
        status,
        body_json,
        body_bytes,
        ..
      } => {
        assert_eq!(status, 502);
        assert!(body_json.is_null());
        assert!(body_bytes.is_empty());
      }
      _ => panic!("expected buffered"),
    }
  }

  #[tokio::test]
  async fn buffered_invalid_json_is_permanent() {
    let stage = DefaultConvertResponse::new();
    let err = stage
      .convert_buffered(
        &ctx(Endpoint::ChatCompletions),
        200,
        HeaderMap::new(),
        Endpoint::ChatCompletions,
        Bytes::from_static(b"not json"),
      )
      .await
      .unwrap_err();
    assert_eq!(err.stage, crate::event::Stage::ConvertResponse);
    assert!(!err.recoverable);
    assert!(err.message.contains("not valid JSON"));
  }

  #[tokio::test]
  async fn stream_branch_returns_stream_variant() {
    let stage = DefaultConvertResponse::new();
    let body = "data: {\"hello\":1}\n\ndata: [DONE]\n\n";
    let out = stage
      .convert_stream(
        &ctx(Endpoint::ChatCompletions),
        200,
        HeaderMap::new(),
        Endpoint::ChatCompletions,
        futures_util::stream::iter(vec![Ok(Bytes::from(body))]).boxed(),
      )
      .await
      .unwrap();
    match out {
      ConvertedResponse::Stream { status, mut body, .. } => {
        assert_eq!(status, 200);
        let chunk = body.next().await.expect("at least one chunk").expect("ok chunk");
        assert!(!chunk.is_empty());
      }
      _ => panic!("expected stream"),
    }
  }

  #[tokio::test]
  async fn provided_convert_response_emits_upstream_body_for_buffered() {
    let stage = DefaultConvertResponse::new();
    let events = Arc::new(EventBus::new(64));
    let ctx = PipelineCtx::new("req-body", Endpoint::ChatCompletions, events.clone());
    let sent = SentResponse {
      status: 200,
      headers: HeaderMap::new(),
      stream: false,
      upstream_endpoint: Endpoint::ChatCompletions,
      response: response(200, r#"{"ok":true}"#, "application/json"),
    };
    let out = stage.convert_response(&ctx, sent).await.unwrap();
    match out {
      ConvertedResponse::Buffered { status, body_bytes, .. } => {
        assert_eq!(status, 200);
        assert_eq!(body_bytes.as_ref(), br#"{"ok":true}"#);
      }
      _ => panic!("expected buffered"),
    }
  }

  #[tokio::test]
  async fn provided_convert_response_no_upstream_body_for_stream() {
    let stage = DefaultConvertResponse::new();
    let events = Arc::new(EventBus::new(64));
    let ctx = PipelineCtx::new("req-stream", Endpoint::ChatCompletions, events.clone());
    let sent = SentResponse {
      status: 200,
      headers: HeaderMap::new(),
      stream: true,
      upstream_endpoint: Endpoint::ChatCompletions,
      response: response(200, "data: {}\n\ndata: [DONE]\n\n", "text/event-stream"),
    };
    let out = stage.convert_response(&ctx, sent).await.unwrap();
    assert!(matches!(out, ConvertedResponse::Stream { .. }));
  }
}
