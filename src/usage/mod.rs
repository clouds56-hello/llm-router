use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

pub struct UsageDb {
  conn: Mutex<Connection>,
}

pub struct Record<'a> {
  pub account_id: &'a str,
  pub model: &'a str,
  pub initiator: &'a str,
  pub prompt_tokens: Option<u64>,
  pub completion_tokens: Option<u64>,
  pub latency_ms: u64,
  pub status: u16,
  pub stream: bool,
}

impl UsageDb {
  pub fn open(path: &Path) -> Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let conn = Connection::open(path).with_context(|| format!("open usage db {}", path.display()))?;
    conn.execute_batch(
      r#"
            CREATE TABLE IF NOT EXISTS requests (
              id INTEGER PRIMARY KEY,
              ts INTEGER NOT NULL,
              account_id TEXT NOT NULL,
              model TEXT NOT NULL,
              prompt_tok INTEGER,
              completion_tok INTEGER,
              latency_ms INTEGER NOT NULL,
              status INTEGER NOT NULL,
              stream INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
            CREATE INDEX IF NOT EXISTS idx_requests_account ON requests(account_id);
            "#,
    )?;
    // Idempotent migration: add `initiator` column if missing.
    let has_init: bool = conn
      .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name = 'initiator'")?
      .exists([])?;
    if !has_init {
      conn.execute_batch("ALTER TABLE requests ADD COLUMN initiator TEXT NOT NULL DEFAULT 'user';")?;
    }
    Ok(Self { conn: Mutex::new(conn) })
  }

  pub fn record(&self, r: Record<'_>) -> Result<()> {
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let conn = self.conn.lock().unwrap();
    conn.execute(
      "INSERT INTO requests (ts, account_id, model, initiator, prompt_tok, completion_tok, latency_ms, status, stream)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
      params![
        ts,
        r.account_id,
        r.model,
        r.initiator,
        r.prompt_tokens.map(|v| v as i64),
        r.completion_tokens.map(|v| v as i64),
        r.latency_ms as i64,
        r.status as i64,
        r.stream as i64,
      ],
    )?;
    Ok(())
  }

  pub fn summary(&self, since_ts: i64, account: Option<&str>) -> Result<Vec<RowSummary>> {
    let conn = self.conn.lock().unwrap();
    let mut sql = String::from(
      "SELECT account_id, model, initiator, COUNT(*) AS n,
                    COALESCE(SUM(prompt_tok),0), COALESCE(SUM(completion_tok),0),
                    COALESCE(AVG(latency_ms),0)
             FROM requests
             WHERE ts >= ?1",
    );
    if account.is_some() {
      sql.push_str(" AND account_id = ?2");
    }
    sql.push_str(" GROUP BY account_id, model, initiator ORDER BY n DESC");
    let mut stmt = conn.prepare(&sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
      Ok(RowSummary {
        account: row.get::<_, String>(0)?,
        model: row.get::<_, String>(1)?,
        initiator: row.get::<_, String>(2)?,
        count: row.get::<_, i64>(3)? as u64,
        prompt_tokens: row.get::<_, i64>(4)? as u64,
        completion_tokens: row.get::<_, i64>(5)? as u64,
        avg_latency_ms: row.get::<_, f64>(6)?,
      })
    };
    let iter: Vec<RowSummary> = if let Some(a) = account {
      stmt
        .query_map(params![since_ts, a], map_row)?
        .collect::<rusqlite::Result<_>>()?
    } else {
      stmt
        .query_map(params![since_ts], map_row)?
        .collect::<rusqlite::Result<_>>()?
    };
    Ok(iter)
  }
}

#[derive(Debug)]
pub struct RowSummary {
  pub account: String,
  pub model: String,
  pub initiator: String,
  pub count: u64,
  pub prompt_tokens: u64,
  pub completion_tokens: u64,
  pub avg_latency_ms: f64,
}
