pub mod migration;
pub mod requests;
pub mod sessions;
pub mod usage;

use bytes::Bytes;
use rusqlite::Connection;
use snafu::Snafu;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

pub use usage::UsageDb;

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
  pub data_dir: PathBuf,
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

#[derive(Debug)]
pub struct CallRecord {
  pub ts: i64,
  pub session_id: Option<String>,
  pub endpoint: String,
  pub account_id: String,
  pub provider_id: String,
  pub model: String,
  pub initiator: String,
  pub status: u16,
  pub stream: bool,
  pub latency_ms: u64,
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  pub req_headers: Bytes,
  pub req_body: Bytes,
  pub resp_headers: Option<Bytes>,
  pub resp_body: Option<Bytes>,
  pub messages: Vec<MessageRecord>,
}

#[derive(Debug)]
pub struct MessageRecord {
  pub role: String,
  pub status: Option<u16>,
  pub body: Bytes,
}

impl DbStore {
  pub fn spawn(options: DbOptions) -> Result<Self> {
    migration::migrate_legacy_usage(&options.paths.data_dir, &options.paths.usage_db)?;

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
  let usage_conn = Connection::open(&paths.usage_db)?;
  let mut usage = usage::UsageDb::open(usage_conn)?;
  let mut requests = requests::RequestsDb::new(paths.requests_dir)?;
  let mut sessions = sessions::SessionsDb::open(&paths.sessions_db)?;

  while let Some(op) = rx.blocking_recv() {
    match op {
      WriteOp::Record(record) => {
        if let Err(e) = usage.record(&record) {
          tracing::warn!(error = %e, "failed to write usage db row");
        }
        if let Err(e) = requests.record(&record) {
          tracing::warn!(error = %e, "failed to write requests db row");
        }
        if let Err(e) = sessions.record(&record) {
          tracing::warn!(error = %e, "failed to write sessions db row");
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
