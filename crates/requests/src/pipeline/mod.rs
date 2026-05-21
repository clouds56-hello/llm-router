//! Runner that drives a [`Profile`] through the 6-stage pipeline.
//!
//! Responsibilities:
//!
//! * Build a fresh [`PipelineCtx`] from the inbound [`RawInbound`].
//! * Emit [`StageEvent::Started`] before the first stage and
//!   [`StageEvent::Completed`] after the last (always).
//! * Run each stage; on success, emit the matching per-stage event carrying
//!   the stage's own output (cloned where the type permits); on failure,
//!   emit [`StageEvent::Error`] (with the stage / recoverable / stop flags
//!   pulled verbatim from [`PipelineError`]) followed by `Completed { success
//!   = !err.stop }` and short-circuit.
//! * Return `Result<ConvertedResponse, PipelineError>` to the caller. The
//!   runner does not retain partial state — subscribers that need the
//!   per-stage outputs read them off the [`EventBus`] events, which carry
//!   each stage's own output as the payload.
//! * A stage may halt the pipeline deliberately by returning
//!   `PipelineError::stop(...)`. This is the mechanism used by dry-run
//!   profiles (e.g. [`NoopSend`](crate::stages::NoopSend)): the runner
//!   still emits Error + Completed, but the caller can branch on
//!   `err.stop` to render a successful early-termination report instead
//!   of a failure.
//!
//! Hooks are intentionally absent from PR1.
//!
//! [`Profile`]: crate::profile::Profile
//! [`StageEvent::Started`]: crate::event::StageEvent::Started
//! [`StageEvent::Completed`]: crate::event::StageEvent::Completed
//! [`StageEvent::Error`]: crate::event::StageEvent::Error
//! [`EventBus`]: crate::event::EventBus

pub mod config;
pub mod ctx;
pub mod error;
pub mod stages;

use crate::event::{EventBus, StageEvent};
use crate::profile::Profile;
pub use config::{RunConfig, RunConfigBuilder};
use ctx::PipelineCtx;
use error::PipelineError;
use smol_str::SmolStr;
pub use stages::{
  BuildHeadersStage, BuiltHeaders, ConvertRequestStage, ConvertResponseStage, ConvertedRequest, ConvertedResponse,
  ExtractStage, Extracted, RawInbound, ResolveStage, Resolved, SendStage, SentResponse,
};
use std::sync::Arc;
use std::time::Duration;

/// Alias for clarity at call sites — the same type as [`PipelineRunner`].
pub type Pipeline = PipelineRunner;

pub struct PipelineRunner {
  pub profile: Arc<Profile>,
  pub events: Arc<EventBus>,
  retry_policy: RetryPolicy,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RetryPolicy {
  pub max_retries: u32,
  pub initial_backoff: Duration,
}

impl RetryPolicy {
  pub const fn new(max_retries: u32, initial_backoff: Duration) -> Self {
    Self {
      max_retries,
      initial_backoff,
    }
  }

  fn should_retry(&self, attempt: u32, err: &PipelineError) -> bool {
    attempt < self.max_retries && err.stage == crate::event::Stage::Send && err.recoverable && !err.stop
  }

  fn backoff_for(&self, attempt: u32) -> Duration {
    if self.initial_backoff.is_zero() {
      return Duration::ZERO;
    }
    let multiplier = 1u32.checked_shl(attempt.min(31)).unwrap_or(u32::MAX);
    self.initial_backoff.saturating_mul(multiplier)
  }
}

impl PipelineRunner {
  pub fn new(profile: Arc<Profile>, events: Arc<EventBus>) -> Self {
    Self::new_with_retry(profile, events, RetryPolicy::default())
  }

  pub fn new_with_retry(profile: Arc<Profile>, events: Arc<EventBus>, retry_policy: RetryPolicy) -> Self {
    Self {
      profile,
      events,
      retry_policy,
    }
  }

  /// Drive the pipeline through all six stages. Returns the final
  /// [`ConvertedResponse`] on success or the originating [`PipelineError`]
  /// on failure. Callers that need partial state (per-stage outputs)
  /// subscribe to the [`EventBus`] — each per-stage `StageEvent` variant
  /// carries that stage's own output.
  ///
  /// A `PipelineError` with `stop == true` indicates a deliberate early
  /// termination (e.g. dry-run); it is still returned as `Err` but should
  /// not be reported as a failure.
  pub async fn run(&self, raw: RawInbound) -> Result<ConvertedResponse, PipelineError> {
    self.run_with(raw, RunConfig::default()).await
  }

