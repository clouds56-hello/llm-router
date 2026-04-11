use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};

use super::usage::TokenUsage;
use super::SharedConn;

#[derive(Debug, Clone)]
pub struct RequestRecordStart {
  pub request_id: String,
  pub created_at: DateTime<Utc>,
  pub endpoint: String,
  pub provider: String,
  pub adapter: String,
  pub model: String,
  pub account_id: Option<String>,
  pub is_stream: bool,
  pub request_body_json: String,
  pub upstream_request_body_json: String,
}

#[derive(Debug, Clone)]
pub struct RequestRecordCompleted {
  pub request_id: String,
  pub completed_at: DateTime<Utc>,
  pub response_body_json: Option<String>,
  pub response_sse_text: Option<String>,
  pub http_status: Option<u16>,
  pub usage: TokenUsage,
  pub latency_ms: i64,
}

#[derive(Debug, Clone)]
pub struct RequestRecordFailed {
  pub request_id: String,
  pub completed_at: DateTime<Utc>,
  pub http_status: Option<u16>,
  pub error_text: String,
  pub response_sse_text: Option<String>,
  pub latency_ms: i64,
}

pub(super) struct RequestsTable {
  conn: SharedConn,
}

impl RequestsTable {
  pub(super) fn new(conn: SharedConn) -> Result<Self> {
    let this = Self { conn };
    this.init_schema()?;
    Ok(this)
  }

  fn init_schema(&self) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute_batch(
      "
      CREATE TABLE IF NOT EXISTS llm_requests (
        request_id TEXT PRIMARY KEY,
        created_at TEXT NOT NULL,
        completed_at TEXT,
        endpoint TEXT NOT NULL,
        provider TEXT NOT NULL,
        adapter TEXT NOT NULL,
        model TEXT NOT NULL,
        account_id TEXT,
        is_stream INTEGER NOT NULL,
        request_body_json TEXT NOT NULL,
        upstream_request_body_json TEXT,
        response_body_json TEXT,
        response_sse_text TEXT,
        http_status INTEGER,
        error_text TEXT,
        prompt_tokens INTEGER,
        completion_tokens INTEGER,
        total_tokens INTEGER,
        latency_ms INTEGER
      );
      CREATE INDEX IF NOT EXISTS idx_llm_requests_created_at ON llm_requests(created_at DESC);
      CREATE INDEX IF NOT EXISTS idx_llm_requests_provider_account_created_at ON llm_requests(provider, account_id, created_at DESC);
      CREATE INDEX IF NOT EXISTS idx_llm_requests_model_created_at ON llm_requests(model, created_at DESC);
      ",
    )?;

    ensure_column(&conn, "llm_requests", "response_sse_text", "TEXT")?;
    ensure_column(&conn, "llm_requests", "upstream_request_body_json", "TEXT")?;
    ensure_column(&conn, "llm_requests", "prompt_tokens", "INTEGER")?;
    ensure_column(&conn, "llm_requests", "completion_tokens", "INTEGER")?;
    ensure_column(&conn, "llm_requests", "total_tokens", "INTEGER")?;
    ensure_column(&conn, "llm_requests", "latency_ms", "INTEGER")?;
    Ok(())
  }

  pub(super) fn record_request_started(&self, input: RequestRecordStart) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "INSERT OR REPLACE INTO llm_requests(
        request_id, created_at, endpoint, provider, adapter, model, account_id, is_stream, request_body_json, upstream_request_body_json
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
      params![
        input.request_id,
        input.created_at.to_rfc3339(),
        input.endpoint,
        input.provider,
        input.adapter,
        input.model,
        input.account_id,
        bool_to_int(input.is_stream),
        input.request_body_json,
        input.upstream_request_body_json
      ],
    )?;
    Ok(())
  }

  pub(super) fn record_request_completed(&self, input: RequestRecordCompleted) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "UPDATE llm_requests
       SET completed_at = ?2,
           response_body_json = ?3,
           response_sse_text = ?4,
           http_status = ?5,
           error_text = NULL,
           prompt_tokens = ?6,
           completion_tokens = ?7,
           total_tokens = ?8,
           latency_ms = ?9
       WHERE request_id = ?1",
      params![
        input.request_id,
        input.completed_at.to_rfc3339(),
        input.response_body_json,
        input.response_sse_text,
        input.http_status.map(i64::from),
        input.usage.prompt_tokens,
        input.usage.completion_tokens,
        input.usage.total_tokens,
        input.latency_ms
      ],
    )?;
    Ok(())
  }

  pub(super) fn record_request_failed(&self, input: RequestRecordFailed) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "UPDATE llm_requests
       SET completed_at = ?2,
           response_sse_text = ?3,
           http_status = ?4,
           error_text = ?5,
           latency_ms = ?6
       WHERE request_id = ?1",
      params![
        input.request_id,
        input.completed_at.to_rfc3339(),
        input.response_sse_text,
        input.http_status.map(i64::from),
        input.error_text,
        input.latency_ms
      ],
    )?;
    Ok(())
  }

  pub(super) fn prune_older_than(&self, cutoff_ts: &str) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute("DELETE FROM llm_requests WHERE created_at < ?1", params![cutoff_ts])?;
    Ok(())
  }
}

fn ensure_column(conn: &Connection, table: &str, column: &str, sql_ty: &str) -> Result<()> {
  let pragma = format!("PRAGMA table_info({table})");
  let mut has_column = false;
  let mut stmt = conn.prepare(&pragma)?;
  let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
  for row in rows {
    if row?.as_str() == column {
      has_column = true;
      break;
    }
  }
  if !has_column {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {sql_ty}");
    conn.execute(&sql, [])?;
  }
  Ok(())
}

fn bool_to_int(v: bool) -> i64 {
  if v {
    1
  } else {
    0
  }
}

#[cfg(test)]
mod tests {
  use chrono::Utc;
  use rusqlite::Connection;

  use crate::db::{RequestRecordStart, RequestStore};

  #[test]
  fn schema_init_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let _store = RequestStore::new(&db).expect("first init");
    let _store2 = RequestStore::new(&db).expect("second init");
  }

  #[test]
  fn prune_removes_old_request_rows() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let old = Utc::now() - chrono::Duration::days(40);
    store
      .record_request_started(RequestRecordStart {
        request_id: "req-old".to_string(),
        created_at: old,
        endpoint: "/v1/chat/completions".to_string(),
        provider: "openai".to_string(),
        adapter: "openai".to_string(),
        model: "gpt-5".to_string(),
        account_id: Some("a1".to_string()),
        is_stream: false,
        request_body_json: "{}".to_string(),
        upstream_request_body_json: "{}".to_string(),
      })
      .expect("start");

    store.prune_older_than_days(30).expect("prune");
    let conn = Connection::open(db).expect("open");
    let cnt: i64 = conn
      .query_row(
        "SELECT COUNT(*) FROM llm_requests WHERE request_id = 'req-old'",
        [],
        |row| row.get(0),
      )
      .expect("count");
    assert_eq!(cnt, 0);
  }
}
