//! No-op ConvertRequest stage. Echoes the inbound body unchanged.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{ConvertRequestStage, ConvertedRequest, Extracted, Resolved};
use async_trait::async_trait;

pub struct NoopConvertRequest;

#[async_trait]
impl ConvertRequestStage for NoopConvertRequest {
  async fn convert_request(
    &self,
    _ctx: &PipelineCtx,
    extracted: &Extracted,
    _resolved: &Resolved,
  ) -> Result<ConvertedRequest, PipelineError> {
    Ok(ConvertedRequest {
      upstream_body: extracted.body_json.clone(),
      upstream_wire_body: extracted.decoded_body.clone(),
    })
  }
}
