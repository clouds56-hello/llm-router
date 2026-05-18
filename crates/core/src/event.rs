use crate::db::{MessageRecord, SessionSource, Usage};
use crate::router2_event::Router2Event;
use bytes::Bytes;
use llm_headers::HeaderMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, oneshot};

/// Top-level event flowing on the in-process broadcast bus.
///
/// Subdomain enums group related variants so consumers can `match` on the
/// domain (lifecycle, account, session, router2) without listing every
/// variant at the top level. Telemetry (`StreamProgress`) and control
/// (`Shutdown`) stay at the top level — they're not lifecycle records.
///
/// Note: `Event` is **not** `Clone`. The bus broadcasts `Arc<Event>` so
/// every subscriber sees the same allocation without copying the payload.
/// If a subscriber needs to retain a subdomain payload it can clone the
/// inner enum (e.g. `Router2Event`) which still derives `Clone`.
#[derive(Debug)]
pub enum Event {
  /// Inbound request lifecycle events.
  LegacyRequest(LegacyRequestEvent),
  /// Account / pool lifecycle events.
  Account(AccountEvent),
  /// Session lifecycle events.
  Session(SessionEvent),
  /// Router2 pipeline stage events (relocated to `llm_core::router2_event`).
  /// Embedded here so subscribers can observe router2 stages on the same
  /// in-process bus that already carries the lifecycle events.
  Router2(Router2Event),

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

/// Inbound request lifecycle. Each variant corresponds to a well-defined
/// point in the gateway's per-request state machine.
#[derive(Debug)]
pub enum LegacyRequestEvent {
  /// Request accepted. Emitted before body decode/parse begins.
  Started {
    request_id: String,
    ts: i64,
    endpoint: String,
    session_id: Option<String>,
    peer_addr: Option<String>,
    local_addr: Option<String>,
    method: String,
    url: Option<String>,
  },

  /// Request headers parsed/classified (before body parse).
  Headers {
    request_id: String,
    ts: i64,
    endpoint_hint: Option<String>,
    path: Option<String>,
    session_id: Option<String>,
    project_id: Option<String>,
    header_initiator: Option<String>,
    local_addr: Option<String>,
    mode: Option<String>,
    route_mode_hint: Option<String>,
    inbound_headers: HeaderMap,
  },

  /// Request routed to an account, about to send upstream.
  Parsed {
    request_id: String,
    /// Retry attempt number (0 = first attempt).
    attempt: u32,
    account_id: String,
    provider_id: String,
    model: String,
    stream: bool,
    initiator: String,
    behave_as: Option<String>,
    /// Post-decompression raw bytes of the inbound request body.
    /// Empty for requests without a body. Used to populate
    /// `CallRecord.inbound_req.body` and the `inbound_req_body` DB column.
    inbound_body: Bytes,
  },

  /// Upstream response headers received.
  ///
  /// Carries the outbound request snapshot (method/url/headers/body) because
  /// in routed mode it only becomes known after the upstream send completes.
  /// Proxy passthrough also fills these for consistency.
  Responded {
    request_id: String,
    /// Retry attempt number (0 = first attempt).
    attempt: u32,
    /// Upstream HTTP status code (us ← upstream).
    outbound_status: u16,
    /// Time from inbound request start until upstream response headers arrived.
    latency_ms: u64,
    /// Upstream response headers (the response we received from upstream).
    outbound_resp_headers: HeaderMap,
    /// Outbound request method (us → upstream). `None` if capture was unavailable.
    outbound_req_method: Option<String>,
    /// Outbound request URL.
    outbound_req_url: Option<String>,
    /// Outbound request headers actually sent upstream.
    outbound_req_headers: Option<HeaderMap>,
    /// Outbound request body actually sent upstream (post-encoding).
    outbound_req_body: Option<Bytes>,
  },

  /// Per-attempt result with full response data.
  /// Emitted once per attempt (including retries).
  /// `request_id` is the base ID; `attempt` distinguishes retries.
  Result {
    request_id: String,
    /// Retry attempt number (0 = first attempt).
    attempt: u32,
    session_source: SessionSource,
    latency_ms: u64,
    /// Inbound HTTP status code (us → client).
    inbound_status: u16,
    usage: Usage,
    request_error: Option<String>,
    /// Inbound response headers (us → client).
    inbound_resp_headers: HeaderMap,
    /// Inbound response body (us → client), possibly truncated.
    inbound_resp_body: Bytes,
    /// Outbound response body (upstream → us), possibly truncated.
    /// Headers were already delivered on `RequestResponded`.
    outbound_resp_body: Option<Bytes>,
    messages: Vec<MessageRecord>,
  },

  /// Overall request completed (terminal outcome for the whole request).
  /// Emitted exactly once per request, after all attempts.
  Completed {
    request_id: String,
    /// Whether the request ultimately succeeded.
    success: bool,
    /// Total number of attempts made (1 = no retries, 2 = one retry, ...).
    total_attempts: u32,
    /// Final HTTP status code (None if no successful upstream response was reached).
    final_status: Option<u16>,
    /// Total latency from RequestStarted to completion.
    total_latency_ms: u64,
    /// Error message if `success == false`.
    error: Option<String>,
  },

  /// A single attempt failed and will be retried.
  Retry {
    request_id: String,
    /// The attempt number that just failed.
    attempt: u32,
    error: String,
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
}

impl EventBus {
  /// Create a new event bus with the given per-receiver channel capacity.
  /// Slow receivers that fall more than `capacity` events behind will see
  /// `RecvError::Lagged` on their next `recv()`.
  pub fn new(capacity: usize) -> Self {
    let (tx, _) = broadcast::channel(capacity.max(1));
    Self { tx }
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

  /// Gracefully shut down the event bus, waiting for the consumer to drain.
  pub async fn shutdown(&self) {
    let (tx, rx) = oneshot::channel();
    let _ = self.tx.send(Arc::new(Event::Shutdown(Mutex::new(Some(tx)))));
    let _ = rx.await;
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
