//! Closed set of pipeline-observation events emitted by [`PipelineRunner`].
//!
//! Successful stage completions get a per-stage variant (`Extract`, `Resolve`,
//! ...). Failures are funneled through a single [`StageEvent::Error`] variant
//! tagged with the originating [`Stage`] so subscribers can filter without
//! pattern-matching on N error variants.
//!
//! Back-half variants (`BuildHeaders`, `ConvertRequest`, `Send`,
//! `ConvertResponse`) are declared in PR1 but never emitted until the
//! corresponding stage impls land.
//!
//! [`PipelineRunner`]: crate::pipeline::PipelineRunner

use llm_core::provider::Endpoint;
use llm_core::ClientId;
use smol_str::SmolStr;

/// Identifies which pipeline stage produced an event. Used both as a tag on
/// success variants and as a field on [`StageEvent::Error`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
  Extract,
  Resolve,
  BuildHeaders,
  ConvertRequest,
  Send,
  ConvertResponse,
}

impl Stage {
  pub fn as_str(self) -> &'static str {
    match self {
      Stage::Extract => "extract",
      Stage::Resolve => "resolve",
      Stage::BuildHeaders => "build_headers",
      Stage::ConvertRequest => "convert_request",
      Stage::Send => "send",
      Stage::ConvertResponse => "convert_response",
    }
  }
}

impl std::fmt::Display for Stage {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

#[derive(Debug, Clone)]
pub enum StageEvent {
  /// Emitted once at the very start of [`PipelineRunner::run`].
  Started {
    endpoint: Endpoint,
  },
  /// Extract stage completed successfully.
  Extract {
    client_id: Option<ClientId>,
    model: SmolStr,
    stream: bool,
  },
  /// Resolve stage completed successfully.
  Resolve {
    client_id: Option<ClientId>,
    model: SmolStr,
    upstream_model: SmolStr,
    account_id: SmolStr,
    provider_id: SmolStr,
    upstream_endpoint: Endpoint,
  },
  /// Reserved for PR2.
  BuildHeaders,
  /// Reserved for PR2.
  ConvertRequest,
  /// Reserved for PR2.
  Send,
  /// Reserved for PR2.
  ConvertResponse,
  /// Any stage failure. `recoverable` is propagated verbatim from the
  /// [`PipelineError`] returned by the stage; the runner does not infer it.
  ///
  /// [`PipelineError`]: crate::pipeline::error::PipelineError
  Error {
    stage: Stage,
    message: SmolStr,
    recoverable: bool,
  },
  /// Emitted once at the end of [`PipelineRunner::run`], success or failure.
  Completed {
    success: bool,
    attempts: u32,
  },
}
