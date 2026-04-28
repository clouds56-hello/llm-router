use super::{headers_json, CallRecord, HttpSnapshot, Result};
use rusqlite::{params, Connection};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use time::macros::format_description;

const CACHE_CAP: usize = 3;
const REQUESTS_RENAME_MIGRATION: &str =
  include_str!("../../scripts/migrations/001_requests_rename_to_inbound_outbound.sql");

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

  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    let conn = self.conn_for_ts(r.ts)?;
    let inbound_req_headers = headers_json(&r.inbound_req.headers);
    let outbound_req_headers = r.outbound_req.as_ref().map(|s| headers_json(&s.headers));
    let outbound_resp_headers = r.outbound_resp.as_ref().map(|s| headers_json(&s.headers));
    let inbound_resp_headers = headers_json(&r.inbound_resp.headers);

    conn.execute(
      "INSERT INTO requests (ts, session_id, endpoint, account_id, provider_id, model, initiator, status, stream, latency_ms,
                             prompt_tok, completion_tok,
                             inbound_req_method, inbound_req_url, inbound_req_headers, inbound_req_body,
                             outbound_req_method, outbound_req_url, outbound_req_headers, outbound_req_body,
                             outbound_resp_status, outbound_resp_headers, outbound_resp_body,
                             inbound_resp_status, inbound_resp_headers, inbound_resp_body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
               ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
      params![
        r.ts,
        r.session_id,
        r.endpoint,
        r.account_id,
        r.provider_id,
        r.model,
        r.initiator,
        r.status as i64,
        r.stream as i64,
        r.latency_ms as i64,
        r.prompt_tokens.map(|v| v as i64),
        r.completion_tokens.map(|v| v as i64),
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

fn opt_str<'a, F>(snap: Option<&'a HttpSnapshot>, f: F) -> Option<&'a str>
where
  F: FnOnce(&'a HttpSnapshot) -> Option<&'a str>,
{
  snap.and_then(f)
}

fn open_day_db(path: &Path) -> Result<Connection> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent)?;
  }
  let conn = Connection::open(path)?;
  create_schema(&conn)?;
  apply_migrations(&conn)?;
  add_missing_columns(&conn)?;
  Ok(conn)
}

fn create_schema(conn: &Connection) -> Result<()> {
  conn.execute_batch(
    r#"
    CREATE TABLE IF NOT EXISTS requests (
      id INTEGER PRIMARY KEY,
      ts INTEGER NOT NULL,
      session_id TEXT NOT NULL,
      endpoint TEXT NOT NULL,
      account_id TEXT NOT NULL,
      provider_id TEXT NOT NULL,
      model TEXT NOT NULL,
      initiator TEXT NOT NULL,
      status INTEGER NOT NULL,
      stream INTEGER NOT NULL,
      latency_ms INTEGER NOT NULL,
      prompt_tok INTEGER,
      completion_tok INTEGER,
      inbound_req_method TEXT,
      inbound_req_url TEXT,
      inbound_req_headers BLOB NOT NULL,
      inbound_req_body BLOB NOT NULL,
      outbound_req_method TEXT,
      outbound_req_url TEXT,
      outbound_req_headers BLOB,
      outbound_req_body BLOB,
      outbound_resp_status INTEGER,
      outbound_resp_headers BLOB,
      outbound_resp_body BLOB,
      inbound_resp_status INTEGER,
      inbound_resp_headers BLOB,
      inbound_resp_body BLOB
    );
    CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
    CREATE INDEX IF NOT EXISTS idx_requests_session ON requests(session_id);
    CREATE INDEX IF NOT EXISTS idx_requests_account ON requests(account_id);
    "#,
  )?;
  Ok(())
}

fn apply_migrations(conn: &Connection) -> Result<()> {
  conn.execute_batch(
    r#"
    CREATE TABLE IF NOT EXISTS _migrations (
      name TEXT PRIMARY KEY,
      applied_ts INTEGER NOT NULL
    );
    "#,
  )?;
  if column_exists(conn, "req_headers")? {
    ensure_old_outbound_columns(conn)?;
    if !migration_applied(conn, "001_requests_rename_to_inbound_outbound")? {
      conn.execute_batch(REQUESTS_RENAME_MIGRATION)?;
      mark_migration(conn, "001_requests_rename_to_inbound_outbound")?;
    }
  } else if column_exists(conn, "inbound_req_headers")? {
    mark_migration(conn, "001_requests_rename_to_inbound_outbound")?;
  }
  Ok(())
}

