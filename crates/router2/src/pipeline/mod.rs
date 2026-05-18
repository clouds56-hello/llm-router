//! Runner that drives a [`Profile`] through the 6-stage pipeline.
//!
//! Responsibilities:
//!
//! * Build a fresh [`PipelineCtx`] from the inbound [`RawInbound`].
//! * Emit [`StageEvent::Started`] before the first stage and
//!   [`StageEvent::Completed`] after the last (always).
//! * Run each stage; on success, emit the matching per-stage event carrying
//!   the stage's own output (cloned where the type permits); on failure,
//!   emit [`StageEvent::Error`] (with the stage/recoverable flag pulled
//!   verbatim from [`PipelineError`]) followed by `Completed { success: false }`
//!   and short-circuit.
//! * The runner can be configured (via [`RunnerOptions::stop_after`]) to
//!   short-circuit with success after a specific stage completes. This is
//!   how dry-run / smoke flows skip the Send half without needing a
//!   special-case profile constructor.
//!
//! The runner does not maintain a parallel "snapshot" — accumulated state
//! is exposed only via the returned [`PipelineOutcome`]. Subscribers that
//! need a running view fold the per-stage events themselves.
//!
//! Hooks are intentionally absent from PR1.
//!
//! [`Profile`]: crate::profile::Profile
//! [`StageEvent::Started`]: crate::event::StageEvent::Started
//! [`StageEvent::Completed`]: crate::event::StageEvent::Completed
//! [`StageEvent::Error`]: crate::event::StageEvent::Error

pub mod ctx;
pub mod error;
pub mod outcome;
pub mod stages;

use crate::event::{ConvertedResponseSummary, EventBus, SentSummary, Stage, StageEvent};
use crate::profile::Profile;
use ctx::PipelineCtx;
use error::PipelineError;
use outcome::PipelineOutcome;
use smol_str::SmolStr;
pub use stages::{
  BuildHeadersStage, BuiltHeaders, ConvertRequestStage, ConvertResponseStage, ConvertedRequest, ConvertedResponse,
  ExtractStage, Extracted, RawInbound, ResolveStage, Resolved, SendStage, SentResponse,
};
use std::sync::Arc;

/// Alias for clarity at call sites — the same type as [`PipelineRunner`].
pub type Pipeline = PipelineRunner;

/// Per-run configuration knobs for [`PipelineRunner`].
///
/// `stop_after` short-circuits the run with success once the named stage
/// completes — used by dry-run / smoke flows that want the front-half output
/// (BuildHeaders + ConvertRequest) without invoking Send.
#[derive(Debug, Clone, Default)]
pub struct RunnerOptions {
  pub stop_after: Option<Stage>,
}

impl RunnerOptions {
  pub fn stop_after(stage: Stage) -> Self {
    Self {
      stop_after: Some(stage),
    }
  }
}

pub struct PipelineRunner {
  pub profile: Arc<Profile>,
  pub events: Arc<EventBus>,
  pub options: RunnerOptions,
}

impl PipelineRunner {
  pub fn new(profile: Arc<Profile>, events: Arc<EventBus>) -> Self {
    Self {
      profile,
      events,
      options: RunnerOptions::default(),
    }
  }

  pub fn with_options(profile: Arc<Profile>, events: Arc<EventBus>, options: RunnerOptions) -> Self {
    Self {
      profile,
      events,
      options,
    }
  }

