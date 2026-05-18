//! Event types and dispatcher for the router2 pipeline.
//!
//! Two payload shapes are supported:
//!
//! * [`StageEvent`] — a closed enum of stage-observation variants the runner
//!   emits at well-defined points. New variants are added as the pipeline
//!   grows; subscribers `match` on them.
//! * [`CustomEvent`] — an `Any`-typed escape hatch for middleware / decorator
//!   stages (e.g. retry, cache) to publish their own structured records
//!   without modifying [`StageEvent`]. The payload is shared via `Arc` so
//!   subscribers can cheaply clone and downcast.

pub mod stage;

pub use stage::{ConvertedResponseSummary, SentSummary, Stage, StageEvent};

use parking_lot_helper::Mutex;
use smol_str::SmolStr;
use std::any::Any;
use std::sync::Arc;

/// Minimal local re-export so we don't need to add `parking_lot` to this
/// crate's deps; `std::sync::Mutex` is fine for the event bus.
mod parking_lot_helper {
  pub use std::sync::Mutex;
}

/// A single event flowing through the pipeline.
#[derive(Clone)]
pub struct Event {
  pub request_id: SmolStr,
  pub attempt: u32,
  pub payload: EventPayload,
}

/// Either a typed pipeline event or an arbitrary user-defined record.
#[derive(Clone)]
pub enum EventPayload {
  Known(StageEvent),
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

/// In-process fan-out for [`Event`]s. Subscribers register a boxed callback.
///
/// Intentionally simple for PR1: callbacks run inline on `emit`. A future
/// revision can swap this for an mpsc/broadcast channel without disturbing
/// the call sites (which only touch `emit`).
#[derive(Default)]
pub struct EventBus {
  subscribers: Mutex<Vec<Arc<dyn Fn(&Event) + Send + Sync>>>,
}

impl EventBus {
  pub fn new() -> Self {
    Self {
      subscribers: Mutex::new(Vec::new()),
    }
  }

  pub fn subscribe<F>(&self, f: F)
  where
    F: Fn(&Event) + Send + Sync + 'static,
  {
    self.subscribers.lock().unwrap().push(Arc::new(f));
  }

  pub fn emit(&self, event: Event) {
    // Snapshot the subscriber list so callbacks may freely re-subscribe
    // (or block) without re-entering the mutex.
    let subs: Vec<_> = self.subscribers.lock().unwrap().clone();
    for sub in subs {
      sub(&event);
    }
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

  #[test]
  fn event_bus_fans_out_to_all_subscribers() {
    use std::sync::atomic::{AtomicU32, Ordering};

    let bus = EventBus::new();
    let calls = Arc::new(AtomicU32::new(0));
    for _ in 0..3 {
      let calls = calls.clone();
      bus.subscribe(move |_| {
        calls.fetch_add(1, Ordering::SeqCst);
      });
    }

    bus.emit(Event {
      request_id: SmolStr::new("req-1"),
      attempt: 0,
      payload: EventPayload::Custom(CustomEvent::new("test.ping", ())),
    });
    assert_eq!(calls.load(Ordering::SeqCst), 3);
  }
}
