pub mod archive;
pub mod migrate;
pub mod requests;
pub mod sessions;
pub mod usage;

use bytes::Bytes;
use snafu::Snafu;

pub use llm_core::db::{CallRecord, DbPaths, HttpSnapshot, MessageRecord, PartRecord};
#[allow(unused_imports)]
pub(crate) use llm_core::db::{Usage, UsageDetails};
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
  sessions: &mut Option<sessions::SessionsDb>,
  record: &CallRecord,
) {
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
  endpoint: String,
  model: String,
  initiator: String,
  stream: bool,
  account_id: String,
  provider_id: String,
  inbound_req: HttpSnapshot,
  outbound_req: Option<HttpSnapshot>,
  latency_header_ms: Option<u64>,
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
}

impl EventHandler for DbEventHandler {
  fn handle(&mut self, event: &Event) {
    match event {
      Event::RequestStarted {
        request_id,
        ts,
        endpoint,
        initiator,
        session_id,
        project_id,
        inbound_req,
      } => {
        if let Err(e) = self
          .requests
          .started(request_id, *ts, endpoint, session_id.as_deref(), inbound_req)
        {
          tracing::warn!(error = %e, "failed to insert started requests db row");
        }
        self.pending.insert(
          (request_id.clone(), 0),
          PendingRequest {
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
            latency_header_ms: None,
          },
        );
      }
      Event::RequestParsed {
        request_id,
        attempt,
        account_id,
        provider_id,
        model,
        stream,
        initiator,
        outbound_req,
      } => {
        // For retry attempts, clone from the base (attempt 0) entry
        let key = (request_id.clone(), *attempt);
        if *attempt > 0 && !self.pending.contains_key(&key) {
          if let Some(base) = self.pending.get(&(request_id.clone(), 0)).cloned() {
            let mut retry = base;
            retry.latency_header_ms = None;
            self.pending.insert(key.clone(), retry);
          }
        }
        if let Some(p) = self.pending.get_mut(&key) {
          p.account_id = account_id.clone();
          p.provider_id = provider_id.clone();
          p.model = model.clone();
          p.stream = *stream;
          p.initiator = initiator.clone();
          p.outbound_req = outbound_req.clone();
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
              outbound_req: outbound_req.as_ref(),
            },
          ) {
            tracing::warn!(error = %e, "failed to update parsed requests db row");
          }
        }
      }
      Event::RequestResponded {
        request_id,
        attempt,
        status,
        latency_ms,
        resp_headers,
        ..
      } => {
        let key = (request_id.clone(), *attempt);
        if let Some(p) = self.pending.get_mut(&key) {
          p.latency_header_ms = Some(*latency_ms);
          if let Err(e) = self.requests.responded(p.ts, request_id, *attempt, *latency_ms, *status, resp_headers) {
            tracing::warn!(error = %e, "failed to update responded requests db row");
          }
        }
      }
      Event::RequestResult {
        request_id,
        attempt,
        session_source,
        latency_ms,
        status,
        usage,
        request_error,
        inbound_resp,
        outbound_resp,
        messages,
      } => {
        let key = (request_id.clone(), *attempt);
        let composite_id = if *attempt == 0 {
          request_id.clone()
        } else {
          format!("{request_id}:{attempt}")
        };
        let pending = self.pending.remove(&key);
        let record = if let Some(p) = pending {
          CallRecord {
            ts: p.ts,
            session_id: p.session_id.unwrap_or_default(),
            session_source: *session_source,
            request_id: composite_id,
            request_error: request_error.clone(),
            project_id: p.project_id,
            endpoint: p.endpoint,
            account_id: p.account_id,
            provider_id: p.provider_id,
            model: p.model,
            initiator: p.initiator,
            status: *status,
            stream: p.stream,
            latency_ms: Some(*latency_ms),
            latency_header_ms: p.latency_header_ms,
            usage: usage.clone(),
            inbound_req: p.inbound_req,
            outbound_req: p.outbound_req,
            outbound_resp: outbound_resp.clone(),
            inbound_resp: inbound_resp.clone(),
            messages: messages.clone(),
          }
        } else {
          tracing::debug!(request_id = %request_id, attempt = *attempt, "RequestResult without prior RequestParsed");
          CallRecord {
            ts: 0,
            session_id: String::new(),
            session_source: *session_source,
            request_id: composite_id,
            request_error: request_error.clone(),
            project_id: None,
            endpoint: String::new(),
            account_id: String::new(),
            provider_id: String::new(),
            model: String::new(),
            initiator: String::new(),
            status: *status,
            stream: false,
            latency_ms: Some(*latency_ms),
            latency_header_ms: None,
            usage: usage.clone(),
            inbound_req: HttpSnapshot::default(),
            outbound_req: None,
            outbound_resp: outbound_resp.clone(),
            inbound_resp: inbound_resp.clone(),
            messages: messages.clone(),
          }
        };
        if let Err(e) = self.requests.result(&record) {
          tracing::warn!(error = %e, "failed to update result requests db row");
        }
        write_record(&mut self.usage, &mut self.sessions, &record);
      }
      Event::RequestCompleted { request_id, .. } => {
        // Clean up any remaining pending state for the base request (all attempts)
        self.pending.retain(|(id, _), _| id != request_id);
      }
      _ => {}
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use llm_core::db::HttpSnapshot;
  use llm_core::event::{Event, EventHandler};
  use reqwest::header::HeaderMap;
  use rusqlite::Connection;

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
      initiator: Some("user".into()),
      session_id: Some("sess-1".into()),
      project_id: None,
      inbound_req: HttpSnapshot::default(),
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
      outbound_req: None,
    }
  }

  fn result(req_id: &str, attempt: u32, status: u16, error: Option<&str>) -> Event {
    Event::RequestResult {
      request_id: req_id.into(),
      attempt,
      session_source: SessionSource::Header,
      latency_ms: 10,
      status,
      usage: Usage::default(),
      request_error: error.map(str::to_string),
      inbound_resp: HttpSnapshot {
        method: None,
        url: None,
        status: Some(status),
        headers: HeaderMap::new(),
        body: bytes::Bytes::new(),
      },
      outbound_resp: None,
      messages: vec![],
    }
  }

  fn responded(req_id: &str, attempt: u32, latency_ms: u64) -> Event {
    Event::RequestResponded {
      request_id: req_id.into(),
      attempt,
      status: 200,
      latency_ms,
      resp_headers: HeaderMap::new(),
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
        .prepare(
          "SELECT request_id, status, request_error, latency_header_ms FROM requests ORDER BY request_id",
        )
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
}