fn ensure_old_outbound_columns(conn: &Connection) -> Result<()> {
  for (col, def) in &[
    ("outbound_method", "TEXT"),
    ("outbound_url", "TEXT"),
    ("outbound_headers", "BLOB"),
    ("outbound_body", "BLOB"),
  ] {
    add_column_if_missing(conn, col, def)?;
  }
  Ok(())
}

fn add_missing_columns(conn: &Connection) -> Result<()> {
  for (col, def) in &[
    ("inbound_req_method", "TEXT"),
    ("inbound_req_url", "TEXT"),
    ("inbound_req_headers", "BLOB NOT NULL DEFAULT X''"),
    ("inbound_req_body", "BLOB NOT NULL DEFAULT X''"),
    ("outbound_req_method", "TEXT"),
    ("outbound_req_url", "TEXT"),
    ("outbound_req_headers", "BLOB"),
    ("outbound_req_body", "BLOB"),
    ("outbound_resp_status", "INTEGER"),
    ("outbound_resp_headers", "BLOB"),
    ("outbound_resp_body", "BLOB"),
    ("inbound_resp_status", "INTEGER"),
    ("inbound_resp_headers", "BLOB"),
    ("inbound_resp_body", "BLOB"),
  ] {
    add_column_if_missing(conn, col, def)?;
  }
  Ok(())
}

fn migration_applied(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
  conn
    .prepare("SELECT 1 FROM _migrations WHERE name = ?1")?
    .exists(params![name])
}

fn mark_migration(conn: &Connection, name: &str) -> rusqlite::Result<()> {
  conn.execute(
    "INSERT OR IGNORE INTO _migrations (name, applied_ts) VALUES (?1, ?2)",
    params![name, time::OffsetDateTime::now_utc().unix_timestamp()],
  )?;
  Ok(())
}

fn add_column_if_missing(conn: &Connection, column: &str, definition: &str) -> rusqlite::Result<()> {
  if !column_exists(conn, column)? {
    conn.execute_batch(&format!("ALTER TABLE requests ADD COLUMN {column} {definition};"))?;
  }
  Ok(())
}

fn column_exists(conn: &Connection, column: &str) -> rusqlite::Result<bool> {
  conn
    .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name = ?1")?
    .exists(params![column])
}

fn day_key(ts: i64) -> String {
  let dt = time::OffsetDateTime::from_unix_timestamp(ts).unwrap_or_else(|_| time::OffsetDateTime::now_utc());
  dt.date()
    .format(format_description!("[year]-[month]-[day]"))
    .unwrap_or_else(|_| "1970-01-01".to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn migrates_existing_day_file_to_inbound_outbound_names() {
    let dir = std::env::temp_dir().join(format!("llm-router-req-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2099-01-01.db");
    {
      let conn = Connection::open(&path).unwrap();
      conn
        .execute_batch(
          r#"
          CREATE TABLE requests (
            id INTEGER PRIMARY KEY,
            ts INTEGER NOT NULL,
            session_id TEXT,
            endpoint TEXT NOT NULL,
            account_id TEXT NOT NULL,
            provider_id TEXT NOT NULL,
            model TEXT NOT NULL,
            initiator TEXT NOT NULL,
            status INTEGER NOT NULL,
            stream INTEGER NOT NULL,
            latency_ms INTEGER NOT NULL,
            prompt_tok INTEGER,
            completion_tok INTEGER,
            req_headers BLOB NOT NULL,
            req_body BLOB NOT NULL,
            resp_headers BLOB,
            resp_body BLOB
          );
          "#,
        )
        .unwrap();
    }
    let conn = open_day_db(&path).unwrap();
    assert!(column_exists(&conn, "inbound_req_headers").unwrap());
    assert!(column_exists(&conn, "outbound_req_headers").unwrap());
    assert!(column_exists(&conn, "outbound_resp_headers").unwrap());
    assert!(column_exists(&conn, "inbound_resp_headers").unwrap());
    assert!(!column_exists(&conn, "req_headers").unwrap());
  }
}
