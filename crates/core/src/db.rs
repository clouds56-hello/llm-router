use bytes::Bytes;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct DbPaths {
  pub usage_db: PathBuf,
  pub sessions_db: PathBuf,
  pub requests_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct DbOptions {
  pub paths: DbPaths,
  pub queue_capacity: usize,
  pub body_max_bytes: usize,
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
  pub session_id: String,
  pub session_source: SessionSource,
  pub request_id: Option<String>,
  pub request_error: Option<String>,
  pub project_id: Option<String>,
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
  pub inbound_req: HttpSnapshot,
  pub outbound_req: Option<HttpSnapshot>,
  pub outbound_resp: Option<HttpSnapshot>,
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
  pub part_type: String,
  pub content: Bytes,
}

#[derive(Debug, Clone, Default)]
pub struct HttpSnapshot {
  pub method: Option<String>,
  pub url: Option<String>,
  pub status: Option<u16>,
  pub headers: reqwest::header::HeaderMap,
  pub body: Bytes,
}

pub type OutboundSnapshot = HttpSnapshot;

pub trait DbStore: Send + Sync {
  fn body_max_bytes(&self) -> usize;
  fn record(&self, record: CallRecord);
}
