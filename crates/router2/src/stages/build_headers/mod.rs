//! No-op BuildHeaders stage. Returns an empty header set. Used by
//! [`Profile::partial_front_half`](crate::profile::Profile::partial_front_half)
//! to keep the runner type-checking before PR2 lands the real impl.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{BuildHeadersStage, BuiltHeaders, Extracted, Resolved};
use async_trait::async_trait;

pub struct NoopBuildHeaders;

#[async_trait]
impl BuildHeadersStage for NoopBuildHeaders {
  async fn build_headers(
    &self,
    _ctx: &PipelineCtx,
    _extracted: &Extracted,
    _resolved: &Resolved,
  ) -> Result<BuiltHeaders, PipelineError> {
    Ok(BuiltHeaders::default())
  }
}