  /// Same as [`run`](Self::run) but with a caller-supplied [`RunConfig`]
  /// bag. The bag is attached to [`PipelineCtx::config`] (wrapped in an
  /// `Arc`) and is visible to every stage via `ctx.config`. Used by
  /// secondary pipeline variants (proxy passthrough) that thread
  /// transport-level hints down to custom Resolve / Send stages.
  pub async fn run_with(&self, raw: RawInbound, config: RunConfig) -> Result<ConvertedResponse, PipelineError> {
    let request_id = raw.request_id.clone().unwrap_or_else(|| SmolStr::new(uuid_like()));
    let config = Arc::new(config);
    let mut attempt = 0;

    loop {
      match self
        .run_attempt(raw.clone(), config.clone(), request_id.clone(), attempt)
        .await
      {
        Ok(response) => return Ok(response),
        Err(err) if self.retry_policy.should_retry(attempt, &err) => {
          let backoff = self.retry_policy.backoff_for(attempt);
          if !backoff.is_zero() {
            tokio::time::sleep(backoff).await;
          }
          attempt += 1;
        }
        Err(err) => return Err(err),
      }
    }
  }

  async fn run_attempt(
    &self,
    raw: RawInbound,
    config: Arc<RunConfig>,
    request_id: SmolStr,
    attempt: u32,
  ) -> Result<ConvertedResponse, PipelineError> {
    let ctx = PipelineCtx::new_with_attempt_and_config(request_id, attempt, raw.endpoint, self.events.clone(), config);
    ctx.emit_stage(StageEvent::Started {
      endpoint: raw.endpoint.into(),
    });

    // ---- Extract ----
    let extracted = match self.profile.extract.extract(&ctx, raw).await {
      Ok(e) => {
        ctx.emit_stage(StageEvent::Extract((&e).into()));
        e
      }
      Err(err) => return Err(self.fail(&ctx, err)),
    };

    // ---- Resolve ----
    let resolved = match self.profile.resolve.resolve(&ctx, &extracted).await {
      Ok(r) => {
        ctx.emit_stage(StageEvent::Resolve((&r).into()));
        r
      }
      Err(err) => return Err(self.fail(&ctx, err)),
    };

    // ---- BuildHeaders ----
    let headers = match self
      .profile
      .build_headers
      .build_headers(&ctx, &extracted, &resolved)
      .await
    {
      Ok(h) => {
        ctx.emit_stage(StageEvent::BuildHeaders((&h).into()));
        h
      }
      Err(err) => return Err(self.fail(&ctx, err)),
    };

    // ---- ConvertRequest ----
    let converted = match self
      .profile
      .convert_request
      .convert_request(&ctx, &extracted, &resolved)
      .await
    {
      Ok(c) => {
        ctx.emit_stage(StageEvent::ConvertRequest((&c).into()));
        c
      }
      Err(err) => return Err(self.fail(&ctx, err)),
    };

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
        ctx.emit_stage(StageEvent::Send((&s).into()));
        s
      }
      Err(err) => return Err(self.fail(&ctx, err)),
    };

    // ---- ConvertResponse ----
    let converted_response = match self.profile.convert_response.convert_response(&ctx, sent).await {
      Ok(c) => {
        // Build the summary before moving `c` to the caller — body (when
        // buffered) is shared via the same Arc<Value>.
        ctx.emit_stage(StageEvent::ConvertResponse((&c).into()));
        c
      }
      Err(err) => return Err(self.fail(&ctx, err)),
    };

    match &converted_response.body {
      stages::ConvertedBody::Buffered { body_bytes, .. } => {
        ctx.emit_record(llm_core::request_event::RecordEvent::ConvertedBody {
          body: body_bytes.clone(),
          error: None,
        });
        ctx.emit_stage(StageEvent::Completed {
          success: true,
          attempts: ctx.attempt + 1,
        });
      }
      stages::ConvertedBody::Stream { .. } => {}
    }
    Ok(converted_response)
  }

  /// Emit [`StageEvent::Error`] + [`StageEvent::Completed`] for the given
  /// error and return it. `Completed.success` is always `false` here —
  /// even a deliberate stop did not produce a `ConvertedResponse`.
  /// Subscribers that need to distinguish a stop from a real failure read
  /// the preceding `Error` event's `stop` flag.
  fn fail(&self, ctx: &PipelineCtx, err: PipelineError) -> PipelineError {
    ctx.emit_stage(StageEvent::Error {
      stage: err.stage,
      message: SmolStr::new(err.message().as_ref()),
      recoverable: err.recoverable,
      stop: err.stop,
    });
    ctx.emit_stage(StageEvent::Completed {
      success: false,
      attempts: ctx.attempt + 1,
    });
    err
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
