//! Closed set of pipeline-observation events emitted by [`PipelineRunner`].
//!
//! Each per-stage variant carries the **stage's own output** directly. The
//! cheap-to-clone stage outputs (`Extracted` wrapped in `Arc`, `Resolved`,
//! `BuiltHeaders`, `ConvertedRequest`) are embedded as tuple-variant
//! payloads so subscribers read them with one destructure and the runner
//! reuses the same value it stores on `PipelineOutcome`.
//!
//! `Send` and `ConvertResponse` cannot embed their full outputs — the
//! upstream `reqwest::Response` and SSE `BoxStream` are single-shot — so
//! they expose the cloneable subset (status, headers, ...) via dedicated
//! struct variants.
//!
//! Terminal events (`Error`, `Completed`) carry only their own minimal
//! fields. Subscribers that need accumulated state either fold prior
//! per-stage events or read from the `PipelineOutcome` the runner returns
//! to the caller; the runner no longer maintains a parallel snapshot.
//!
//! [`PipelineRunner`]: crate::pipeline::PipelineRunner

use crate::pipeline::stages::{BuiltHeaders, ConvertedRequest, Extracted, Resolved};
use llm_headers::HeaderMap;
use llm_core::provider::Endpoint;
use serde_json::Value;
use smol_str::SmolStr;
use std::sync::Arc;

/// Identifies which pipeline stage produced an event. Used both as a tag on
/// success variants (implicit via the variant name) and as a field on
/// [`StageEvent::Error`] so subscribers can filter without pattern-matching
/// on N error variants.
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

/// Cloneable subset of [`SentResponse`](crate::pipeline::stages::SentResponse).
/// The full struct can't be cloned (it owns a single-shot `reqwest::Response`),
/// so the `Send` event carries this summary instead.
#[derive(Debug, Clone)]
pub struct SentSummary {
  pub status: u16,
  pub headers: HeaderMap,
  pub upstream_endpoint: Endpoint,
  /// Mirrors the inbound `stream` flag — true iff the client asked for SSE.
  pub stream: bool,
}

/// Cloneable subset of [`ConvertedResponse`](crate::pipeline::stages::ConvertedResponse).
/// `Buffered` shares the response's `Arc<Value>` body; `Stream` leaves
/// `body` as `None` because the live SSE byte stream is single-shot.
#[derive(Debug, Clone)]
pub struct ConvertedResponseSummary {
  pub status: u16,
  pub headers: HeaderMap,
  /// `Some` for buffered responses; `None` for streaming responses.
  pub body: Option<Arc<Value>>,
}

#[derive(Debug, Clone)]
pub enum StageEvent {
  /// Emitted once at the very start of [`PipelineRunner::run`], before any
  /// stage has produced output.
  ///
  /// [`PipelineRunner::run`]: crate::pipeline::PipelineRunner::run
  Started { endpoint: Endpoint },
  /// Extract stage completed successfully. Carries the full
  /// [`Extracted`] payload (wrapped in `Arc` so subscribers share the
  /// same body bytes / JSON without extra clones).
  Extract(Arc<Extracted>),
  /// Resolve stage completed successfully.
  Resolve(Resolved),
  /// BuildHeaders stage completed successfully.
  BuildHeaders(BuiltHeaders),
  /// ConvertRequest stage completed successfully.
  ConvertRequest(ConvertedRequest),
  /// Send stage completed successfully. The full
  /// [`SentResponse`](crate::pipeline::stages::SentResponse) is consumed by
  /// the next stage; observers receive the cloneable [`SentSummary`].
  Send(SentSummary),
  /// ConvertResponse stage completed successfully. Buffered bodies are
  /// shared via `Arc<Value>`; streaming bodies are not included on the
  /// event (the live byte stream is single-shot).
  ConvertResponse(ConvertedResponseSummary),
  /// Any stage failure. `recoverable` is propagated verbatim from the
  /// [`PipelineError`] returned by the stage; the runner does not infer
  /// it. Subscribers that need partial state read from the
  /// [`PipelineOutcome`] the runner returns to the caller, or fold prior
  /// per-stage events themselves.
  ///
  /// [`PipelineError`]: crate::pipeline::error::PipelineError
  /// [`PipelineOutcome`]: crate::pipeline::outcome::PipelineOutcome
  Error {
    stage: Stage,
    message: SmolStr,
    recoverable: bool,
  },
  /// Emitted once at the end of [`PipelineRunner::run`], success or
  /// failure.
  ///
  /// [`PipelineRunner::run`]: crate::pipeline::PipelineRunner::run
  Completed { success: bool, attempts: u32 },
}
