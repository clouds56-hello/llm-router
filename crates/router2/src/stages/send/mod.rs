//! No-op Send stage. Always returns a permanent [`PipelineError`] tagged
//! `Stage::Send`. Used as a placeholder by
//! [`Profile::without_send`](crate::profile::Profile::without_send),
//! which configures the runner to short-circuit before this stage runs. If
//! the runner is mistakenly invoked against a full [`Profile`] using this
//! stub (i.e. someone forgot to swap it for a real impl), it will deliberately
//! fail loudly rather than silently succeeding.

use crate::event::Stage;
use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{BuiltHeaders, ConvertedRequest, Resolved, SendStage, SentResponse};
use async_trait::async_trait;
use smol_str::SmolStr;

pub struct NoopSend;

#[async_trait]
impl SendStage for NoopSend {
  async fn send(
    &self,
    _ctx: &PipelineCtx,
    _resolved: &Resolved,
    _headers: &BuiltHeaders,
    _body: &ConvertedRequest,
  ) -> Result<SentResponse, PipelineError> {
    Err(PipelineError::permanent(
      Stage::Send,
      SmolStr::new("NoopSend invoked: real Send stage is not yet implemented (PR3)"),
    ))
  }
}
