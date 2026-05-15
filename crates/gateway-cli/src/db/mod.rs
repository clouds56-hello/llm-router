pub mod archive;
pub mod migrate;
pub mod requests;
pub mod sessions;
pub mod usage;

use bytes::Bytes;
use llm_headers::HeaderMap;
use snafu::Snafu;

use llm_core::db::SessionSource;
pub use llm_core::db::{CallRecord, DbPaths, HttpSnapshot, MessageRecord, PartRecord};
#[allow(unused_imports)]
pub(crate) use llm_core::db::{Usage, UsageDetails};
pub use usage::UsageDb;

/// Serialise an HTTP header map to JSON bytes, redacting values whose name
/// is sensitive (`authorization`, `proxy-authorization`, `cookie`, anything
/// containing `api-key`). Public so both inbound (server::forward) and
/// outbound (db::requests) capture paths share the same redaction policy.
pub fn headers_json(headers: &llm_headers::HeaderMap) -> Bytes {
  use serde_json::{Map, Value};
  let mut out = Map::new();
  for (name, value) in headers {
    let key = name.as_str().to_ascii_lowercase();
    let value = if is_sensitive_header(&key) {
      "<redacted>".to_string()
    } else {
      value.as_str().to_string()
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

fn write_record(usage: &mut usage::UsageDb, sessions: &mut Option<sessions::SessionsDb>, record: &CallRecord) {
  if let Err(e) = usage.record(record) {
    tracing::warn!(error = %e, "failed to write usage db row");
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
#[derive(Clone)]
struct PendingRequest {
  ts: i64,
  session_id: Option<String>,
  project_id: Option<String>,
  local_addr: Option<String>,
  mode: Option<String>,
  behave_as: Option<String>,
  peer_addr: Option<String>,
  method: Option<String>,
  endpoint: String,
  model: String,
  initiator: String,
  stream: bool,
  account_id: String,
  provider_id: String,
  inbound_url: Option<String>,
  inbound_req_headers: HeaderMap,
  inbound_req_body: Bytes,
  outbound_method: Option<String>,
  outbound_url: Option<String>,
  outbound_req_headers: HeaderMap,
  outbound_req_body: Bytes,
  outbound_resp_headers: HeaderMap,
  outbound_have: bool,
  latency_header_ms: Option<u64>,
  result_written: bool,
}

/// Database writer that implements `EventHandler` for use with the event bus.
/// Accumulates lifecycle events in memory and writes a full row on RequestCompleted.
pub struct DbEventHandler {
  usage: usage::UsageDb,
  requests: requests::RequestsDb,
  sessions: Option<sessions::SessionsDb>,
  pending: HashMap<(String, u32), PendingRequest>,
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
    Ok(Self {
      usage,
      requests,
      sessions,
      pending: HashMap::new(),
    })
  }

  fn fallback_ts() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
  }
}

impl EventHandler for DbEventHandler {
  fn handle(&mut self, event: &Event) {
    match event {
      Event::RequestStarted {
        request_id,
        ts,
        endpoint,
        session_id,
        peer_addr,
        local_addr,
        method,
        url,
      } => {
        let inbound_req = HttpSnapshot {
          method: Some(method.clone()),
          url: url.clone(),
          status: None,
          req_headers: HeaderMap::new(),
          req_body: Bytes::new(),
          resp_headers: HeaderMap::new(),
          resp_body: Bytes::new(),
        };
        if let Err(e) = self.requests.started(
          request_id,
          *ts,
          endpoint,
          session_id.as_deref(),
          requests::RequestContext {
            local_addr: local_addr.as_deref(),
            ..Default::default()
          },
          peer_addr.as_deref(),
          Some(method.as_str()),
          &inbound_req,
        ) {
          tracing::warn!(error = %e, "failed to insert started requests db row");
        }
        self.pending.insert(
          (request_id.clone(), 0),
          PendingRequest {
            ts: *ts,
            session_id: session_id.clone(),
            project_id: None,
            local_addr: local_addr.clone(),
            mode: None,
            behave_as: None,
            peer_addr: peer_addr.clone(),
            method: Some(method.clone()),
            endpoint: endpoint.clone(),
            model: String::new(),
            initiator: String::new(),
            stream: false,
            account_id: String::new(),
            provider_id: String::new(),
            inbound_url: url.clone(),
            inbound_req_headers: HeaderMap::new(),
            inbound_req_body: Bytes::new(),
            outbound_method: None,
            outbound_url: None,
            outbound_req_headers: HeaderMap::new(),
            outbound_req_body: Bytes::new(),
            outbound_resp_headers: HeaderMap::new(),
            outbound_have: false,
            latency_header_ms: None,
            result_written: false,
          },
        );
      }
      Event::RequestHeaders {
        request_id,
        ts,
        endpoint_hint,
        path,
        session_id,
        project_id,
        header_initiator,
        local_addr,
        mode,
        route_mode_hint: _,
        inbound_headers,
      } => {
        let endpoint = endpoint_hint.clone().or_else(|| path.clone()).unwrap_or_default();
        let pending_start = self.pending.get(&(request_id.clone(), 0)).cloned();
        let method = pending_start.as_ref().and_then(|p| p.method.as_deref());
        let url = pending_start.as_ref().and_then(|p| p.inbound_url.as_deref());
        let inbound_req = HttpSnapshot {
          method: method.map(str::to_string),
          url: url.map(str::to_string),
          status: None,
          req_headers: inbound_headers.clone(),
          req_body: Bytes::new(),
          resp_headers: HeaderMap::new(),
          resp_body: Bytes::new(),
        };
        if let Err(e) = self.requests.headers(
          request_id,
          requests::HeadersUpdate {
            ts: *ts,
            endpoint: &endpoint,
            session_id: session_id.as_deref(),
            local_addr: local_addr.as_deref(),
            mode: mode.as_deref(),
            method: method.as_deref(),
            inbound_req: &inbound_req,
          },
        ) {
          tracing::warn!(error = %e, "failed to insert/update headers requests db row");
        }
        let key = (request_id.clone(), 0);
        let pending = self.pending.entry(key).or_insert_with(|| PendingRequest {
          ts: *ts,
          session_id: session_id.clone(),
          project_id: project_id.clone(),
          local_addr: local_addr.clone(),
          mode: mode.clone(),
          behave_as: None,
          peer_addr: None,
          method: method.map(str::to_string),
          endpoint: endpoint.clone(),
          model: String::new(),
          initiator: header_initiator.clone().unwrap_or_default(),
          stream: false,
          account_id: String::new(),
          provider_id: String::new(),
          inbound_url: url.map(str::to_string),
          inbound_req_headers: inbound_headers.clone(),
          inbound_req_body: Bytes::new(),
          outbound_method: None,
          outbound_url: None,
          outbound_req_headers: HeaderMap::new(),
          outbound_req_body: Bytes::new(),
          outbound_resp_headers: HeaderMap::new(),
          outbound_have: false,
          latency_header_ms: None,
          result_written: false,
        });
        pending.ts = *ts;
        pending.session_id = session_id.clone().or_else(|| pending.session_id.clone());
        pending.project_id = project_id.clone().or_else(|| pending.project_id.clone());
        pending.local_addr = local_addr.clone().or_else(|| pending.local_addr.clone());
        pending.mode = mode.clone().or_else(|| pending.mode.clone());
        pending.method = pending.method.clone().or_else(|| method.map(str::to_string));
        if !endpoint.is_empty() {
          pending.endpoint = endpoint;
        }
        if let Some(initiator) = header_initiator.clone() {
          pending.initiator = initiator;
        }
        if pending.inbound_url.is_none() {
          pending.inbound_url = url.map(str::to_string);
        }
        pending.inbound_req_headers = inbound_headers.clone();
      }
      Event::RequestParsed {
        request_id,
        attempt,
        account_id,
        provider_id,
        model,
        stream,
        initiator,
        behave_as,
        inbound_body,
      } => {
        // For retry attempts, clone from the base (attempt 0) entry
        let key = (request_id.clone(), *attempt);
        if *attempt > 0 && !self.pending.contains_key(&key) {
          if let Some(base) = self.pending.get(&(request_id.clone(), 0)).cloned() {
            let mut retry = base;
            retry.latency_header_ms = None;
            retry.result_written = false;
            self.pending.insert(key.clone(), retry);
          }
        }
        if let Some(p) = self.pending.get_mut(&key) {
          p.account_id = account_id.clone();
          p.provider_id = provider_id.clone();
          p.model = model.clone();
          p.stream = *stream;
          p.initiator = initiator.clone();
          p.behave_as = behave_as.clone().or_else(|| p.behave_as.clone());
          p.inbound_req_body = inbound_body.clone();
          if let Err(e) = self.requests.parsed(
            request_id,
            *attempt,
            requests::ParsedUpdate {
              ts: p.ts,
              endpoint: &p.endpoint,
              account_id,
              provider_id,
              model,
              initiator,
              stream: *stream,
              behave_as: behave_as.as_deref(),
              inbound_body: inbound_body.clone(),
            },
          ) {
            tracing::warn!(error = %e, "failed to update parsed requests db row");
          }
        }
      }
      Event::RequestResponded {
        request_id,
        attempt,
        outbound_status,
        latency_ms,
        outbound_resp_headers,
        outbound_req_method,
        outbound_req_url,
        outbound_req_headers,
        outbound_req_body,
      } => {
        let key = (request_id.clone(), *attempt);
        if let Some(p) = self.pending.get_mut(&key) {
          p.latency_header_ms = Some(*latency_ms);
          if outbound_req_method.is_some() {
            p.outbound_method = outbound_req_method.clone();
          }
          if outbound_req_url.is_some() {
            p.outbound_url = outbound_req_url.clone();
          }
          if let Some(h) = outbound_req_headers.as_ref() {
            p.outbound_req_headers = h.clone();
          }
          if let Some(b) = outbound_req_body.as_ref() {
            p.outbound_req_body = b.clone();
          }
          p.outbound_resp_headers = outbound_resp_headers.clone();
          p.outbound_have = true;
          if let Err(e) = self.requests.responded(
            p.ts,
            request_id,
            *attempt,
            *latency_ms,
            *outbound_status,
            outbound_resp_headers,
            outbound_req_method.as_deref(),
            outbound_req_url.as_deref(),
            outbound_req_headers.as_ref(),
            outbound_req_body.as_ref(),
          ) {
            tracing::warn!(error = %e, "failed to update responded requests db row");
          }
        }
      }
      Event::RequestResult {
        request_id,
        attempt,
        session_source,
        latency_ms,
        inbound_status,
        usage,
        request_error,
        inbound_resp_headers,
        inbound_resp_body,
        outbound_resp_body,
        messages,
      } => {
        let key = (request_id.clone(), *attempt);
        let composite_id = if *attempt == 0 {
          request_id.clone()
        } else {
          format!("{request_id}:{attempt}")
        };
        let pending = if *attempt == 0 {
          self.pending.get_mut(&key).map(|p| {
            p.result_written = true;
            p.clone()
          })
        } else {
          self.pending.remove(&key)
        };
        let record = if let Some(p) = pending {
          let outbound = if p.outbound_have || outbound_resp_body.is_some() {
            Some(HttpSnapshot {
              method: p.outbound_method.clone(),
              url: p.outbound_url.clone(),
              status: Some(*inbound_status),
              req_headers: p.outbound_req_headers.clone(),
              req_body: p.outbound_req_body.clone(),
              resp_headers: p.outbound_resp_headers.clone(),
              resp_body: outbound_resp_body.clone().unwrap_or_default(),
            })
          } else {
            None
          };
          let inbound = HttpSnapshot {
            method: p.method.clone(),
            url: p.inbound_url.clone(),
            status: Some(*inbound_status),
            req_headers: p.inbound_req_headers.clone(),
            req_body: p.inbound_req_body.clone(),
            resp_headers: inbound_resp_headers.clone(),
            resp_body: inbound_resp_body.clone(),
          };
          CallRecord {
            ts: p.ts,
            session_id: p.session_id.unwrap_or_default(),
            session_source: *session_source,
            user: None,
            local_addr: p.local_addr,
            mode: p.mode,
            behave_as: p.behave_as,
            peer_addr: p.peer_addr,
            method: p.method,
            request_id: composite_id,
            request_error: request_error.clone(),
            project_id: p.project_id,
            endpoint: p.endpoint,
            account_id: p.account_id,
            provider_id: p.provider_id,
            model: p.model,
            initiator: p.initiator,
            status: *inbound_status,
            stream: p.stream,
            latency_ms: Some(*latency_ms),
            latency_header_ms: p.latency_header_ms,
            usage: usage.clone(),
            inbound,
            outbound,
            messages: messages.clone(),
          }
        } else {
          let fallback_ts = Self::fallback_ts();
          tracing::warn!(
            request_id = %request_id,
            attempt = *attempt,
            fallback_ts,
            "RequestResult without prior RequestParsed; persisting with current timestamp"
          );
          CallRecord {
            ts: fallback_ts,
            session_id: String::new(),
            session_source: *session_source,
            user: None,
            local_addr: None,
            mode: None,
            behave_as: None,
            peer_addr: None,
            method: None,
            request_id: composite_id,
            request_error: request_error.clone(),
            project_id: None,
            endpoint: String::new(),
            account_id: String::new(),
            provider_id: String::new(),
            model: String::new(),
            initiator: String::new(),
            status: *inbound_status,
            stream: false,
            latency_ms: Some(*latency_ms),
            latency_header_ms: None,
            usage: usage.clone(),
            inbound: HttpSnapshot {
              status: Some(*inbound_status),
              resp_headers: inbound_resp_headers.clone(),
              resp_body: inbound_resp_body.clone(),
              ..Default::default()
            },
            outbound: outbound_resp_body.as_ref().map(|b| HttpSnapshot {
              status: Some(*inbound_status),
              resp_body: b.clone(),
              ..Default::default()
            }),
            messages: messages.clone(),
          }
        };
        if let Err(e) = self.requests.result(&record) {
          tracing::warn!(error = %e, "failed to update result requests db row");
        }
        write_record(&mut self.usage, &mut self.sessions, &record);
      }
      Event::RequestCompleted {
        request_id,
        success,
        final_status,
        error,
        ..
      } => {
        if !success {
          let key = (request_id.clone(), 0);
          if let Some(p) = self.pending.get(&key).cloned().filter(|p| !p.result_written) {
            let inbound = HttpSnapshot {
              method: p.method.clone(),
              url: p.inbound_url.clone(),
              status: *final_status,
              req_headers: p.inbound_req_headers.clone(),
              req_body: p.inbound_req_body.clone(),
              resp_headers: HeaderMap::new(),
              resp_body: Bytes::new(),
            };
            let outbound = if p.outbound_have {
              Some(HttpSnapshot {
                method: p.outbound_method.clone(),
                url: p.outbound_url.clone(),
                status: *final_status,
                req_headers: p.outbound_req_headers.clone(),
                req_body: p.outbound_req_body.clone(),
                resp_headers: p.outbound_resp_headers.clone(),
                resp_body: Bytes::new(),
              })
            } else {
              None
            };
            let record = CallRecord {
              ts: p.ts,
              session_id: p.session_id.unwrap_or_default(),
              session_source: SessionSource::Header,
              user: None,
              local_addr: p.local_addr,
              mode: p.mode,
              behave_as: p.behave_as,
              peer_addr: p.peer_addr,
              method: p.method,
              request_id: request_id.clone(),
              request_error: error.clone(),
              project_id: p.project_id,
              endpoint: p.endpoint,
              account_id: p.account_id,
              provider_id: p.provider_id,
              model: p.model,
              initiator: p.initiator,
              status: final_status.unwrap_or(0),
              stream: p.stream,
              latency_ms: None,
              latency_header_ms: p.latency_header_ms,
              usage: Usage::default(),
              inbound,
              outbound,
              messages: Vec::new(),
            };
            if let Err(e) = self.requests.result(&record) {
              tracing::warn!(error = %e, "failed to persist completed failure row");
            }
            write_record(&mut self.usage, &mut self.sessions, &record);
          }
        }
        // Clean up any remaining pending state for the base request (all attempts).
        self.pending.retain(|(id, _), _| id != request_id);
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use llm_core::event::{Event, EventHandler};
  use llm_headers::HeaderMap;
  use rusqlite::{params, Connection};

  fn tempdir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("llm-router-db-events-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
  }

  fn make_handler() -> (DbEventHandler, std::path::PathBuf) {
    let dir = tempdir();
    let paths = DbPaths {
      usage_db: dir.join("usage.db"),
      sessions_db: dir.join("sessions.db"),
      requests_dir: dir.join("requests"),
    };
    std::fs::create_dir_all(&paths.requests_dir).unwrap();
    let h = DbEventHandler::new(paths).unwrap();
    (h, dir)
  }

  fn started(req_id: &str, ts: i64) -> Event {
    Event::RequestStarted {
      request_id: req_id.into(),
      ts,
      endpoint: "chat_completions".into(),
      session_id: Some("sess-1".into()),
      peer_addr: Some("127.0.0.1:4142".into()),
      local_addr: Some("127.0.0.1:4141".into()),
      method: "POST".into(),
      url: Some("https://example.test/v1/responses".into()),
    }
  }

  fn started_without_peer(req_id: &str, ts: i64) -> Event {
    Event::RequestStarted {
      request_id: req_id.into(),
      ts,
      endpoint: "chat_completions".into(),
      session_id: Some("sess-1".into()),
      peer_addr: None,
      local_addr: None,
      method: "POST".into(),
      url: Some("https://example.test/v1/responses".into()),
    }
  }

  fn parsed(req_id: &str, attempt: u32) -> Event {
    Event::RequestParsed {
      request_id: req_id.into(),
      attempt,
      account_id: "acct".into(),
      provider_id: "prov".into(),
      model: "m".into(),
      stream: false,
      initiator: "user".into(),
      behave_as: Some("architect".into()),
      inbound_body: bytes::Bytes::new(),
    }
  }

  fn headers(req_id: &str, ts: i64) -> Event {
    Event::RequestHeaders {
      request_id: req_id.into(),
      ts,
      endpoint_hint: Some("responses".into()),
      path: Some("/v1/responses".into()),
      session_id: Some("sess-1".into()),
      project_id: Some("proj-1".into()),
      header_initiator: Some("user".into()),
      local_addr: Some("localhost:4141".into()),
      mode: Some("route".into()),
      route_mode_hint: Some("route".into()),
      inbound_headers: HeaderMap::new(),
    }
  }

  fn result(req_id: &str, attempt: u32, status: u16, error: Option<&str>) -> Event {
    Event::RequestResult {
      request_id: req_id.into(),
      attempt,
      session_source: SessionSource::Header,
      latency_ms: 10,
      inbound_status: status,
      usage: Usage::default(),
      request_error: error.map(str::to_string),
      inbound_resp_headers: HeaderMap::new(),
      inbound_resp_body: bytes::Bytes::new(),
      outbound_resp_body: None,
      messages: vec![],
    }
  }

  fn responded(req_id: &str, attempt: u32, latency_ms: u64) -> Event {
    Event::RequestResponded {
      request_id: req_id.into(),
      attempt,
      outbound_status: 200,
      latency_ms,
      outbound_resp_headers: HeaderMap::new(),
      outbound_req_method: None,
      outbound_req_url: None,
      outbound_req_headers: None,
      outbound_req_body: None,
    }
  }

  fn completed(
    req_id: &str,
    success: bool,
    total_attempts: u32,
    final_status: Option<u16>,
    error: Option<&str>,
  ) -> Event {
    Event::RequestCompleted {
      request_id: req_id.into(),
      success,
      total_attempts,
      final_status,
      total_latency_ms: 100,
      error: error.map(str::to_string),
    }
  }

  /// Open all `*.db` files in requests dir and return selected request row fields.
  fn fetch_rows(dir: &std::path::Path) -> Vec<(String, Option<i64>, Option<String>, Option<i64>)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn
        .prepare("SELECT request_id, status, request_error, latency_header_ms FROM requests ORDER BY request_id")
        .unwrap();
      let iter = stmt
        .query_map([], |r| {
          Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<i64>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<i64>>(3)?,
          ))
        })
        .unwrap();
      for r in iter {
        rows.push(r.unwrap());
      }
    }
    rows.sort();
    rows
  }

  fn fetch_request_timestamps(dir: &std::path::Path) -> Vec<(String, i64)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn
        .prepare("SELECT request_id, ts FROM requests ORDER BY request_id")
        .unwrap();
      let iter = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
        .unwrap();
      for r in iter {
        rows.push(r.unwrap());
      }
    }
    rows.sort();
    rows
  }

  fn fetch_sessions(dir: &std::path::Path) -> Vec<(String, Option<String>)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn
        .prepare("SELECT request_id, session_id FROM requests ORDER BY request_id")
        .unwrap();
      let iter = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))
        .unwrap();
      for r in iter {
        rows.push(r.unwrap());
      }
    }
    rows.sort();
    rows
  }

  fn fetch_peer_addr_and_method(dir: &std::path::Path) -> Vec<(String, Option<String>, Option<String>)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn
        .prepare("SELECT request_id, peer_addr, method FROM requests ORDER BY request_id")
        .unwrap();
      let iter = stmt
        .query_map([], |r| {
          Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
          ))
        })
        .unwrap();
      for r in iter {
        rows.push(r.unwrap());
      }
    }
    rows.sort();
    rows
  }

  fn fetch_endpoint_and_headers(dir: &std::path::Path) -> Vec<(String, String, String)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn
        .prepare("SELECT request_id, endpoint, inbound_req_headers FROM requests ORDER BY request_id")
        .unwrap();
      let iter = stmt
        .query_map([], |r| {
          Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            String::from_utf8(r.get::<_, Vec<u8>>(2)?).unwrap_or_default(),
          ))
        })
        .unwrap();
      for r in iter {
        rows.push(r.unwrap());
      }
    }
    rows.sort();
    rows
  }

  fn fetch_context(dir: &std::path::Path) -> Vec<(String, Option<String>, Option<String>, Option<String>)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn
        .prepare("SELECT request_id, local_addr, mode, behave_as FROM requests ORDER BY request_id")
        .unwrap();
      let iter = stmt
        .query_map([], |r| {
          Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
          ))
        })
        .unwrap();
      for r in iter {
        rows.push(r.unwrap());
      }
    }
    rows.sort();
    rows
  }

  #[test]
  fn first_attempt_success_writes_one_row() {
    let (mut h, dir) = make_handler();
    let req = "req-1";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&parsed(req, 0));
    h.handle(&result(req, 0, 200, None));
    h.handle(&completed(req, true, 1, Some(200), None));

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 1, "expected exactly one row, got {rows:?}");
    assert_eq!(rows[0].0, "req-1");
    assert_eq!(rows[0].1, Some(200));
    assert_eq!(rows[0].2, None);
  }

  #[test]
  fn response_header_latency_is_recorded() {
    let (mut h, dir) = make_handler();
    let req = "req-header-latency";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&parsed(req, 0));
    h.handle(&responded(req, 0, 42));
    h.handle(&result(req, 0, 200, None));
    h.handle(&completed(req, true, 1, Some(200), None));

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 1, "expected exactly one row, got {rows:?}");
    assert_eq!(rows[0].0, "req-header-latency");
    assert_eq!(rows[0].3, Some(42));
  }

  #[test]
  fn request_started_session_id_is_preserved() {
    let (mut h, dir) = make_handler();
    let req = "req-session";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&parsed(req, 0));
    h.handle(&responded(req, 0, 42));
    h.handle(&result(req, 0, 200, None));

    let rows = fetch_sessions(&dir);
    assert_eq!(rows, vec![("req-session".into(), Some("sess-1".into()))]);
  }

  #[test]
  fn request_started_with_peer_persists_peer_addr_and_method() {
    let (mut h, dir) = make_handler();
    let req = "req-peer";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));

    let rows = fetch_peer_addr_and_method(&dir);
    assert_eq!(
      rows,
      vec![("req-peer".into(), Some("127.0.0.1:4142".into()), Some("POST".into()))]
    );
  }

  #[test]
  fn request_started_without_peer_leaves_peer_addr_null() {
    let (mut h, dir) = make_handler();
    let req = "req-no-peer";
    let ts = 1_700_000_000;
    h.handle(&started_without_peer(req, ts));

    let rows = fetch_peer_addr_and_method(&dir);
    assert_eq!(rows, vec![("req-no-peer".into(), None, Some("POST".into()))]);
  }

  #[test]
  fn headers_update_existing_started_row_without_reinserting() {
    let (mut h, dir) = make_handler();
    let req = "req-headers";
    let ts = 1_700_000_000;
    let mut event = headers(req, ts);
    if let Event::RequestHeaders { inbound_headers, .. } = &mut event {
      inbound_headers.insert("x-test", "1");
    }

    h.handle(&started(req, ts));
    h.handle(&event);

    let rows = fetch_endpoint_and_headers(&dir);
    assert_eq!(rows.len(), 1, "expected one row after headers update, got {rows:?}");
    assert_eq!(rows[0].0, "req-headers");
    assert_eq!(rows[0].1, "responses");
    assert!(rows[0].2.contains("\"x-test\":\"1\""));
  }

  #[test]
  fn context_columns_are_populated_from_headers_and_parsed() {
    let (mut h, dir) = make_handler();
    let req = "req-context";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&headers(req, ts));
    h.handle(&parsed(req, 0));

    let rows = fetch_context(&dir);
    assert_eq!(
      rows,
      vec![(
        "req-context".into(),
        Some("localhost:4141".into()),
        Some("route".into()),
        Some("architect".into())
      )]
    );
  }

  /// Lifecycle persistence inserts a partial row before a final result exists,
  /// so interrupted requests are still visible in the request DB.
  #[test]
  fn started_and_parsed_without_result_writes_partial_row() {
    let (mut h, dir) = make_handler();
    let req = "req-no-result";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&parsed(req, 0));
    // No RequestResult, no RequestCompleted.

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 1, "expected one partial row, got {rows:?}");
    assert_eq!(rows[0].0, "req-no-result");
    assert_eq!(rows[0].1, None);
  }

  #[test]
  fn started_and_headers_then_completed_failure_writes_error_row() {
    let (mut h, dir) = make_handler();
    let req = "req-parse-fail";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&headers(req, ts));
    h.handle(&completed(req, false, 1, Some(400), Some("invalid JSON request body")));

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 1, "expected one failure row, got {rows:?}");
    assert_eq!(rows[0].0, "req-parse-fail");
    assert_eq!(rows[0].1, Some(400));
    assert_eq!(rows[0].2.as_deref(), Some("invalid JSON request body"));
    let peer_rows = fetch_peer_addr_and_method(&dir);
    assert_eq!(
      peer_rows,
      vec![(
        "req-parse-fail".into(),
        Some("127.0.0.1:4142".into()),
        Some("POST".into())
      )]
    );
  }

  /// A `RequestResult` for an attempt must persist a row immediately, even if
  /// the terminal `RequestCompleted` never arrives (e.g., process killed
  /// mid-stream, or the completion event is dropped). DB writes are driven by
  /// per-attempt results, not by the request-level terminal event.
  #[test]
  fn attempt_result_without_completed_writes_row() {
    let (mut h, dir) = make_handler();
    let req = "req-no-complete";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    h.handle(&parsed(req, 0));
    h.handle(&result(req, 0, 200, None));
    // Intentionally no RequestCompleted.

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 1, "expected exactly one row, got {rows:?}");
    assert_eq!(rows[0].0, "req-no-complete");
    assert_eq!(rows[0].1, Some(200));
    assert_eq!(rows[0].2, None);
  }

  #[test]
  fn orphan_result_uses_current_timestamp_instead_of_epoch() {
    let (mut h, dir) = make_handler();
    let req = "req-orphan";

    h.handle(&result(req, 0, 200, None));

    let rows = fetch_request_timestamps(&dir);
    assert_eq!(rows.len(), 1, "expected one orphan result row, got {rows:?}");
    assert_eq!(rows[0].0, "req-orphan");
    assert!(rows[0].1 > 0, "expected current timestamp fallback, got {rows:?}");
  }

  #[test]
  fn retry_then_success_writes_two_rows() {
    let (mut h, dir) = make_handler();
    let req = "req-2";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    // Attempt 0: fails
    h.handle(&parsed(req, 0));
    h.handle(&Event::RequestRetry {
      request_id: req.into(),
      attempt: 0,
      error: "upstream 500".into(),
    });
    h.handle(&result(req, 0, 500, Some("upstream 500")));
    // Attempt 1: succeeds
    h.handle(&parsed(req, 1));
    h.handle(&result(req, 1, 200, None));
    h.handle(&completed(req, true, 2, Some(200), None));

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 2, "expected two rows, got {rows:?}");
    assert_eq!(rows[0].0, "req-2");
    assert_eq!(rows[0].1, Some(500));
    assert_eq!(rows[0].2.as_deref(), Some("upstream 500"));
    assert_eq!(rows[1].0, "req-2:1");
    assert_eq!(rows[1].1, Some(200));
    assert_eq!(rows[1].2, None);
    let peer_rows = fetch_peer_addr_and_method(&dir);
    assert_eq!(
      peer_rows,
      vec![
        ("req-2".into(), Some("127.0.0.1:4142".into()), Some("POST".into())),
        ("req-2:1".into(), Some("127.0.0.1:4142".into()), Some("POST".into())),
      ]
    );
  }

  #[test]
  fn all_attempts_failed_writes_three_rows() {
    let (mut h, dir) = make_handler();
    let req = "req-3";
    let ts = 1_700_000_000;
    h.handle(&started(req, ts));
    for attempt in 0..3u32 {
      h.handle(&parsed(req, attempt));
      h.handle(&Event::RequestRetry {
        request_id: req.into(),
        attempt,
        error: format!("err-{attempt}"),
      });
      h.handle(&result(req, attempt, 500, Some(&format!("err-{attempt}"))));
    }
    h.handle(&completed(req, false, 3, None, Some("all attempts failed")));

    let rows = fetch_rows(&dir);
    assert_eq!(rows.len(), 3, "expected three rows, got {rows:?}");
    assert_eq!(rows[0].0, "req-3");
    assert_eq!(rows[1].0, "req-3:1");
    assert_eq!(rows[2].0, "req-3:2");
    for r in &rows {
      assert_eq!(r.1, Some(500));
      assert!(r.2.is_some());
    }
  }

  // ---------------------------------------------------------------------------
  // Per-event field-persistence test.
  //
  // Drives the full lifecycle (Started -> Headers -> Parsed -> Responded ->
  // Result -> Completed) with non-default values for every field, and after
  // each event verifies the columns the writer is responsible for setting at
  // that point match the event's payload. Locks in the current upsert/COALESCE
  // behavior as a regression net for the field-rename refactor.
  // ---------------------------------------------------------------------------

  /// Read the single matching row from any per-day requests db as a column-name
  /// keyed map. Strings come back as Some(String); blobs as Some(String) via
  /// from_utf8 (good enough for header-JSON / body assertions in this test).
  fn fetch_row_map(
    dir: &std::path::Path,
    request_id: &str,
  ) -> std::collections::HashMap<String, rusqlite::types::Value> {
    use rusqlite::types::Value;
    let mut out: Option<std::collections::HashMap<String, Value>> = None;
    for entry in std::fs::read_dir(dir.join("requests")).unwrap() {
      let p = entry.unwrap().path();
      if p.extension().and_then(|e| e.to_str()) != Some("db") {
        continue;
      }
      let conn = Connection::open(&p).unwrap();
      let mut stmt = conn.prepare("SELECT * FROM requests WHERE request_id = ?1").unwrap();
      let col_names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
      let mut rows = stmt.query(params![request_id]).unwrap();
      if let Some(row) = rows.next().unwrap() {
        let mut m = std::collections::HashMap::new();
        for (i, name) in col_names.iter().enumerate() {
          let v: Value = row.get(i).unwrap();
          m.insert(name.clone(), v);
        }
        out = Some(m);
        break;
      }
    }
    out.unwrap_or_else(|| panic!("no row found for request_id={request_id}"))
  }

  fn as_text(v: &rusqlite::types::Value) -> Option<String> {
    use rusqlite::types::Value;
    match v {
      Value::Text(s) => Some(s.clone()),
      Value::Blob(b) => Some(String::from_utf8_lossy(b).to_string()),
      _ => None,
    }
  }
  fn as_int(v: &rusqlite::types::Value) -> Option<i64> {
    use rusqlite::types::Value;
    match v {
      Value::Integer(i) => Some(*i),
      _ => None,
    }
  }
  fn is_null(v: &rusqlite::types::Value) -> bool {
    matches!(v, rusqlite::types::Value::Null)
  }

  #[test]
  fn every_event_persists_its_fields() {
    let (mut h, dir) = make_handler();
    let req = "req-full";
    let ts: i64 = 1_700_000_000;

    // --- 1. RequestStarted ---------------------------------------------------
    let started = Event::RequestStarted {
      request_id: req.into(),
      ts,
      endpoint: "chat_completions".into(),
      session_id: Some("sess-full".into()),
      peer_addr: Some("10.0.0.1:9999".into()),
      local_addr: Some("127.0.0.1:4141".into()),
      method: "POST".into(),
      url: Some("https://upstream.test/v1/responses".into()),
    };
    h.handle(&started);
    {
      let row = fetch_row_map(&dir, req);
      assert_eq!(as_int(&row["ts"]), Some(ts), "ts after RequestStarted");
      assert_eq!(as_text(&row["session_id"]).as_deref(), Some("sess-full"));
      assert_eq!(as_text(&row["peer_addr"]).as_deref(), Some("10.0.0.1:9999"));
      assert_eq!(as_text(&row["method"]).as_deref(), Some("POST"));
      assert_eq!(as_text(&row["endpoint"]).as_deref(), Some("chat_completions"));
      assert_eq!(as_text(&row["inbound_req_method"]).as_deref(), Some("POST"));
      assert_eq!(
        as_text(&row["inbound_req_url"]).as_deref(),
        Some("https://upstream.test/v1/responses")
      );
      // Fields not yet populated.
      assert!(is_null(&row["status"]), "status null after Started");
      assert!(is_null(&row["latency_ms"]), "latency_ms null after Started");
      assert!(
        is_null(&row["latency_header_ms"]),
        "latency_header_ms null after Started"
      );
      assert!(is_null(&row["input_tok"]));
      assert!(is_null(&row["output_tok"]));
      assert!(is_null(&row["outbound_resp_status"]));
      assert!(is_null(&row["inbound_resp_status"]));
      // account_id/provider_id/model/initiator are inserted as empty strings.
      assert_eq!(as_text(&row["account_id"]).as_deref(), Some(""));
      assert_eq!(as_text(&row["provider_id"]).as_deref(), Some(""));
      assert_eq!(as_text(&row["model"]).as_deref(), Some(""));
      assert_eq!(as_text(&row["initiator"]).as_deref(), Some(""));
    }

    // --- 2. RequestHeaders ---------------------------------------------------
    let mut inbound_headers = HeaderMap::new();
    inbound_headers.insert("x-request-id", "abc-123");
    let headers_event = Event::RequestHeaders {
      request_id: req.into(),
      ts,
      endpoint_hint: Some("responses".into()),
      path: Some("/v1/responses".into()),
      session_id: Some("sess-full".into()),
      project_id: Some("proj-full".into()),
      header_initiator: Some("system".into()),
      local_addr: Some("127.0.0.1:4141".into()),
      mode: Some("route".into()),
      route_mode_hint: Some("route".into()),
      inbound_headers: inbound_headers.clone(),
    };
    h.handle(&headers_event);
    {
      let row = fetch_row_map(&dir, req);
      // Endpoint overwritten by hint.
      assert_eq!(as_text(&row["endpoint"]).as_deref(), Some("responses"));
      // inbound_req_headers now contains the JSON-encoded header.
      let hdr_json = as_text(&row["inbound_req_headers"]).unwrap_or_default();
      assert!(
        hdr_json.contains("\"x-request-id\":\"abc-123\""),
        "expected header in JSON, got: {hdr_json}"
      );
      // COALESCE keeps prior values.
      assert_eq!(as_text(&row["session_id"]).as_deref(), Some("sess-full"));
      assert_eq!(as_text(&row["method"]).as_deref(), Some("POST"));
      assert_eq!(as_text(&row["inbound_req_method"]).as_deref(), Some("POST"));
      assert_eq!(
        as_text(&row["inbound_req_url"]).as_deref(),
        Some("https://upstream.test/v1/responses")
      );
      // header_initiator and project_id are NOT written to the requests row by
      // RequestHeaders (they live only in the in-memory pending state until
      // RequestParsed/RequestResult).
      assert_eq!(as_text(&row["initiator"]).as_deref(), Some(""));
    }

    // --- 3. RequestParsed ----------------------------------------------------
    let parsed_event = Event::RequestParsed {
      request_id: req.into(),
      attempt: 0,
      account_id: "acct-full".into(),
      provider_id: "prov-full".into(),
      model: "gpt-test".into(),
      stream: true,
      initiator: "user".into(),
      behave_as: Some("architect".into()),
      inbound_body: bytes::Bytes::from_static(b"{\"hello\":\"world\"}"),
    };
    h.handle(&parsed_event);
    {
      let row = fetch_row_map(&dir, req);
      assert_eq!(as_text(&row["account_id"]).as_deref(), Some("acct-full"));
      assert_eq!(as_text(&row["provider_id"]).as_deref(), Some("prov-full"));
      assert_eq!(as_text(&row["model"]).as_deref(), Some("gpt-test"));
      assert_eq!(as_text(&row["initiator"]).as_deref(), Some("user"));
      assert_eq!(as_int(&row["stream"]), Some(1));
      assert_eq!(
        as_text(&row["inbound_req_body"]).as_deref(),
        Some("{\"hello\":\"world\"}")
      );
      // Endpoint is rewritten by RequestParsed using pending.endpoint, which
      // RequestHeaders updated to "responses".
      assert_eq!(as_text(&row["endpoint"]).as_deref(), Some("responses"));
      // Still not set.
      assert!(is_null(&row["status"]));
      assert!(is_null(&row["latency_ms"]));
    }

    // --- 4. RequestResponded -------------------------------------------------
    let mut outbound_resp_headers = HeaderMap::new();
    outbound_resp_headers.insert("x-upstream", "yes");
    let mut outbound_req_headers = HeaderMap::new();
    outbound_req_headers.insert("x-custom", "yes");
    let responded_event = Event::RequestResponded {
      request_id: req.into(),
      attempt: 0,
      outbound_status: 201,
      latency_ms: 42,
      outbound_resp_headers: outbound_resp_headers.clone(),
      outbound_req_method: Some("POST".into()),
      outbound_req_url: Some("https://upstream.test/v1/chat".into()),
      outbound_req_headers: Some(outbound_req_headers.clone()),
      outbound_req_body: Some(bytes::Bytes::from_static(b"{\"out\":true}")),
    };
    h.handle(&responded_event);
    {
      let row = fetch_row_map(&dir, req);
      // status mirrors outbound_status here.
      assert_eq!(as_int(&row["status"]), Some(201));
      assert_eq!(as_int(&row["outbound_resp_status"]), Some(201));
      assert_eq!(as_int(&row["latency_header_ms"]), Some(42));
      let resp_hdr = as_text(&row["outbound_resp_headers"]).unwrap_or_default();
      assert!(resp_hdr.contains("\"x-upstream\":\"yes\""), "got: {resp_hdr}");
      assert_eq!(as_text(&row["outbound_req_method"]).as_deref(), Some("POST"));
      assert_eq!(
        as_text(&row["outbound_req_url"]).as_deref(),
        Some("https://upstream.test/v1/chat")
      );
      let req_hdr = as_text(&row["outbound_req_headers"]).unwrap_or_default();
      assert!(req_hdr.contains("\"x-custom\":\"yes\""), "got: {req_hdr}");
      assert_eq!(as_text(&row["outbound_req_body"]).as_deref(), Some("{\"out\":true}"));
    }

    // --- 5. RequestResult ----------------------------------------------------
    let mut inbound_resp_headers_map = HeaderMap::new();
    inbound_resp_headers_map.insert("content-type", "application/json");
    let usage = Usage {
      input_tokens: Some(11),
      output_tokens: Some(22),
      details: UsageDetails {
        cache_read: Some(3),
        reasoning: Some(4),
      },
    };
    let result_event = Event::RequestResult {
      request_id: req.into(),
      attempt: 0,
      session_source: SessionSource::Header,
      latency_ms: 123,
      inbound_status: 200,
      usage,
      request_error: None,
      inbound_resp_headers: inbound_resp_headers_map.clone(),
      inbound_resp_body: bytes::Bytes::from_static(b"{\"ok\":true}"),
      outbound_resp_body: Some(bytes::Bytes::from_static(b"{\"upstream\":\"body\"}")),
      messages: vec![],
    };
    h.handle(&result_event);
    {
      let row = fetch_row_map(&dir, req);
      // status now overwritten to inbound_status (200).
      assert_eq!(as_int(&row["status"]), Some(200));
      assert_eq!(as_int(&row["inbound_resp_status"]), Some(200));
      assert_eq!(as_int(&row["latency_ms"]), Some(123));
      // latency_header_ms preserved by COALESCE.
      assert_eq!(as_int(&row["latency_header_ms"]), Some(42));
      assert_eq!(as_int(&row["input_tok"]), Some(11));
      assert_eq!(as_int(&row["output_tok"]), Some(22));
      assert_eq!(as_int(&row["cached_tok"]), Some(3));
      assert_eq!(as_int(&row["reasoning_tok"]), Some(4));
      let inbound_hdr = as_text(&row["inbound_resp_headers"]).unwrap_or_default();
      assert!(
        inbound_hdr.contains("\"content-type\":\"application/json\""),
        "got: {inbound_hdr}"
      );
      assert_eq!(as_text(&row["inbound_resp_body"]).as_deref(), Some("{\"ok\":true}"));
      assert_eq!(
        as_text(&row["outbound_resp_body"]).as_deref(),
        Some("{\"upstream\":\"body\"}")
      );
      assert!(is_null(&row["request_error"]));
      assert_eq!(as_int(&row["stream"]), Some(1));
      assert_eq!(as_text(&row["account_id"]).as_deref(), Some("acct-full"));
      assert_eq!(as_text(&row["provider_id"]).as_deref(), Some("prov-full"));
      assert_eq!(as_text(&row["model"]).as_deref(), Some("gpt-test"));
      assert_eq!(as_text(&row["initiator"]).as_deref(), Some("user"));
    }

    // --- 6. RequestCompleted (success) --------------------------------------
    let completed_event = Event::RequestCompleted {
      request_id: req.into(),
      success: true,
      total_attempts: 1,
      final_status: Some(200),
      total_latency_ms: 200,
      error: None,
    };
    h.handle(&completed_event);
    {
      // Row should remain intact; RequestCompleted only cleans up pending state
      // (and on failures may write a row, but here success=true and a Result
      // already wrote the row).
      let row = fetch_row_map(&dir, req);
      assert_eq!(as_int(&row["status"]), Some(200));
      assert_eq!(as_int(&row["latency_ms"]), Some(123));
      assert_eq!(as_int(&row["input_tok"]), Some(11));
      assert!(is_null(&row["request_error"]));
    }
  }
}
