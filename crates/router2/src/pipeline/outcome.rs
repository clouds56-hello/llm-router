//! Terminal outcome of a single [`PipelineRunner::run`] invocation.
//!
//! Mirrors the final [`StageEvent::Completed`] event but carries the typed
//! error (if any) so callers don't need to subscribe to the bus just to learn
//! the result.
//!
//! `built_headers` and `converted_request` are populated when the runner
//! completes the front-half (Extract → ConvertRequest) successfully. They let
//! callers inspect the outbound payload — useful for the gateway CLI smoke
//! command and dry-run flows — without having to drain the event bus.
//!
//! The back-half (`response`) will be added here in PR3b once the
//! Send/ConvertResponse stages produce real values.
//!
//! [`PipelineRunner::run`]: crate::pipeline::PipelineRunner::run
//! [`StageEvent::Completed`]: crate::event::StageEvent::Completed

use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{BuiltHeaders, ConvertedRequest, Resolved};

#[derive(Debug, Clone)]
pub struct PipelineOutcome {
  pub success: bool,
  pub attempts: u32,
  pub error: Option<PipelineError>,
  /// Resolve-stage output. `Some` once Resolve has run successfully.
  pub resolved: Option<Resolved>,
  /// BuildHeaders-stage output. `Some` once BuildHeaders has run successfully.
  pub built_headers: Option<BuiltHeaders>,
  /// ConvertRequest-stage output. `Some` once ConvertRequest has run
  /// successfully — for stop_before_send / dry-run callers this is the
  /// final outbound payload.
  pub converted_request: Option<ConvertedRequest>,
}

impl PipelineOutcome {
  pub fn success(attempts: u32) -> Self {
    Self {
      success: true,
      attempts,
      error: None,
      resolved: None,
      built_headers: None,
      converted_request: None,
    }
  }

  pub fn failure(attempts: u32, error: PipelineError) -> Self {
    Self {
      success: false,
      attempts,
      error: Some(error),
      resolved: None,
      built_headers: None,
      converted_request: None,
    }
  }

  pub fn with_resolved(mut self, resolved: Resolved) -> Self {
    self.resolved = Some(resolved);
    self
  }

  pub fn with_built_headers(mut self, headers: BuiltHeaders) -> Self {
    self.built_headers = Some(headers);
    self
  }

  pub fn with_converted_request(mut self, converted: ConvertedRequest) -> Self {
    self.converted_request = Some(converted);
    self
  }
}
