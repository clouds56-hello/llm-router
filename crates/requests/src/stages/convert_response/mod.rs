//! No-op ConvertResponse stage. Drops the upstream response and returns an
//! empty buffered placeholder. Pairs with [`NoopSend`](crate::stages::NoopSend);
//! only reachable when the back-half is wired but neither stub has been swapped
//! out yet.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ConvertResponseStage, ConvertedResponse};
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream::BoxStream;
use llm_core::provider::Endpoint;
use llm_headers::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

pub mod default;
pub use default::DefaultConvertResponse;

fn noop_buffered() -> ConvertedResponse {
  ConvertedResponse::Buffered {
    status: 0,
    headers: HeaderMap::new(),
    body_json: Arc::new(Value::Null),
    body_bytes: Bytes::new(),
  }
}

pub struct NoopConvertResponse;

#[async_trait]
impl ConvertResponseStage for NoopConvertResponse {
  async fn convert_buffered(
    &self,
    _ctx: &PipelineCtx,
    status: u16,
    _headers: HeaderMap,
    _upstream_endpoint: Endpoint,
    _body: Bytes,
  ) -> Result<ConvertedResponse, PipelineError> {
    let _ = status;
    Ok(noop_buffered())
  }

  async fn convert_stream(
    &self,
    _ctx: &PipelineCtx,
    status: u16,
    _headers: HeaderMap,
    _upstream_endpoint: Endpoint,
    _body: BoxStream<'static, std::io::Result<Bytes>>,
  ) -> Result<ConvertedResponse, PipelineError> {
    let _ = status;
    Ok(noop_buffered())
  }
}
