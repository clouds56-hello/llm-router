//! Runner that drives a [`Profile`] through the 6-stage pipeline.
//!
//! Responsibilities:
//!
//! * Build a fresh [`PipelineCtx`] from the inbound [`RawInbound`].
//! * Emit [`StageEvent::Started`] before the first stage and
//!   [`StageEvent::Completed`] after the last (always).
//! * Run each stage; on success, emit the matching per-stage event; on
//!   failure, emit [`StageEvent::Error`] (with the stage/recoverable flag
//!   pulled verbatim from [`PipelineError`]) and short-circuit.
//! * In PR2, the runner can be configured (via [`Profile::stop_before_send`])
//!   to stop after the ConvertRequest stage and report success. This mode
//!   exists so the front five stages are exercisable end-to-end before the
//!   real Send / ConvertResponse pair lands in PR3.
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

use crate::event::{EventBus, StageEvent};
use crate::profile::Profile;
use ctx::PipelineCtx;
use error::PipelineError;
use outcome::PipelineOutcome;
pub use stages::{
  BuildHeadersStage, BuiltHeaders, ConvertRequestStage, ConvertResponseStage, ConvertedRequest,
  ConvertedResponse, ExtractStage, Extracted, RawInbound, ResolveStage, Resolved, SendStage, SentResponse,
};
use smol_str::SmolStr;
use std::sync::Arc;

/// Alias for clarity at call sites — the same type as [`PipelineRunner`].
pub type Pipeline = PipelineRunner;

pub struct PipelineRunner {
  pub profile: Arc<Profile>,
  pub events: Arc<EventBus>,
}

impl PipelineRunner {
  pub fn new(profile: Arc<Profile>, events: Arc<EventBus>) -> Self {
    Self { profile, events }
  }

  pub async fn run(&self, raw: RawInbound) -> PipelineOutcome {
    let request_id = raw
      .request_id
      .clone()
      .unwrap_or_else(|| SmolStr::new(uuid_like()));
    let ctx = PipelineCtx::new(request_id, self.events.clone());
    ctx.emit_known(StageEvent::Started { endpoint: raw.endpoint });

    // ---- Extract ----
    let extracted = match self.profile.extract.extract(&ctx, raw).await {
      Ok(e) => {
        ctx.emit_known(StageEvent::Extract {
          client_id: e.client_id.clone(),
          model: e.model.clone(),
          stream: e.stream,
        });
        e
      }
      Err(err) => return self.fail(&ctx, err),
    };

    // ---- Resolve ----
    let resolved = match self.profile.resolve.resolve(&ctx, &extracted).await {
      Ok(r) => {
        ctx.emit_known(StageEvent::Resolve {
          client_id: r.client_id.clone(),
          model: r.model.clone(),
          upstream_model: r.upstream_model.clone(),
          account_id: r.account_id.clone(),
          provider_id: r.provider_id.clone(),
          upstream_endpoint: r.upstream_endpoint,
        });
        r
      }
      Err(err) => return self.fail(&ctx, err),
    };

    // ---- BuildHeaders ----
    let headers = match self
      .profile
      .build_headers
      .build_headers(&ctx, &extracted, &resolved)
      .await
    {
      Ok(h) => {
        ctx.emit_known(StageEvent::BuildHeaders);
        h
      }
      Err(err) => return self.fail(&ctx, err),
    };

    // ---- ConvertRequest ----
    let converted = match self
      .profile
      .convert_request
      .convert_request(&ctx, &extracted, &resolved)
      .await
    {
      Ok(c) => {
        ctx.emit_known(StageEvent::ConvertRequest);
        c
      }
      Err(err) => return self.fail(&ctx, err),
    };

    // PR2 short-circuit: a `stop_before_send` profile signals that the Send
    // half is intentionally stubbed and the run should report success after
    // ConvertRequest. Removed in PR3 once a real Send stage lands.
    if self.profile.stop_before_send {
      let _ = (&headers, &converted); // PR3 consumes these.
      ctx.emit_known(StageEvent::Completed {
        success: true,
        attempts: ctx.attempt + 1,
      });
      return PipelineOutcome::success(ctx.attempt + 1);
    }

    // ---- Send ----
    let sent = match self.profile.send.send(&ctx, &resolved, &headers, &converted).await {
      Ok(s) => {
        ctx.emit_known(StageEvent::Send);
        s
      }
      Err(err) => return self.fail(&ctx, err),
    };

    // ---- ConvertResponse ----
    let _converted_response = match self.profile.convert_response.convert_response(&ctx, sent).await {
      Ok(c) => {
        ctx.emit_known(StageEvent::ConvertResponse);
        c
      }
      Err(err) => return self.fail(&ctx, err),
    };

    ctx.emit_known(StageEvent::Completed {
      success: true,
      attempts: ctx.attempt + 1,
    });
    PipelineOutcome::success(ctx.attempt + 1)
  }

  fn fail(&self, ctx: &PipelineCtx, err: PipelineError) -> PipelineOutcome {
    ctx.emit_known(StageEvent::Error {
      stage: err.stage,
      message: err.message.clone(),
      recoverable: err.recoverable,
    });
    ctx.emit_known(StageEvent::Completed {
      success: false,
      attempts: ctx.attempt + 1,
    });
    PipelineOutcome::failure(ctx.attempt + 1, err)
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
