use crate::db::{HttpSnapshot, MessageRecord, SessionSource};
use reqwest::header::HeaderMap;
use tokio::sync::{mpsc, oneshot};

/// Events emitted by the router during request processing and account management.
#[derive(Debug)]
pub enum Event {
  // --- Request lifecycle ---
  /// Request received and parsed. Emitted before upstream send.
  RequestStarted {
    request_id: String,
    ts: i64,
    endpoint: String,
    model: String,
    initiator: String,
    stream: bool,
    session_id: Option<String>,
    project_id: Option<String>,
    inbound_req: HttpSnapshot,
  },

  /// Request routed to an account, about to send upstream.
  RequestParsed {
    request_id: String,
    account_id: String,
    provider_id: String,
    outbound_req: Option<HttpSnapshot>,
  },

  /// Upstream response headers received.
  RequestResponded {
    request_id: String,
    status: u16,
    resp_headers: HeaderMap,
  },

  /// Request/stream completed with final metrics.
  /// Fields already sent in RequestStarted/RequestParsed are NOT repeated here.
  /// The DB handler merges from its in-memory accumulator.
  RequestCompleted {
    request_id: String,
    session_source: SessionSource,
    latency_ms: u64,
    status: u16,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    request_error: Option<String>,
    inbound_resp: HttpSnapshot,
    outbound_resp: Option<HttpSnapshot>,
    messages: Vec<MessageRecord>,
  },

  /// A request failed after all retries.
  RequestFailed {
    request_id: Option<String>,
    model: String,
    account: String,
    endpoint: String,
    error: String,
    latency_ms: u64,
  },

  /// A single attempt failed and will be retried.
  RequestRetry {
    request_id: Option<String>,
    model: String,
    account: String,
    attempt: usize,
    error: String,
  },

  // --- Account / pool ---
  /// An account was marked as failed and placed in cooldown.
  AccountCooldown {
    account: String,
    provider: String,
    cooldown_secs: u64,
  },

  /// An account recovered from cooldown.
  AccountRecovered {
    account: String,
    provider: String,
  },

  /// A session was bound to an account.
  SessionCreated {
    session_id: String,
    account: String,
  },

  /// A session expired or was evicted.
  SessionExpired {
    session_id: String,
  },

  /// An upstream auth token was refreshed.
  TokenRefreshed {
    account: String,
    provider: String,
  },

  // --- Control ---
  /// Request graceful shutdown; sender receives `()` when drain is complete.
  Shutdown(oneshot::Sender<()>),

  // --- Streaming progress ---
  /// Periodic progress update from an active streaming response.
  StreamProgress {
    request_id: Option<String>,
    model: String,
    endpoint: String,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    bytes_streamed: u64,
    chunks: u64,
  },
}

/// Non-blocking event emitter. Cloneable, stored in AppState.
#[derive(Clone)]
pub struct EventBus {
  tx: mpsc::Sender<Event>,
}

impl EventBus {
  /// Create a new event bus with the given bounded channel capacity.
  /// Returns the bus (producer side) and the receiver (consumer side).
  pub fn new(capacity: usize) -> (Self, EventReceiver) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (Self { tx }, EventReceiver { rx })
  }

  /// Emit an event without blocking. Drops the event if the channel is full.
  pub fn emit(&self, event: Event) {
    match self.tx.try_send(event) {
      Ok(()) => {}
      Err(mpsc::error::TrySendError::Full(_)) => {
        tracing::warn!("event bus full, dropping event");
      }
      Err(mpsc::error::TrySendError::Closed(_)) => {
        tracing::warn!("event bus closed, dropping event");
      }
    }
  }

  /// Gracefully shut down the event bus, waiting for the consumer to drain.
  pub async fn shutdown(&self) {
    let (tx, rx) = oneshot::channel();
    // Best-effort; if the bus is full or closed, we just proceed.
    let _ = self.tx.send(Event::Shutdown(tx)).await;
    let _ = rx.await;
  }
}

/// Consumer side of the event bus. Passed to the background handler thread.
pub struct EventReceiver {
  rx: mpsc::Receiver<Event>,
}

impl EventReceiver {
  /// Blocking receive. Intended for use in a dedicated OS thread.
  pub fn blocking_recv(&mut self) -> Option<Event> {
    self.rx.blocking_recv()
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
    let (bus, _rx) = Self::new(1);
    bus
  }
}

/// Spawn a background OS thread that consumes events and dispatches to handlers.
pub fn spawn_event_loop(mut receiver: EventReceiver, mut handlers: Vec<Box<dyn EventHandler>>) -> std::thread::JoinHandle<()> {
  std::thread::spawn(move || {
    while let Some(event) = receiver.blocking_recv() {
      if let Event::Shutdown(done) = event {
        for handler in &mut handlers {
          handler.flush();
        }
        let _ = done.send(());
        break;
      }
      for handler in &mut handlers {
        handler.handle(&event);
      }
    }
    // Channel closed without shutdown event — flush anyway
    for handler in &mut handlers {
      handler.flush();
    }
  })
}
