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
  migrate::Migration {
    version: 6,
    name: "add_context_and_metrics",
    sql: include_str!("../../../../scripts/migrations/requests/006_add_context_and_metrics.sql"),
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

pub struct HeadersUpdate<'a> {
  pub ts: i64,
  pub endpoint: &'a str,
  pub session_id: Option<&'a str>,
  pub local_addr: Option<&'a str>,
  pub mode: Option<&'a str>,
  pub method: Option<&'a str>,
  pub inbound_req: &'a HttpSnapshot,
}

#[derive(Default)]
pub struct RequestContext<'a> {
  pub user: Option<&'a str>,
  pub local_addr: Option<&'a str>,
  pub mode: Option<&'a str>,
  pub behave_as: Option<&'a str>,
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
    let inbound_req_headers = headers_json(&r.inbound.req_headers);
    let outbound_req_headers = r.outbound.as_ref().map(|s| headers_json(&s.req_headers));
    let outbound_resp_headers = r.outbound.as_ref().map(|s| headers_json(&s.resp_headers));
    let inbound_resp_headers = headers_json(&r.inbound.resp_headers);

    conn.execute(
      "INSERT INTO requests (ts, session_id, peer_addr, method, request_id, request_error, endpoint, account_id, provider_id, model, initiator, status, stream, latency_ms, latency_header_ms,
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
        r.peer_addr.as_deref(),
        r.method.as_deref(),
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
        r.inbound.method.as_deref(),
        r.inbound.url.as_deref(),
        inbound_req_headers.as_ref(),
        r.inbound.req_body.as_ref(),
        opt_str(r.outbound.as_ref(), |s| s.method.as_deref()),
        opt_str(r.outbound.as_ref(), |s| s.url.as_deref()),
        outbound_req_headers.as_ref().map(|b| b.as_ref()),
        r.outbound.as_ref().map(|s| s.req_body.as_ref()),
        r.outbound.as_ref().and_then(|s| s.status).map(|v| v as i64),
        outbound_resp_headers.as_ref().map(|b| b.as_ref()),
        r.outbound.as_ref().map(|s| s.resp_body.as_ref()),
        r.inbound.status.map(|v| v as i64),
        inbound_resp_headers.as_ref(),
        r.inbound.resp_body.as_ref(),
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
    ctx: RequestContext<'_>,
    peer_addr: Option<&str>,
    method: Option<&str>,
    inbound_req: &HttpSnapshot,
  ) -> Result<()> {
    let conn = self.conn_for_ts(ts)?;
    let inbound_req_headers = headers_json(&inbound_req.req_headers);
    conn.execute(
      "INSERT INTO requests (ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, request_id, endpoint, account_id, provider_id, model, initiator,
                              inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, '', '', '', '', ?11, ?12, ?13, ?14)
       ON CONFLICT(request_id) DO UPDATE SET
         ts=excluded.ts,
         session_id=COALESCE(session_id, excluded.session_id),
         user=COALESCE(user, excluded.user),
         local_addr=COALESCE(local_addr, excluded.local_addr),
         mode=COALESCE(mode, excluded.mode),
         behave_as=COALESCE(behave_as, excluded.behave_as),
         peer_addr=COALESCE(peer_addr, excluded.peer_addr),
         method=COALESCE(method, excluded.method),
         endpoint=excluded.endpoint,
         inbound_req_method=excluded.inbound_req_method,
         inbound_req_url=excluded.inbound_req_url,
         inbound_req_headers=excluded.inbound_req_headers,
         inbound_req_body=excluded.inbound_req_body",
      params![
        ts,
        session_id,
        ctx.user,
        ctx.local_addr,
        ctx.mode,
        ctx.behave_as,
        peer_addr,
        method,
        request_id,
        endpoint,
        inbound_req.method.as_deref(),
        inbound_req.url.as_deref(),
        inbound_req_headers.as_ref(),
        inbound_req.req_body.as_ref(),
      ],
    )?;
    Ok(())
  }

  pub fn headers(&mut self, request_id: &str, h: HeadersUpdate<'_>) -> Result<()> {
    let conn = self.conn_for_ts(h.ts)?;
    let inbound_req_headers = headers_json(&h.inbound_req.req_headers);
    let updated = conn.execute(
      "UPDATE requests SET
         session_id=COALESCE(?2, session_id),
         local_addr=COALESCE(?3, local_addr),
         mode=COALESCE(?4, mode),
         method=COALESCE(?5, method),
         endpoint=?6,
         inbound_req_method=COALESCE(?7, inbound_req_method),
         inbound_req_url=COALESCE(?8, inbound_req_url),
         inbound_req_headers=?9
       WHERE request_id=?1",
      params![
        request_id,
        h.session_id,
        h.local_addr,
        h.mode,
        h.method,
        h.endpoint,
        h.inbound_req.method.as_deref(),
        h.inbound_req.url.as_deref(),
        inbound_req_headers.as_ref(),
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %request_id, "RequestHeaders without started requests row");
    }
    Ok(())
  }

  pub fn parsed(&mut self, base_request_id: &str, attempt: u32, p: ParsedUpdate<'_>) -> Result<()> {
    let request_id = composite_request_id(base_request_id, attempt);
    let conn = self.conn_for_ts(p.ts)?;
    if attempt > 0 {
      conn.execute(
      "INSERT INTO requests (ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, request_id, endpoint, account_id, provider_id, model, initiator,
                                inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body)
         SELECT ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, ?2, endpoint, '', '', '', '',
                 inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body
         FROM requests WHERE request_id = ?1
         ON CONFLICT(request_id) DO NOTHING",
        params![base_request_id, request_id],
      )?;
    }
    let updated = conn.execute(
      "UPDATE requests SET
         endpoint=?7,
         account_id=?2,
         provider_id=?3,
         model=?4,
         initiator=?5,
         stream=?6,
         behave_as=COALESCE(?9, behave_as),
         inbound_req_body=?8
       WHERE request_id=?1",
      params![
        request_id,
        p.account_id,
        p.provider_id,
        p.model,
        p.initiator,
        p.stream as i64,
        p.endpoint,
        p.inbound_body.as_ref(),
        p.behave_as,
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %base_request_id, attempt, "RequestParsed without started requests row");
    }
    Ok(())
  }

  #[allow(clippy::too_many_arguments)]
  pub fn responded(
    &mut self,
    ts: i64,
    base_request_id: &str,
    attempt: u32,
    latency_header_ms: u64,
    status: u16,
    outbound_resp_headers: &llm_headers::HeaderMap,
    outbound_req_method: Option<&str>,
    outbound_req_url: Option<&str>,
    outbound_req_headers: Option<&llm_headers::HeaderMap>,
    outbound_req_body: Option<&bytes::Bytes>,
  ) -> Result<()> {
    let request_id = composite_request_id(base_request_id, attempt);
    let conn = self.conn_for_ts(ts)?;
    let outbound_resp_headers_json = headers_json(outbound_resp_headers);
    let outbound_req_headers_json = outbound_req_headers.map(headers_json);
    let updated = conn.execute(
      "UPDATE requests SET
         status=?2,
         latency_header_ms=?3,
         outbound_resp_status=?2,
         outbound_resp_headers=?4,
         outbound_req_method=?5,
         outbound_req_url=?6,
         outbound_req_headers=?7,
         outbound_req_body=?8
       WHERE request_id=?1",
      params![
        request_id,
        status as i64,
        latency_header_ms as i64,
        outbound_resp_headers_json.as_ref(),
        outbound_req_method,
        outbound_req_url,
        outbound_req_headers_json.as_ref().map(|b| b.as_ref()),
        outbound_req_body.map(|b| b.as_ref()),
      ],
    )?;
    if updated == 0 {
      tracing::warn!(request_id = %base_request_id, attempt, "RequestResponded without started requests row");
    }
    Ok(())
  }

  pub fn result(&mut self, r: &CallRecord) -> Result<()> {
    let conn = self.conn_for_ts(r.ts)?;
    let outbound_resp_headers = r.outbound.as_ref().map(|s| headers_json(&s.resp_headers));
    let inbound_resp_headers = headers_json(&r.inbound.resp_headers);
    conn.execute(
      "INSERT INTO requests (ts, session_id, user, local_addr, mode, behave_as, peer_addr, method, request_id, request_error, endpoint, account_id, provider_id,
                              model, initiator, status, stream, latency_ms, latency_header_ms,
                              input_tok, output_tok, cached_tok, reasoning_tok,
                             inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body,
                             outbound_req_method, outbound_req_url, outbound_req_headers, outbound_req_body,
                             outbound_resp_status, outbound_resp_headers, outbound_resp_body,
                             inbound_resp_status, inbound_resp_headers, inbound_resp_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20,
               ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37)
       ON CONFLICT(request_id) DO UPDATE SET
         user=COALESCE(user, excluded.user),
         local_addr=COALESCE(local_addr, excluded.local_addr),
         mode=COALESCE(mode, excluded.mode),
         behave_as=COALESCE(behave_as, excluded.behave_as),
         peer_addr=COALESCE(peer_addr, excluded.peer_addr),
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
        r.user.as_deref(),
        r.local_addr.as_deref(),
        r.mode.as_deref(),
        r.behave_as.as_deref(),
        r.peer_addr.as_deref(),
        r.method.as_deref(),
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
        r.inbound.method.as_deref(),
        r.inbound.url.as_deref(),
        headers_json(&r.inbound.req_headers).as_ref(),
        r.inbound.req_body.as_ref(),
        opt_str(r.outbound.as_ref(), |s| s.method.as_deref()),
        opt_str(r.outbound.as_ref(), |s| s.url.as_deref()),
        r.outbound.as_ref().map(|s| headers_json(&s.req_headers)).as_ref().map(|b| b.as_ref()),
        r.outbound.as_ref().map(|s| s.req_body.as_ref()),
        r.outbound.as_ref().and_then(|s| s.status).map(|v| v as i64),
        outbound_resp_headers.as_ref().map(|b| b.as_ref()),
        r.outbound.as_ref().map(|s| s.resp_body.as_ref()),
        r.inbound.status.map(|v| v as i64),
        inbound_resp_headers.as_ref(),
        r.inbound.resp_body.as_ref(),
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
  pub behave_as: Option<&'a str>,
  pub inbound_body: bytes::Bytes,
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
      "user",
      "local_addr",
      "mode",
      "behave_as",
      "input_tok",
      "output_tok",
      "cached_tok",
      "reasoning_tok",
      "latency_header_ms",
      "peer_addr",
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
    let metrics_exists: bool = conn
      .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'metrics'")
      .unwrap()
      .exists([])
      .unwrap();
    assert!(metrics_exists, "missing metrics table");
    for col in [
      "request_id",
      "user",
      "local_addr",
      "mode",
      "behave_as",
      "peer_addr",
      "method",
      "path",
      "url",
      "status",
      "request_error",
      "account_id",
      "provider_id",
      "latency_ms",
      "inbound_req_headers",
      "inbound_resp_headers",
      "inbound_resp_body",
    ] {
      let exists: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('metrics') WHERE name = ?1")
        .unwrap()
        .exists(params![col])
        .unwrap();
      assert!(exists, "missing metrics column {col}");
    }
    let v: i64 = conn
      .prepare("SELECT MAX(version) FROM schema_migrations")
      .unwrap()
      .query_row([], |r| r.get(0))
      .unwrap();
    assert_eq!(v, 6);
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
      user: None,
      local_addr: None,
      mode: None,
      behave_as: None,
      peer_addr: Some("127.0.0.1:4142".into()),
      method: Some("POST".into()),
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
      inbound: HttpSnapshot::default(),
      outbound: None,
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

  #[test]
  fn headers_updates_existing_started_row() {
    let dir = std::env::temp_dir().join(format!("llm-router-req-headers-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = RequestsDb::new(dir.clone()).unwrap();
    let ts = 1_700_000_000;
    let request_id = "request-headers";
    let started = HttpSnapshot {
      method: Some("POST".into()),
      url: Some("https://example.test/original".into()),
      ..Default::default()
    };
    db.started(
      request_id,
      ts,
      "chat_completions",
      Some("sess-1"),
      RequestContext::default(),
      Some("127.0.0.1:4142"),
      Some("POST"),
      &started,
    )
    .unwrap();

    let mut headers = llm_headers::HeaderMap::new();
    headers.insert("x-test", "1");
    let with_headers = HttpSnapshot {
      method: Some("POST".into()),
      url: Some("https://example.test/original".into()),
      req_headers: headers,
      ..Default::default()
    };
    db.headers(
      request_id,
      HeadersUpdate {
        ts,
        endpoint: "responses",
        session_id: Some("sess-1"),
        local_addr: Some("localhost:4141"),
        mode: Some("route"),
        method: Some("POST"),
        inbound_req: &with_headers,
      },
    )
    .unwrap();

    let conn = open_day_db(&dir.join(format!("{}.db", day_key(ts)))).unwrap();
    let row: (i64, String, Option<String>, Option<String>, Vec<u8>) = conn
      .query_row(
        "SELECT COUNT(*), endpoint, peer_addr, method, inbound_req_headers FROM requests WHERE request_id = ?1",
        params![request_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
      )
      .unwrap();
    assert_eq!(row.0, 1);
    assert_eq!(row.1, "responses");
    assert_eq!(row.2.as_deref(), Some("127.0.0.1:4142"));
    assert_eq!(row.3.as_deref(), Some("POST"));
    assert!(String::from_utf8(row.4).unwrap().contains("\"x-test\":\"1\""));
  }

  #[test]
  fn result_insert_can_backfill_source_and_method() {
    let dir = std::env::temp_dir().join(format!("llm-router-req-result-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut db = RequestsDb::new(dir.clone()).unwrap();
    let ts = 1_700_000_000;

    db.result(&CallRecord {
      ts,
      session_id: "session-1".into(),
      session_source: SessionSource::Header,
      user: None,
      local_addr: None,
      mode: None,
      behave_as: None,
      peer_addr: Some("127.0.0.1:4142".into()),
      method: Some("POST".into()),
      request_id: "request-result".into(),
      request_error: None,
      project_id: Some("project-1".into()),
      endpoint: "responses".into(),
      account_id: "account".into(),
      provider_id: "provider".into(),
      model: "model".into(),
      initiator: "user".into(),
      status: 200,
      stream: false,
      latency_ms: Some(10),
      latency_header_ms: Some(4),
      usage: Usage::default(),
      inbound: HttpSnapshot::default(),
      outbound: None,
      messages: Vec::new(),
    })
    .unwrap();

    let conn = open_day_db(&dir.join(format!("{}.db", day_key(ts)))).unwrap();
    let row: (Option<String>, Option<String>) = conn
      .query_row(
        "SELECT peer_addr, method FROM requests WHERE request_id = 'request-result'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
      )
      .unwrap();
    assert_eq!(row.0.as_deref(), Some("127.0.0.1:4142"));
    assert_eq!(row.1.as_deref(), Some("POST"));
  }
}
