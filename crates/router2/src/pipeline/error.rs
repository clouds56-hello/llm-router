//! Pipeline error type. Stages are responsible for constructing this with the
//! correct [`Stage`] tag and `recoverable` flag; the runner emits a matching
//! [`StageEvent::Error`] verbatim from these fields.
//!
//! [`StageEvent::Error`]: crate::event::StageEvent::Error

use crate::event::Stage;
use smol_str::SmolStr;

#[derive(Debug, Clone)]
pub struct PipelineError {
  pub stage: Stage,
  pub message: SmolStr,
  /// `true` iff a retry-style decorator may sensibly re-invoke the stage.
  /// Permanent failures (bad request, 4xx, unknown model) set this to
  /// `false`.
  pub recoverable: bool,
}

impl PipelineError {
  pub fn new(stage: Stage, message: impl Into<SmolStr>, recoverable: bool) -> Self {
    Self {
      stage,
      message: message.into(),
      recoverable,
    }
  }

  pub fn permanent(stage: Stage, message: impl Into<SmolStr>) -> Self {
    Self::new(stage, message, false)
  }

  pub fn recoverable(stage: Stage, message: impl Into<SmolStr>) -> Self {
    Self::new(stage, message, true)
  }
}

impl std::fmt::Display for PipelineError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    write!(f, "[{}] {}", self.stage, self.message)
  }
}

impl std::error::Error for PipelineError {}
