use super::{headers_json, migrate, CallRecord, HttpSnapshot, Result};
use rusqlite::{params, Connection};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use time::macros::format_description;

const CACHE_CAP: usize = 3;
const BOOTSTRAP: &str = include_str!("../../../../scripts/migrations/requests/000_bootstrap.sql");
const MIGRATIONS: &[migrate::Migration] = &[
  migrate::Migration {
    version: 1,
    name: "initial",
    sql: include_str!("../../../../scripts/migrations/requests/001_initial.sql"),
  },
  migrate::Migration {
    version: 2,
    name: "add_correlation_and_error",
    sql: include_str!("../../../../scripts/migrations/requests/002_add_correlation_and_error.sql"),
  },
  migrate::Migration {
    version: 3,
    name: "add_usage_breakdown",
    sql: include_str!("../../../../scripts/migrations/requests/003_add_usage_breakdown.sql"),
  },
  migrate::Migration {
    version: 4,
    name: "add_response_header_latency",
    sql: include_str!("../../../../scripts/migrations/requests/004_add_response_header_latency.sql"),
  },
  migrate::Migration {
    version: 5,
    name: "add_source_and_method",
    sql: include_str!("../../../../scripts/migrations/requests/005_add_source_and_method.sql"),
  },
];

pub fn latest_version() -> u32 {
  migrate::latest_version(MIGRATIONS)
}

pub struct RequestsDb {
  dir: PathBuf,
  conns: HashMap<String, Connection>,
  order: VecDeque<String>,
}

impl RequestsDb {
  pub fn new(dir: PathBuf) -> Result<Self> {
    std::fs::create_dir_all(&dir)?;
    Ok(Self {
      dir,
      conns: HashMap::new(),
      order: VecDeque::new(),
    })
  }

