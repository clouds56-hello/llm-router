use crate::db::Usage;
use crate::request_event::RequestEvent;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};

/// Top-level event flowing on the in-process broadcast bus.
///
/// Subdomain enums group related variants so consumers can `match` on the
/// domain (lifecycle, account, session, requests) without listing every
/// variant at the top level. Telemetry (`StreamProgress`) and control
/// (`Shutdown`) stay at the top level — they're not lifecycle records.
///
/// Note: `Event` is **not** `Clone`. The bus broadcasts `Arc<Event>` so
/// every subscriber sees the same allocation without copying the payload.
/// If a subscriber needs to retain a subdomain payload it can clone the
/// inner enum (e.g. `RequestEvent`) which still derives `Clone`.
#[derive(Debug)]
pub enum Event {
  /// Account / pool lifecycle events.
  Account(AccountEvent),
  /// Session lifecycle events.
  Session(SessionEvent),
  /// Requests pipeline stage events (relocated to `tokn_core::request_event`).
  /// Embedded here so subscribers can observe requests stages on the same
  /// in-process bus that already carries the lifecycle events.
  Requests(RequestEvent),

  // --- Control ---
  /// Request graceful shutdown; sender receives `()` when drain is complete.
  ///
  /// Wrapped in `Mutex<Option<...>>` because `oneshot::Sender` is move-only
  /// and the variant must be observable from `&Event` (we hand subscribers
  /// `&*Arc<Event>`). The consumer that handles shutdown takes the sender
  /// out of the slot and signals; other subscribers see the variant but
  /// find the slot already drained. The outer `Arc<Event>` provides the
  /// sharing — no additional `Arc` is needed inside.
  Shutdown(Mutex<Option<oneshot::Sender<()>>>),

  // --- Streaming progress (telemetry, not lifecycle) ---
  /// Periodic progress update from an active streaming response.
  StreamProgress {
    request_id: String,
    model: String,
    endpoint: String,
    usage: Usage,
    bytes_streamed: u64,
    chunks: u64,
  },
}

/// Account / pool lifecycle events.
#[derive(Debug, Clone)]
pub enum AccountEvent {
  /// An account was marked as failed and placed in cooldown.
  Cooldown {
    account: String,
    provider: String,
    cooldown_secs: u64,
  },
  /// An account recovered from cooldown.
  Recovered { account: String, provider: String },
  /// An upstream auth token was refreshed.
  TokenRefreshed { account: String, provider: String },
}

/// Session lifecycle events.
#[derive(Debug, Clone)]
pub enum SessionEvent {
  /// A session was bound to an account.
  Created { session_id: String, account: String },
  /// A session expired or was evicted.
  Expired { session_id: String },
}

/// Non-blocking event emitter backed by a tokio broadcast channel.
///
/// Cloneable; subscribers obtain independent [`broadcast::Receiver`]s via
/// [`EventBus::subscribe`]. The channel carries `Arc<Event>` so every
/// subscriber sees the same allocation without copying the payload.
/// Emitting with no live subscribers is a no-op (the underlying `send`
/// returns `Err`, which we swallow).
#[derive(Clone)]
pub struct EventBus {
  tx: broadcast::Sender<Arc<Event>>,
  active_finalizers: Arc<AtomicUsize>,
}

impl EventBus {
  /// Create a new event bus with the given per-receiver channel capacity.
  /// Slow receivers that fall more than `capacity` events behind will see
  /// `RecvError::Lagged` on their next `recv()`.
  pub fn new(capacity: usize) -> Self {
    let (tx, _) = broadcast::channel(capacity.max(1));
    Self {
      tx,
      active_finalizers: Arc::new(AtomicUsize::new(0)),
    }
  }

  /// Subscribe to events. The returned receiver only sees events emitted
  /// *after* it was created.
  pub fn subscribe(&self) -> broadcast::Receiver<Arc<Event>> {
    self.tx.subscribe()
  }

