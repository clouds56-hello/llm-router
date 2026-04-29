pub mod migrate;
pub mod requests;
pub mod sessions;
pub mod usage;

use bytes::Bytes;
use snafu::Snafu;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

pub use usage::UsageDb;

/// Serialise an HTTP header map to JSON bytes, redacting values whose name
/// is sensitive (`authorization`, `proxy-authorization`, `cookie`, anything
/// containing `api-key`). Public so both inbound (server::forward) and
/// outbound (db::requests) capture paths share the same redaction policy.
pub fn headers_json(headers: &reqwest::header::HeaderMap) -> Bytes {
  use serde_json::{Map, Value};
  let mut out = Map::new();
  for (name, value) in headers {
    let key = name.as_str().to_ascii_lowercase();
    let value = if is_sensitive_header(&key) {
      "<redacted>".to_string()
    } else {
      value.to_str().unwrap_or("<non-utf8>").to_string()
    };
    out.insert(key, Value::String(value));
  }
  serde_json::to_vec(&Value::Object(out)).unwrap_or_default().into()
}

pub fn is_sensitive_header(name: &str) -> bool {
  matches!(name, "authorization" | "proxy-authorization" | "cookie") || name.contains("api-key")
}

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
  #[snafu(display("db io"))]
  Io { source: std::io::Error },

  #[snafu(display("sqlite"))]
  Sqlite { source: rusqlite::Error },

  #[snafu(display("db writer channel closed"))]
  ChannelClosed,
}

impl From<std::io::Error> for Error {
  fn from(source: std::io::Error) -> Self {
    Error::Io { source }
  }
}

impl From<rusqlite::Error> for Error {
  fn from(source: rusqlite::Error) -> Self {
    Error::Sqlite { source }
  }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone)]
pub struct DbStore {
  tx: mpsc::Sender<WriteOp>,
  body_max_bytes: usize,
}

pub struct DbPaths {
  pub usage_db: PathBuf,
  pub sessions_db: PathBuf,
  pub requests_dir: PathBuf,
}
pub struct DbOptions {
  pub paths: DbPaths,
  pub queue_capacity: usize,
  pub body_max_bytes: usize,
}

#[derive(Debug)]
enum WriteOp {
  Record(Box<CallRecord>),
  Shutdown(oneshot::Sender<()>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionSource {
  Header,
  Auto,
}

impl SessionSource {
  pub fn as_str(self) -> &'static str {
    match self {
      SessionSource::Header => "header",
      SessionSource::Auto => "auto",
    }
  }
}

#[derive(Debug)]
pub struct CallRecord {
  pub ts: i64,
  /// The session id used for sessions.db rows. Always populated; either the
  /// inbound header value (`session_source = Header`) or a fresh UUID
  /// (`session_source = Auto`).
  pub session_id: String,
  pub session_source: SessionSource,
  pub endpoint: String,
  pub account_id: String,
  pub provider_id: String,
  pub model: String,
  pub initiator: String,
  /// Status code returned to the client (== `inbound_resp.status`). Kept as
  /// a top-level scalar so usage.db and indexes don't have to chase the
  /// nested snapshot.
  pub status: u16,
  pub stream: bool,
  pub latency_ms: u64,
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  /// What the client sent us.
  pub inbound_req: HttpSnapshot,
  /// What we sent to the upstream provider (post-transform, post-auth).
  /// `None` when no upstream attempt succeeded enough to capture.
  pub outbound_req: Option<HttpSnapshot>,
  /// What the upstream provider returned to us.
  pub outbound_resp: Option<HttpSnapshot>,
  /// What we forwarded back to the client. Today this matches
  /// `outbound_resp` byte-for-byte (transparent forwarder); kept as its own
  /// snapshot so future response transforms remain auditable.
  pub inbound_resp: HttpSnapshot,
  pub messages: Vec<MessageRecord>,
}

#[derive(Debug)]
pub struct MessageRecord {
  pub role: String,
  pub status: Option<u16>,
  pub parts: Vec<PartRecord>,
}

#[derive(Debug, Clone)]
pub struct PartRecord {
  /// e.g. `"text"`, `"image_url"`, `"image"`, `"tool_use"`, `"tool_result"`,
  /// `"input_text"`, `"raw"`, …
  pub part_type: String,
  /// utf-8 for `text`/`input_text` parts, JSON bytes for everything else.
  pub content: Bytes,
}

/// One side of an HTTP exchange. Method/url are populated for requests,
/// status for responses; all four can be present when reconstructing a full
/// inbound or outbound snapshot.
#[derive(Debug, Clone, Default)]
pub struct HttpSnapshot {
  pub method: Option<String>,
  pub url: Option<String>,
  pub status: Option<u16>,
  pub headers: reqwest::header::HeaderMap,
  pub body: Bytes,
}

/// Backwards-compatible alias used by the provider capture slot.
pub type OutboundSnapshot = HttpSnapshot;

impl DbStore {
  pub fn spawn(options: DbOptions) -> Result<Self> {
    let capacity = options.queue_capacity.max(1);
    let (tx, rx) = mpsc::channel(capacity);
    let body_max_bytes = options.body_max_bytes;
    std::thread::spawn(move || {
      if let Err(e) = writer_loop(options.paths, rx) {
        tracing::error!(error = %e, "db writer stopped");
      }
    });
    Ok(Self { tx, body_max_bytes })
  }

  pub fn body_max_bytes(&self) -> usize {
    self.body_max_bytes
  }

  pub fn record(&self, record: CallRecord) {
    match self.tx.try_send(WriteOp::Record(Box::new(record))) {
      Ok(()) => {}
      Err(mpsc::error::TrySendError::Full(_)) => tracing::warn!("db queue full, dropping record"),
      Err(mpsc::error::TrySendError::Closed(_)) => tracing::warn!("db queue closed, dropping record"),
    }
  }

  pub async fn shutdown(&self) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    self
      .tx
      .send(WriteOp::Shutdown(tx))
      .await
      .map_err(|_| Error::ChannelClosed)?;
    rx.await.map_err(|_| Error::ChannelClosed)?;
    Ok(())
  }
}

fn writer_loop(paths: DbPaths, mut rx: mpsc::Receiver<WriteOp>) -> Result<()> {
  let mut usage = usage::UsageDb::open(&paths.usage_db)?;
  let mut requests = requests::RequestsDb::new(paths.requests_dir)?;
  let mut sessions = match sessions::SessionsDb::open(&paths.sessions_db) {
    Ok(s) => Some(s),
    Err(e) => {
      tracing::error!(error = %e, path = %paths.sessions_db.display(), "sessions.db open failed; continuing without per-message capture");
      None
    }
  };

  while let Some(op) = rx.blocking_recv() {
    match op {
      WriteOp::Record(record) => {
        if let Err(e) = usage.record(&record) {
          tracing::warn!(error = %e, "failed to write usage db row");
        }
        if let Err(e) = requests.record(&record) {
          tracing::warn!(error = %e, "failed to write requests db row");
        }
        if let Some(s) = sessions.as_mut() {
          if let Err(e) = s.record(&record) {
            tracing::warn!(error = %e, session_id = %record.session_id, "failed to write sessions db row");
          }
        }
      }
      WriteOp::Shutdown(done) => {
        let _ = done.send(());
        break;
      }
    }
  }
  Ok(())
}