  /// Iterate every existing day file under `dir` (without opening them).
  pub fn day_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if !dir.exists() {
      return Ok(out);
    }
    for entry in std::fs::read_dir(dir)? {
      let entry = entry?;
      let path = entry.path();
      if path.extension().and_then(|s| s.to_str()) == Some("db") {
        out.push(path);
      }
    }
    Ok(out)
  }

  #[cfg(test)]
  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    let conn = self.conn_for_ts(r.ts)?;
    let inbound_req_headers = headers_json(&r.inbound_req.headers);
    let outbound_req_headers = r.outbound_req.as_ref().map(|s| headers_json(&s.headers));
    let outbound_resp_headers = r.outbound_resp.as_ref().map(|s| headers_json(&s.headers));
    let inbound_resp_headers = headers_json(&r.inbound_resp.headers);

    conn.execute(
      "INSERT INTO requests (ts, session_id, source, method, request_id, request_error, endpoint, account_id, provider_id, model, initiator, status, stream, latency_ms, latency_header_ms,
                             input_tok, output_tok, cached_tok, reasoning_tok,
                             inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body,
                             outbound_req_method, outbound_req_url, outbound_req_headers, outbound_req_body,
                             outbound_resp_status, outbound_resp_headers, outbound_resp_body,
                             inbound_resp_status, inbound_resp_headers, inbound_resp_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19,
               ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)",
      params![
        r.ts,
        r.session_id,
        Option::<String>::None,
        r.inbound_req.method.clone(),
        r.request_id,
        r.request_error,
        r.endpoint,
        r.account_id,
        r.provider_id,
        r.model,
        r.initiator,
        r.status as i64,
        r.stream as i64,
        r.latency_ms.map(|v| v as i64),
        r.latency_header_ms.map(|v| v as i64),
        r.usage.input_tokens.map(|v| v as i64),
        r.usage.output_tokens.map(|v| v as i64),
        r.usage.details.cache_read.map(|v| v as i64),
        r.usage.details.reasoning.map(|v| v as i64),
        r.inbound_req.method.as_deref(),
        r.inbound_req.url.as_deref(),
        inbound_req_headers.as_ref(),
        r.inbound_req.body.as_ref(),
        opt_str(r.outbound_req.as_ref(), |s| s.method.as_deref()),
        opt_str(r.outbound_req.as_ref(), |s| s.url.as_deref()),
        outbound_req_headers.as_ref().map(|b| b.as_ref()),
        r.outbound_req.as_ref().map(|s| s.body.as_ref()),
        r.outbound_resp.as_ref().and_then(|s| s.status).map(|v| v as i64),
        outbound_resp_headers.as_ref().map(|b| b.as_ref()),
        r.outbound_resp.as_ref().map(|s| s.body.as_ref()),
        r.inbound_resp.status.map(|v| v as i64),
        inbound_resp_headers.as_ref(),
        r.inbound_resp.body.as_ref(),
      ],
    )?;
    Ok(())
  }

  pub fn started(
    &mut self,
    request_id: &str,
    ts: i64,
    endpoint: &str,
    session_id: Option<&str>,
    source: Option<&str>,
    method: Option<&str>,
    inbound_req: &HttpSnapshot,
  ) -> Result<()> {
    let conn = self.conn_for_ts(ts)?;
    let inbound_req_headers = headers_json(&inbound_req.headers);
    conn.execute(
      "INSERT INTO requests (ts, session_id, source, method, request_id, endpoint, account_id, provider_id, model, initiator,
                             inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', '', '', '', ?7, ?8, ?9, ?10)
       ON CONFLICT(request_id) DO UPDATE SET
         ts=excluded.ts,
         session_id=COALESCE(session_id, excluded.session_id),
         source=COALESCE(source, excluded.source),
         method=COALESCE(method, excluded.method),
         endpoint=excluded.endpoint,
         inbound_req_method=excluded.inbound_req_method,
         inbound_req_url=excluded.inbound_req_url,
         inbound_req_headers=excluded.inbound_req_headers,
         inbound_req_body=excluded.inbound_req_body",
      params![
        ts,
        session_id,
        source,
        method,
        request_id,
        endpoint,
        inbound_req.method.as_deref(),
        inbound_req.url.as_deref(),
        inbound_req_headers.as_ref(),
        inbound_req.body.as_ref(),
      ],
    )?;
    Ok(())
  }

  pub fn parsed(&mut self, base_request_id: &str, attempt: u32, p: ParsedUpdate<'_>) -> Result<()> {
    let request_id = composite_request_id(base_request_id, attempt);
    let conn = self.conn_for_ts(p.ts)?;
    let outbound_req_headers = p.outbound_req.map(|s| headers_json(&s.headers));
    if attempt > 0 {
      conn.execute(
      "INSERT INTO requests (ts, session_id, source, method, request_id, endpoint, account_id, provider_id, model, initiator,
                               inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body)
         SELECT ts, session_id, source, method, ?2, endpoint, '', '', '', '',
                inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body
         FROM requests WHERE request_id = ?1
         ON CONFLICT(request_id) DO NOTHING",
        params![base_request_id, request_id],
      )?;
    }
    let updated = conn.execute(
      "UPDATE requests SET
         endpoint=?11,
         account_id=?2,
         provider_id=?3,
         model=?4,
         initiator=?5,
         stream=?6,
         outbound_req_method=?7,
         outbound_req_url=?8,
         outbound_req_headers=?9,
         outbound_req_body=?10
       WHERE request_id=?1",
      params![
        request_id,
        p.account_id,
        p.provider_id,
        p.model,
        p.initiator,
        p.stream as i64,
        opt_str(p.outbound_req, |s| s.method.as_deref()),
        opt_str(p.outbound_req, |s| s.url.as_deref()),
        outbound_req_headers.as_ref().map(|b| b.as_ref()),
        p.outbound_req.map(|s| s.body.as_ref()),
        p.endpoint,
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %base_request_id, attempt, "RequestParsed without started requests row");
    }
    Ok(())
  }

  pub fn responded(
    &mut self,
    ts: i64,
    base_request_id: &str,
    attempt: u32,
    latency_header_ms: u64,
    status: u16,
    resp_headers: &reqwest::header::HeaderMap,
  ) -> Result<()> {
    let request_id = composite_request_id(base_request_id, attempt);
    let conn = self.conn_for_ts(ts)?;
    let outbound_resp_headers = headers_json(resp_headers);
    let updated = conn.execute(
      "UPDATE requests SET
         status=?2,
         latency_header_ms=?3,
         outbound_resp_status=?2,
         outbound_resp_headers=?4
       WHERE request_id=?1",
      params![request_id, status as i64, latency_header_ms as i64, outbound_resp_headers.as_ref()],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %base_request_id, attempt, "RequestResponded without started requests row");
    }
    Ok(())
  }

  pub fn result(&mut self, r: &CallRecord) -> Result<()> {
    let conn = self.conn_for_ts(r.ts)?;
    let outbound_resp_headers = r.outbound_resp.as_ref().map(|s| headers_json(&s.headers));
    let inbound_resp_headers = headers_json(&r.inbound_resp.headers);
    conn.execute(
      "INSERT INTO requests (ts, session_id, source, method, request_id, request_error, endpoint, account_id, provider_id,
                             model, initiator, status, stream, latency_ms, latency_header_ms,
                             input_tok, output_tok, cached_tok, reasoning_tok,
                             inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body,
                             outbound_req_method, outbound_req_url, outbound_req_headers, outbound_req_body,
                             outbound_resp_status, outbound_resp_headers, outbound_resp_body,
                             inbound_resp_status, inbound_resp_headers, inbound_resp_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
               ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33)
       ON CONFLICT(request_id) DO UPDATE SET
         source=COALESCE(source, excluded.source),
         method=COALESCE(method, excluded.method),
         request_error=excluded.request_error,
         endpoint=excluded.endpoint,
         account_id=excluded.account_id,
         provider_id=excluded.provider_id,
         model=excluded.model,
         initiator=excluded.initiator,
         status=excluded.status,
         stream=excluded.stream,
         latency_ms=excluded.latency_ms,
         latency_header_ms=COALESCE(latency_header_ms, excluded.latency_header_ms),
         input_tok=excluded.input_tok,
         output_tok=excluded.output_tok,
         cached_tok=excluded.cached_tok,
         reasoning_tok=excluded.reasoning_tok,
         inbound_req_method=excluded.inbound_req_method,
         inbound_req_url=excluded.inbound_req_url,
         inbound_req_headers=excluded.inbound_req_headers,
         inbound_req_body=excluded.inbound_req_body,
         outbound_req_method=excluded.outbound_req_method,
         outbound_req_url=excluded.outbound_req_url,
         outbound_req_headers=excluded.outbound_req_headers,
         outbound_req_body=excluded.outbound_req_body,
         outbound_resp_status=excluded.outbound_resp_status,
         outbound_resp_headers=excluded.outbound_resp_headers,
         outbound_resp_body=excluded.outbound_resp_body,
         inbound_resp_status=excluded.inbound_resp_status,
         inbound_resp_headers=excluded.inbound_resp_headers,
         inbound_resp_body=excluded.inbound_resp_body",
      params![
        r.ts,
        r.session_id,
        Option::<String>::None,
        r.inbound_req.method.clone(),
        r.request_id,
        r.request_error,
        r.endpoint,
        r.account_id,
        r.provider_id,
        r.model,
        r.initiator,
        r.status as i64,
        r.stream as i64,
        r.latency_ms.map(|v| v as i64),
        r.latency_header_ms.map(|v| v as i64),
        r.usage.input_tokens.map(|v| v as i64),
        r.usage.output_tokens.map(|v| v as i64),
        r.usage.details.cache_read.map(|v| v as i64),
        r.usage.details.reasoning.map(|v| v as i64),
        r.inbound_req.method.as_deref(),
        r.inbound_req.url.as_deref(),
        headers_json(&r.inbound_req.headers).as_ref(),
        r.inbound_req.body.as_ref(),
        opt_str(r.outbound_req.as_ref(), |s| s.method.as_deref()),
        opt_str(r.outbound_req.as_ref(), |s| s.url.as_deref()),
        r.outbound_req.as_ref().map(|s| headers_json(&s.headers)).as_ref().map(|b| b.as_ref()),
        r.outbound_req.as_ref().map(|s| s.body.as_ref()),
        r.outbound_resp.as_ref().and_then(|s| s.status).map(|v| v as i64),
        outbound_resp_headers.as_ref().map(|b| b.as_ref()),
        r.outbound_resp.as_ref().map(|s| s.body.as_ref()),
        r.inbound_resp.status.map(|v| v as i64),
        inbound_resp_headers.as_ref(),
        r.inbound_resp.body.as_ref(),
      ],
    )?;
    Ok(())
  }

  fn conn_for_ts(&mut self, ts: i64) -> Result<&mut Connection> {
    let key = day_key(ts);
    if !self.conns.contains_key(&key) {
      if self.order.len() >= CACHE_CAP {
        if let Some(old) = self.order.pop_front() {
          self.conns.remove(&old);
        }
      }
      let conn = open_day_db(&self.dir.join(format!("{key}.db")))?;
      self.conns.insert(key.clone(), conn);
    }
    self.order.retain(|k| k != &key);
    self.order.push_back(key.clone());
    Ok(self.conns.get_mut(&key).expect("opened requests db"))
  }
}

