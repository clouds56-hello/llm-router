//! Event payload types for the `tokn-requests` pipeline.
//!
//! These types live in `tokn-core` so that the workspace's
//! [`tokn_core::event::Event`] enum can embed a [`Requests(RequestEvent)`]
//! variant without inverting the dep graph (requests already depends on
//! tokn-core).
//!
//! Three payload shapes are supported as peers under
//! [`RequestEventPayload`]:
//!
//! * [`StageEvent`] — a closed enum of lifecycle/observation variants
//!   the runner emits at well-defined points (Started, per-stage
//!   summaries, Error, Completed). Subscribers `match` on them.
//! * [`RecordEvent`] — transport-adjacent captures that sit alongside the
//!   stage lifecycle (inbound connection facts, outbound wire-truth,
//!   parsed usage). Split from `StageEvent` so subscribers that only care
//!   about one axis don't pay a match-arm tax for the other.
//! * [`CustomEvent`] — an `Any`-typed escape hatch for middleware /
//!   decorator stages (e.g. retry, cache) to publish their own
//!   structured records without modifying either of the closed enums.
//!
//! The event bus itself is `tokn_core::event::EventBus` (a tokio broadcast
//! channel); requests publishes `tokn_core::event::Event::Requests(RequestEvent
//! { ... })` directly onto it.
//!
//! [`Requests(RequestEvent)`]: crate::event::Event::Requests

pub mod record;
pub mod stage;

pub use record::RecordEvent;
pub use stage::{
  BuiltHeadersSummary, ConvertedRequestSummary, ConvertedResponseSummary, EndpointLabel, ExtractedSummary,
  ResolvedSummary, SentSummary, Stage, StageEvent,
};

use smol_str::SmolStr;
use std::any::Any;
use std::sync::Arc;

/// A single requests pipeline event. Carries the per-request bookkeeping
/// (request_id, attempt, ts) plus a typed or `Any`-typed payload.
/// `ts` is a millisecond-precision unix timestamp captured at emission time.
#[derive(Clone, Debug)]
pub struct RequestEvent {
  pub request_id: SmolStr,
  pub attempt: u32,
  pub ts: i64,
  pub payload: RequestEventPayload,
}

/// One of three payload shapes carried on a [`RequestEvent`].
///
/// - [`Stage`](RequestEventPayload::Stage) — closed-set lifecycle /
///   per-stage observation events.
/// - [`Record`](RequestEventPayload::Record) — transport-adjacent captures
///   such as inbound connection facts, outbound wire-truth, and usage.
/// - [`Custom`](RequestEventPayload::Custom) — `Any`-typed escape hatch
///   for middleware / decorator stages.
#[derive(Clone, Debug)]
pub enum RequestEventPayload {
  Stage(StageEvent),
  Record(RecordEvent),
  Custom(CustomEvent),
}

/// `Any`-typed payload published by stages or decorators that need to share
/// structured data outside the closed [`StageEvent`] set.
#[derive(Clone)]
pub struct CustomEvent {
  /// Stable namespaced identifier (e.g. `"retry.attempt"`). Subscribers match
  /// on this before downcasting.
  pub kind: &'static str,
  /// Reference-counted so subscribers can cheaply clone the event and
  /// downcast independently.
  pub payload: Arc<dyn Any + Send + Sync>,
}

impl std::fmt::Debug for CustomEvent {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("CustomEvent")
      .field("kind", &self.kind)
      .field("payload", &"<Any>")
      .finish()
  }
}

impl CustomEvent {
  pub fn new<T: Any + Send + Sync>(kind: &'static str, value: T) -> Self {
    Self {
      kind,
      payload: Arc::new(value),
    }
  }

  pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
    self.payload.downcast_ref::<T>()
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn custom_event_roundtrips_via_downcast() {
    #[derive(Debug, PartialEq)]
    struct RetryAttempt {
      n: u32,
      reason: SmolStr,
    }

    let ev = CustomEvent::new(
      "retry.attempt",
      RetryAttempt {
        n: 2,
        reason: SmolStr::new("timeout"),
      },
    );
    assert_eq!(ev.kind, "retry.attempt");
    let inner = ev
      .downcast_ref::<RetryAttempt>()
      .expect("payload should downcast back to its declared type");
    assert_eq!(
      inner,
      &RetryAttempt {
        n: 2,
        reason: SmolStr::new("timeout"),
      }
    );
    assert!(
      ev.downcast_ref::<u32>().is_none(),
      "downcast to the wrong type must return None"
    );
  }
}
