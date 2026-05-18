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
//!   shape.
//! * [`Profile::without_send`] — convenience for dry-run / smoke flows: the
//!   Send and ConvertResponse slots are filled with no-ops. Combine with
//!   [`RunnerOptions::stop_after`](crate::pipeline::RunnerOptions::stop_after)
//!   pointing at [`Stage::ConvertRequest`](crate::event::Stage) to short-
//!   circuit the runner once the outbound request is fully built.

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
    }
  }

  /// Convenience constructor for dry-run / smoke flows. Fills the Send and
  /// ConvertResponse slots with no-op stages. Callers should pair this with
  /// [`RunnerOptions::stop_after(Stage::ConvertRequest)`](crate::pipeline::RunnerOptions::stop_after)
  /// so the runner short-circuits instead of invoking the no-ops.
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
    }
  }
}
