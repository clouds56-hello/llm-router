//! Per-request mutable state threaded through every stage.
//!
//! `PipelineCtx` carries identifiers (request id, attempt counter) and a
//! handle to the [`EventBus`] so stages can publish custom events without
//! holding a separate reference to the bus. Stage outputs are *not* stored
//! here — they flow as function-typed return values between stages — but the
//! ctx is the right home for cross-cutting state we add later (timings,
//! cancellation tokens, etc.).

use crate::event::{CustomEvent, Event, EventBus, EventPayload, StageEvent};
use smol_str::SmolStr;
use std::sync::Arc;

pub struct PipelineCtx {
  pub request_id: SmolStr,
  pub attempt: u32,
  pub events: Arc<EventBus>,
}

impl PipelineCtx {
  pub fn new(request_id: impl Into<SmolStr>, events: Arc<EventBus>) -> Self {
    Self {
      request_id: request_id.into(),
      attempt: 0,
      events,
    }
  }

  /// Publish a [`StageEvent`] tagged with the current request id and attempt.
  pub fn emit_known(&self, payload: StageEvent) {
    self.events.emit(Event {
      request_id: self.request_id.clone(),
      attempt: self.attempt,
      payload: EventPayload::Known(payload),
    });
  }

  /// Publish a [`CustomEvent`] from inside a stage or decorator.
  pub fn emit_custom(&self, kind: &'static str, value: impl std::any::Any + Send + Sync) {
    self.events.emit(Event {
      request_id: self.request_id.clone(),
      attempt: self.attempt,
      payload: EventPayload::Custom(CustomEvent::new(kind, value)),
    });
  }
}