pub struct ParsedUpdate<'a> {
  pub ts: i64,
  pub endpoint: &'a str,
  pub account_id: &'a str,
  pub provider_id: &'a str,
  pub model: &'a str,
  pub initiator: &'a str,
  pub stream: bool,
  pub outbound_req: Option<&'a HttpSnapshot>,
}

pub fn open_day_db(path: &Path) -> Result<Connection> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  let mut conn = Connection::open(path)?;
  migrate::apply(
    &mut conn,
    path,
    "requests",
    migrate::Bootstrap { sql: BOOTSTRAP },
    MIGRATIONS,
  )?;
  Ok(conn)
}

fn opt_str<'a, F>(snap: Option<&'a HttpSnapshot>, f: F) -> Option<&'a str>
where
  F: FnOnce(&'a HttpSnapshot) -> Option<&'a str>,
{
  snap.and_then(f)
}

fn day_key(ts: i64) -> String {
  let dt = time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or_else(|_| time::OffsetDateTime::now_utc());
  dt.date()
    .format(format_description!("[year]-[month]-[day]"))
    .unwrap_or_else(|_| "1970-01-01".to_string())
}

fn composite_request_id(request_id: &str, attempt: u32) -> String {
  if attempt == 0 {
    request_id.to_string()
  } else {
    format!("{request_id}:{attempt}")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::db::{CallRecord, HttpSnapshot, SessionSource, Usage};

  #[test]
  fn fresh_day_file_has_canonical_columns() {
    let dir = std::env::temp_dir().join(format!("llm-router-req-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2099-01-01.db");
    let conn = open_day_db(&path).unwrap();
    for col in [
      "request_id",
      "request_error",
      "input_tok",
      "output_tok",
      "cached_tok",
      "reasoning_tok",
      "latency_header_ms",
      "source",
      "method",
      "inbound_req_headers",
      "inbound_req_body",
      "outbound_req_headers",
      "outbound_req_body",
      "outbound_resp_headers",
      "outbound_resp_body",
      "inbound_resp_headers",
      "inbound_resp_body",
    ] {
      let exists: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name = ?1")
        .unwrap()
        .exists(params![col])
        .unwrap();
      assert!(exists, "missing column {col}");
    }
    let v: i64 = conn
      .prepare("SELECT MAX(version) FROM schema_migrations")
      .unwrap()
      .query_row([], |r| r.get(0))
      .unwrap();
    assert_eq!(v, 5);
  }

  #[test]
  fn migrates_v1_day_file_and_records_request_error() {
    let dir = std::env::temp_dir().join(format!("llm-router-req-mig-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let ts = 100;
    let path = dir.join(format!("{}.db", day_key(ts)));

    {
      let conn = Connection::open(&path).unwrap();
      conn
        .execute_batch(
          "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL, applied_ts INTEGER NOT NULL);",
        )
        .unwrap();
      conn
        .execute_batch(include_str!("../../../../scripts/migrations/requests/001_initial.sql"))
        .unwrap();
      conn
        .execute(
          "INSERT INTO schema_migrations (version, name, applied_ts) VALUES (1, 'initial', 0)",
          [],
        )
        .unwrap();
    }

    let mut db = RequestsDb::new(dir).unwrap();
    db.record(&CallRecord {
      ts,
      session_id: "session-1".into(),
      session_source: SessionSource::Header,
      request_id: "request-1".into(),
      request_error: Some("stream terminated before completion".into()),
      project_id: Some("project-1".into()),
      endpoint: "chat_completions".into(),
      account_id: "account".into(),
      provider_id: "provider".into(),
      model: "model".into(),
      initiator: "user".into(),
      status: 200,
      stream: true,
      latency_ms: Some(1),
      latency_header_ms: Some(1),
      usage: Usage::default(),
      inbound_req: HttpSnapshot::default(),
      outbound_req: None,
      outbound_resp: None,
      inbound_resp: HttpSnapshot::default(),
      messages: Vec::new(),
    })
    .unwrap();

    let conn = open_day_db(&path).unwrap();
    let row: (String, String) = conn
      .query_row("SELECT request_id, request_error FROM requests", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
      })
      .unwrap();
    assert_eq!(row, ("request-1".into(), "stream terminated before completion".into()));
  }
}
