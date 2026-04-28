use super::{CallRecord, Result};
use rusqlite::{params, Connection};

pub struct UsageDb {
  conn: Connection,
}

impl UsageDb {
  pub fn open(conn: Connection) -> Result<Self> {
    conn.execute_batch(
      r#"
      CREATE TABLE IF NOT EXISTS requests (
        id INTEGER PRIMARY KEY,
        ts INTEGER NOT NULL,
        account_id TEXT NOT NULL,
        provider_id TEXT NOT NULL DEFAULT '',
        model TEXT NOT NULL,
        prompt_tok INTEGER,
        completion_tok INTEGER,
        latency_ms INTEGER NOT NULL,
        status INTEGER NOT NULL,
        stream INTEGER NOT NULL
      );
      CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
      CREATE INDEX IF NOT EXISTS idx_requests_account ON requests(account_id);
      PRAGMA user_version = 2;
      "#,
    )?;
    add_column_if_missing(&conn, "provider_id", "TEXT NOT NULL DEFAULT ''")?;
    add_column_if_missing(&conn, "initiator", "TEXT NOT NULL DEFAULT 'user'")?;
    Ok(Self { conn })
  }

  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    self.conn.execute(
      "INSERT INTO requests (ts, account_id, provider_id, model, initiator, prompt_tok, completion_tok, latency_ms, status, stream)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
      params![
        r.ts,
        r.account_id,
        r.provider_id,
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

  pub fn summary(&self, since_ts: i64, account: Option<&str>, provider: Option<&str>) -> Result<Vec<RowSummary>> {
    let mut sql = String::from(
      "SELECT account_id, provider_id, model, initiator, COUNT(*) AS n,
              COALESCE(SUM(prompt_tok),0), COALESCE(SUM(completion_tok),0),
              COALESCE(AVG(latency_ms),0)
       FROM requests
       WHERE ts >= ?1",
    );
    let mut bind_account = false;
    let mut bind_provider = false;
    if account.is_some() {
      bind_account = true;
      sql.push_str(" AND account_id = ?2");
    }
    if provider.is_some() {
      bind_provider = true;
      sql.push_str(if bind_account {
        " AND provider_id = ?3"
      } else {
        " AND provider_id = ?2"
      });
    }
    sql.push_str(" GROUP BY account_id, provider_id, model, initiator ORDER BY n DESC");

    let mut stmt = self.conn.prepare(&sql)?;
    let map_row = |row: &rusqlite::Row<'_>| {
      Ok(RowSummary {
        account: row.get::<_, String>(0)?,
        provider: row.get::<_, String>(1)?,
        model: row.get::<_, String>(2)?,
        initiator: row.get::<_, String>(3)?,
        count: row.get::<_, i64>(4)? as u64,
        prompt_tokens: row.get::<_, i64>(5)? as u64,
        completion_tokens: row.get::<_, i64>(6)? as u64,
        avg_latency_ms: row.get::<_, f64>(7)?,
      })
    };

    let rows = match (bind_account, bind_provider) {
      (true, true) => stmt
        .query_map(params![since_ts, account.unwrap(), provider.unwrap()], map_row)?
        .collect::<rusqlite::Result<_>>()?,
      (true, false) => stmt
        .query_map(params![since_ts, account.unwrap()], map_row)?
        .collect::<rusqlite::Result<_>>()?,
      (false, true) => stmt
        .query_map(params![since_ts, provider.unwrap()], map_row)?
        .collect::<rusqlite::Result<_>>()?,
      (false, false) => stmt
        .query_map(params![since_ts], map_row)?
        .collect::<rusqlite::Result<_>>()?,
    };
    Ok(rows)
  }
}

pub fn add_column_if_missing(conn: &Connection, column: &str, definition: &str) -> rusqlite::Result<()> {
  let exists = conn
    .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name = ?1")?
    .exists(params![column])?;
  if !exists {
    conn.execute_batch(&format!("ALTER TABLE requests ADD COLUMN {column} {definition};"))?;
  }
  Ok(())
}

#[derive(Debug)]
pub struct RowSummary {
  pub account: String,
  pub provider: String,
  pub model: String,
  pub initiator: String,
  pub count: u64,
  pub prompt_tokens: u64,
  pub completion_tokens: u64,
  pub avg_latency_ms: f64,
}
