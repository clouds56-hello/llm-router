//! Composition of stages into a complete pipeline definition.
//!
//! A [`Profile`] holds an `Arc<dyn StageTrait>` for each of the six pipeline
//! slots. Profiles are immutable after construction; per-request behavior is
//! varied by the stage implementations themselves (e.g. a `RetrySend` that
//! wraps an inner `SendStage`). This composition-over-configuration approach
//! is the substitute for the per-stage hook abstraction discussed in
//! planning; see crate-level docs.
//!
//! Constructors:
//!
//! * [`Profile::full`] — all six slots provided. The intended production
//!   shape, used once the Send / ConvertResponse half lands in PR3.
//! * [`Profile::without_send`] — the first four stages are supplied; the
//!   Send and ConvertResponse slots are filled with no-op stages and the
//!   runner is instructed to stop after ConvertRequest and report success.
//!   This is the PR2 testbed for exercising the front five stages end-to-end
//!   before the real network call lands.

use crate::pipeline::stages::{
  BuildHeadersStage, ConvertRequestStage, ConvertResponseStage, ExtractStage, ResolveStage, SendStage,
};
use crate::stages::{NoopConvertResponse, NoopSend};
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
  /// When `true`, [`PipelineRunner`] stops after the ConvertRequest stage
  /// and reports success without touching the Send or ConvertResponse
  /// slots. Removed in PR3 once a real Send stage lands.
  ///
  /// [`PipelineRunner`]: crate::pipeline::PipelineRunner
  pub(crate) stop_before_send: bool,
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
      stop_before_send: false,
    }
  }

  /// PR2 constructor. Runs Extract → Resolve → BuildHeaders → ConvertRequest
  /// then short-circuits with success. The Send and ConvertResponse slots
  /// are filled with no-ops and never invoked.
  pub fn without_send(
    name: impl Into<SmolStr>,
    extract: Arc<dyn ExtractStage>,
    resolve: Arc<dyn ResolveStage>,
    build_headers: Arc<dyn BuildHeadersStage>,
    convert_request: Arc<dyn ConvertRequestStage>,
  ) -> Self {
    Self {
      name: name.into(),
      extract,
      resolve,
      build_headers,
      convert_request,
      send: Arc::new(NoopSend),
      convert_response: Arc::new(NoopConvertResponse),
      stop_before_send: true,
    }
  }
}
