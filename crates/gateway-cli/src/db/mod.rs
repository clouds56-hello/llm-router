pub mod migrate;
pub mod requests;
pub mod sessions;
pub mod usage;

use bytes::Bytes;
use snafu::Snafu;

pub use llm_core::db::{CallRecord, DbPaths, HttpSnapshot, MessageRecord, PartRecord};
pub use usage::UsageDb;

#[cfg(test)]
pub use llm_core::db::SessionSource;

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

fn write_record(
  usage: &mut usage::UsageDb,
  requests: &mut requests::RequestsDb,
  sessions: &mut Option<sessions::SessionsDb>,
  record: &CallRecord,
) {
  if let Err(e) = usage.record(record) {
    tracing::warn!(error = %e, "failed to write usage db row");
  }
  if let Err(e) = requests.record(record) {
    tracing::warn!(error = %e, "failed to write requests db row");
  }
  if let Some(s) = sessions.as_mut() {
    if let Err(e) = s.record(record) {
      tracing::warn!(error = %e, session_id = %record.session_id, "failed to write sessions db row");
    }
  }
}

// --- Event bus integration ---

use llm_core::event::{Event, EventHandler};
use std::collections::HashMap;

/// Partial request data accumulated from lifecycle events before completion.
struct PendingRequest {
  ts: i64,
  session_id: Option<String>,
  project_id: Option<String>,
  endpoint: String,
  model: String,
  initiator: String,
  stream: bool,
  account_id: String,
  provider_id: String,
  inbound_req: HttpSnapshot,
  outbound_req: Option<HttpSnapshot>,
}

/// Database writer that implements `EventHandler` for use with the event bus.
/// Accumulates lifecycle events in memory and writes a full row on RequestCompleted.
pub struct DbEventHandler {
  usage: usage::UsageDb,
  requests: requests::RequestsDb,
  sessions: Option<sessions::SessionsDb>,
  pending: HashMap<String, PendingRequest>,
}

impl DbEventHandler {
  pub fn new(paths: DbPaths) -> Result<Self> {
    let usage = usage::UsageDb::open(&paths.usage_db)?;
    let requests = requests::RequestsDb::new(paths.requests_dir)?;
    let sessions = match sessions::SessionsDb::open(&paths.sessions_db) {
      Ok(s) => Some(s),
      Err(e) => {
        tracing::error!(error = %e, path = %paths.sessions_db.display(), "sessions.db open failed; continuing without per-message capture");
        None
      }
    };
    Ok(Self { usage, requests, sessions, pending: HashMap::new() })
  }
}

impl EventHandler for DbEventHandler {
  fn handle(&mut self, event: &Event) {
    match event {
      Event::RequestStarted { request_id, ts, endpoint, initiator, session_id, project_id, inbound_req } => {
        self.pending.insert(request_id.clone(), PendingRequest {
          ts: *ts,
          session_id: session_id.clone(),
          project_id: project_id.clone(),
          endpoint: endpoint.clone(),
          model: String::new(),
          initiator: initiator.clone().unwrap_or_default(),
          stream: false,
          account_id: String::new(),
          provider_id: String::new(),
          inbound_req: inbound_req.clone(),
          outbound_req: None,
        });
      }
      Event::RequestParsed { request_id, account_id, provider_id, model, stream, initiator, outbound_req } => {
        if let Some(p) = self.pending.get_mut(request_id) {
          p.account_id = account_id.clone();
          p.provider_id = provider_id.clone();
          p.model = model.clone();
          p.stream = *stream;
          p.initiator = initiator.clone();
          p.outbound_req = outbound_req.clone();
        }
      }
      Event::RequestCompleted { request_id, session_source, latency_ms, status, prompt_tokens, completion_tokens, request_error, inbound_resp, outbound_resp, messages } => {
        let pending = self.pending.remove(request_id);
        let record = if let Some(p) = pending {
          CallRecord {
            ts: p.ts,
            session_id: p.session_id.unwrap_or_default(),
            session_source: *session_source,
            request_id: Some(request_id.clone()),
            request_error: request_error.clone(),
            project_id: p.project_id,
            endpoint: p.endpoint,
            account_id: p.account_id,
            provider_id: p.provider_id,
            model: p.model,
            initiator: p.initiator,
            status: *status,
            stream: p.stream,
            latency_ms: *latency_ms,
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            inbound_req: p.inbound_req,
            outbound_req: p.outbound_req,
            outbound_resp: outbound_resp.clone(),
            inbound_resp: inbound_resp.clone(),
            messages: messages.clone(),
          }
        } else {
          // Fallback: no prior events captured (shouldn't happen in normal flow)
          tracing::debug!(request_id = %request_id, "RequestCompleted without prior RequestStarted");
          CallRecord {
            ts: 0,
            session_id: String::new(),
            session_source: *session_source,
            request_id: Some(request_id.clone()),
            request_error: request_error.clone(),
            project_id: None,
            endpoint: String::new(),
            account_id: String::new(),
            provider_id: String::new(),
            model: String::new(),
            initiator: String::new(),
            status: *status,
            stream: false,
            latency_ms: *latency_ms,
            prompt_tokens: *prompt_tokens,
            completion_tokens: *completion_tokens,
            inbound_req: HttpSnapshot::default(),
            outbound_req: None,
            outbound_resp: outbound_resp.clone(),
            inbound_resp: inbound_resp.clone(),
            messages: messages.clone(),
          }
        };
        write_record(&mut self.usage, &mut self.requests, &mut self.sessions, &record);
      }
      _ => {}
    }
  }
}
