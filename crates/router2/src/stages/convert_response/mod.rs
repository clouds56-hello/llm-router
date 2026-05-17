//! No-op ConvertResponse stage. Drops the upstream response and returns an
//! empty placeholder. Pairs with [`NoopSend`](crate::stages::NoopSend); only
//! reachable when the back-half is wired but neither stub has been swapped
//! out yet.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ConvertResponseStage, ConvertedResponse, SentResponse};
use async_trait::async_trait;

pub struct NoopConvertResponse;

#[async_trait]
impl ConvertResponseStage for NoopConvertResponse {
  async fn convert_response(
    &self,
    _ctx: &PipelineCtx,
    _sent: SentResponse,
  ) -> Result<ConvertedResponse, PipelineError> {
    Ok(ConvertedResponse)
  }
}
