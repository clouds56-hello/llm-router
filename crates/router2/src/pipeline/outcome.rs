//! Terminal outcome of a single [`PipelineRunner::run`] invocation.
//!
//! Mirrors the final [`StageEvent::Completed`] event but carries the typed
//! error (if any) so callers don't need to subscribe to the bus just to learn
//! the result. The back-half output (response payload) will be added here in
//! PR2 once the Send/ConvertResponse stages produce real values.
//!
//! [`PipelineRunner::run`]: crate::pipeline::PipelineRunner::run
//! [`StageEvent::Completed`]: crate::event::StageEvent::Completed

use crate::pipeline::error::PipelineError;

#[derive(Debug, Clone)]
pub struct PipelineOutcome {
  pub success: bool,
  pub attempts: u32,
  pub error: Option<PipelineError>,
}

impl PipelineOutcome {
  pub fn success(attempts: u32) -> Self {
    Self {
      success: true,
      attempts,
      error: None,
    }
  }

  pub fn failure(attempts: u32, error: PipelineError) -> Self {
    Self {
      success: false,
      attempts,
      error: Some(error),
    }
  }
}