  /// Emit an event without blocking. The event is wrapped in an `Arc` so
  /// subscribers share a single allocation. No-op if there are no
  /// subscribers.
  pub fn emit(&self, event: Event) {
    // broadcast::Sender::send returns Err only when there are no active
    // receivers; treat that as "nobody listening" and drop quietly.
    let _ = self.tx.send(Arc::new(event));
  }

  pub fn begin_finalizer(&self) -> EventFinalizerGuard {
    self.active_finalizers.fetch_add(1, Ordering::AcqRel);
    EventFinalizerGuard {
      active_finalizers: self.active_finalizers.clone(),
      finished: false,
    }
  }

  /// Gracefully shut down the event bus, waiting for the consumer to drain.
  pub async fn shutdown(&self) {
    tracing::info!(
      "shutting down event bus, waiting for active finalizers to finish... (active={})",
      self.active_finalizers.load(Ordering::Acquire)
    );
    while self.active_finalizers.load(Ordering::Acquire) != 0 {
      tokio::task::yield_now().await;
    }
    tracing::debug!("no active finalizers, sending shutdown signal");
    let (tx, rx) = oneshot::channel();
    let _ = self.tx.send(Arc::new(Event::Shutdown(Mutex::new(Some(tx)))));
    let _ = rx.await;
  }
}

pub struct EventFinalizerGuard {
  active_finalizers: Arc<AtomicUsize>,
  finished: bool,
}

impl EventFinalizerGuard {
  pub fn finish(mut self) {
    if !self.finished {
      self.active_finalizers.fetch_sub(1, Ordering::AcqRel);
      self.finished = true;
    }
  }
}

impl Drop for EventFinalizerGuard {
  fn drop(&mut self) {
    if !self.finished {
      self.active_finalizers.fetch_sub(1, Ordering::AcqRel);
      self.finished = true;
    }
  }
}

/// Trait for event handlers that process events on the background thread.
pub trait EventHandler: Send + 'static {
  /// Handle a single event. Called sequentially on the consumer thread.
  fn handle(&mut self, event: &Event);

  /// Called once before the consumer thread exits.
  fn flush(&mut self) {}
}

/// A no-op event bus for contexts where events are not needed (e.g. tests).
impl EventBus {
  pub fn noop() -> Self {
    Self::new(1)
  }
}

/// Spawn a background OS thread that consumes events and dispatches to
/// handlers sequentially. The thread owns a single broadcast receiver; all
/// handlers see every event in arrival order.
///
/// Slow handlers can push the receiver into a `Lagged` state — when that
/// happens we log a warning and continue draining.
pub fn spawn_event_loop(
  mut receiver: broadcast::Receiver<Arc<Event>>,
  mut handlers: Vec<Box<dyn EventHandler>>,
) -> std::thread::JoinHandle<()> {
  std::thread::spawn(move || {
    let mut flushed = false;
    loop {
      match receiver.blocking_recv() {
        Ok(event) => {
          if let Event::Shutdown(slot) = &*event {
            for handler in &mut handlers {
              handler.flush();
            }
            flushed = true;
            if let Some(done) = slot.lock().unwrap().take() {
              let _ = done.send(());
            }
            break;
          }
          for handler in &mut handlers {
            handler.handle(&event);
          }
        }
        Err(broadcast::error::RecvError::Lagged(n)) => {
          tracing::warn!("event loop lagged behind by {n} events");
          continue;
        }
        Err(broadcast::error::RecvError::Closed) => break,
      }
    }
    if !flushed {
      for handler in &mut handlers {
        handler.flush();
      }
    }
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::time::Duration;

  #[tokio::test]
  async fn shutdown_waits_for_active_finalizer() {
    let bus = EventBus::new(4);
    let guard = bus.begin_finalizer();

    let shutdown = tokio::spawn({
      let bus = bus.clone();
      async move { bus.shutdown().await }
    });

    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(!shutdown.is_finished(), "shutdown should wait for active finalizer");

    guard.finish();
    shutdown.await.unwrap();
  }
}