  pub async fn run(&self, raw: RawInbound) -> PipelineOutcome {
    let request_id = raw.request_id.clone().unwrap_or_else(|| SmolStr::new(uuid_like()));
    let ctx = PipelineCtx::new(request_id, raw.endpoint, self.events.clone());
    ctx.emit_known(StageEvent::Started { endpoint: raw.endpoint });

    // The outcome accumulator collects each stage's output so the caller
    // can inspect partial state on failure and the final state on success.
    let mut outcome = PipelineOutcome::success(0);

    // ---- Extract ----
    let extracted = match self.profile.extract.extract(&ctx, raw).await {
      Ok(e) => {
        // Wrap once in an Arc so both the event and downstream stage
        // calls share the same value without cloning the body.
        let arc = Arc::new(e);
        ctx.emit_known(StageEvent::Extract(arc.clone()));
        arc
      }
      Err(err) => return self.fail(&ctx, &mut outcome, err),
    };
    if self.options.stop_after == Some(Stage::Extract) {
      return self.complete(&ctx, outcome);
    }

    // ---- Resolve ----
    let resolved = match self.profile.resolve.resolve(&ctx, &extracted).await {
      Ok(r) => {
        outcome.resolved = Some(r.clone());
        ctx.emit_known(StageEvent::Resolve(r.clone()));
        r
      }
      Err(err) => return self.fail(&ctx, &mut outcome, err),
    };
    if self.options.stop_after == Some(Stage::Resolve) {
      return self.complete(&ctx, outcome);
    }

    // ---- BuildHeaders ----
    let headers = match self
      .profile
      .build_headers
      .build_headers(&ctx, &extracted, &resolved)
      .await
    {
      Ok(h) => {
        outcome.built_headers = Some(h.clone());
        ctx.emit_known(StageEvent::BuildHeaders(h.clone()));
        h
      }
      Err(err) => return self.fail(&ctx, &mut outcome, err),
    };
    if self.options.stop_after == Some(Stage::BuildHeaders) {
      return self.complete(&ctx, outcome);
    }

    // ---- ConvertRequest ----
    let converted = match self
      .profile
      .convert_request
      .convert_request(&ctx, &extracted, &resolved)
      .await
    {
      Ok(c) => {
        outcome.converted_request = Some(c.clone());
        ctx.emit_known(StageEvent::ConvertRequest(c.clone()));
        c
      }
      Err(err) => return self.fail(&ctx, &mut outcome, err),
    };
    if self.options.stop_after == Some(Stage::ConvertRequest) {
      return self.complete(&ctx, outcome);
    }

    // ---- Send ----
    let sent = match self
      .profile
      .send
      .send(&ctx, &extracted, &resolved, &headers, &converted)
      .await
    {
      Ok(s) => {
        // SentResponse owns a single-shot reqwest::Response; emit its
        // cloneable subset for observers and pass the full struct on to
        // ConvertResponse.
        ctx.emit_known(StageEvent::Send(SentSummary {
          status: s.status,
          headers: s.headers.clone(),
          upstream_endpoint: s.upstream_endpoint,
          stream: s.stream,
        }));
        s
      }
      Err(err) => return self.fail(&ctx, &mut outcome, err),
    };
    if self.options.stop_after == Some(Stage::Send) {
      outcome.sent_response = Some(sent);
      return self.complete(&ctx, outcome);
    }

    // ---- ConvertResponse ----
    let converted_response = match self.profile.convert_response.convert_response(&ctx, sent).await {
      Ok(c) => {
        // Build the summary before moving `c` into the outcome — body
        // (when buffered) is shared via the same Arc<Value>.
        let summary = ConvertedResponseSummary {
          status: c.status(),
          headers: c.headers().clone(),
          body: match &c {
            ConvertedResponse::Buffered { body_json, .. } => Some(body_json.clone()),
            ConvertedResponse::Stream { .. } => None,
          },
        };
        ctx.emit_known(StageEvent::ConvertResponse(summary));
        c
      }
      Err(err) => return self.fail(&ctx, &mut outcome, err),
    };

    outcome.success = true;
    outcome.attempts = ctx.attempt + 1;
    outcome.converted_response = Some(converted_response);
    self.complete(&ctx, outcome)
  }

  /// Emit [`StageEvent::Completed`] for a successful (or short-circuited)
  /// run and return the assembled outcome.
  fn complete(&self, ctx: &PipelineCtx, mut outcome: PipelineOutcome) -> PipelineOutcome {
    outcome.success = true;
    outcome.attempts = ctx.attempt + 1;
    ctx.emit_known(StageEvent::Completed {
      success: true,
      attempts: outcome.attempts,
    });
    outcome
  }

  /// Record the failure on `outcome`, emit [`StageEvent::Error`] +
  /// [`StageEvent::Completed { success: false }`], and return the
  /// partially-populated outcome to the caller. Subscribers that need
  /// the accumulated partial state read it from the returned outcome.
  fn fail(&self, ctx: &PipelineCtx, outcome: &mut PipelineOutcome, err: PipelineError) -> PipelineOutcome {
    outcome.success = false;
    outcome.attempts = ctx.attempt + 1;
    outcome.error = Some(err.clone());
    ctx.emit_known(StageEvent::Error {
      stage: err.stage,
      message: err.message.clone(),
      recoverable: err.recoverable,
    });
    ctx.emit_known(StageEvent::Completed {
      success: false,
      attempts: outcome.attempts,
    });
    // Move the populated outcome out by replacing it with a tombstone;
    // the caller's `&mut` reference is local to this method's parent
    // frame and dies immediately on return, so this is sound.
    std::mem::replace(outcome, PipelineOutcome::failure(0, err))
  }
}

/// Cheap unique-ish id without pulling in the `uuid` crate. The runner only
/// uses this when the caller did not supply a request id (tests, smoke
/// fixtures); production transports always populate `RawInbound.request_id`.
fn uuid_like() -> String {
  use std::sync::atomic::{AtomicU64, Ordering};
  static COUNTER: AtomicU64 = AtomicU64::new(0);
  let n = COUNTER.fetch_add(1, Ordering::Relaxed);
  let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .map(|d| d.as_nanos())
    .unwrap_or(0);
  format!("req-{ts:032x}-{n:08x}")
}

// `Stage` is re-exported at the crate root via `lib.rs`.
