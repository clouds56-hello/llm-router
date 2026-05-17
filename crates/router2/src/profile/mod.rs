//! Composition of stages into a complete pipeline definition.
//!
//! A [`Profile`] holds an `Arc<dyn StageTrait>` for each of the six pipeline
//! slots. Profiles are immutable after construction; per-request behavior is
//! varied by the stage implementations themselves (e.g. a `RetrySend` that
//! wraps an inner `SendStage`). This composition-over-configuration approach
//! is the substitute for the per-stage hook abstraction discussed in
//! planning; see crate-level docs.
//!
//! PR1 ships two constructors:
//!
//! * [`Profile::full`] — all six slots provided. The intended production
//!   shape, used once the back-half is implemented in PR2.
//! * [`Profile::partial_front_half`] — only Extract + Resolve provided; the
//!   four back-half slots are filled with no-op stages and the runner is
//!   instructed to stop after Resolve and report success. This is the only
//!   way to get a green end-to-end run in PR1.

use crate::pipeline::stages::{
  BuildHeadersStage, ConvertRequestStage, ConvertResponseStage, ExtractStage, ResolveStage, SendStage,
};
use crate::stages::{
  NoopBuildHeaders, NoopConvertRequest, NoopConvertResponse, NoopSend,
};
use smol_str::SmolStr;
use std::sync::Arc;

pub struct Profile {
  pub name: SmolStr,
  pub extract: Arc<dyn ExtractStage>,
  pub resolve: Arc<dyn ResolveStage>,
  pub build_headers: Arc<dyn BuildHeadersStage>,
  pub convert_request: Arc<dyn ConvertRequestStage>,
  pub send: Arc<dyn SendStage>,
  pub convert_response: Arc<dyn ConvertResponseStage>,
  /// When `true`, [`PipelineRunner`] stops after the Resolve stage and
  /// reports success. PR2 removes this flag along with [`Profile::partial_front_half`].
  ///
  /// [`PipelineRunner`]: crate::pipeline::PipelineRunner
  pub(crate) partial: bool,
}

impl Profile {
  pub fn full(
    name: impl Into<SmolStr>,
    extract: Arc<dyn ExtractStage>,
    resolve: Arc<dyn ResolveStage>,
    build_headers: Arc<dyn BuildHeadersStage>,
    convert_request: Arc<dyn ConvertRequestStage>,
    send: Arc<dyn SendStage>,
    convert_response: Arc<dyn ConvertResponseStage>,
  ) -> Self {
    Self {
      name: name.into(),
      extract,
      resolve,
      build_headers,
      convert_request,
      send,
      convert_response,
      partial: false,
    }
  }

  /// PR1-only constructor. Fills the back-half with no-op stages and sets
  /// the `partial` flag so the runner short-circuits after Resolve.
  pub fn partial_front_half(
    name: impl Into<SmolStr>,
    extract: Arc<dyn ExtractStage>,
    resolve: Arc<dyn ResolveStage>,
  ) -> Self {
    Self {
      name: name.into(),
      extract,
      resolve,
      build_headers: Arc::new(NoopBuildHeaders),
      convert_request: Arc::new(NoopConvertRequest),
      send: Arc::new(NoopSend),
      convert_response: Arc::new(NoopConvertResponse),
      partial: true,
    }
  }
}
