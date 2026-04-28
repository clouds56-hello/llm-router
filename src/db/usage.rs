use super::{migrate, CallRecord, Result};
use rusqlite::{params, Connection};
use std::path::Path;

const BOOTSTRAP: &str = include_str!("../../scripts/migrations/usage/000_bootstrap.sql");
const MIGRATIONS: &[migrate::Migration] = &[migrate::Migration {
  version: 1,
  name: "initial",
  sql: include_str!("../../scripts/migrations/usage/001_initial.sql"),
}];

pub fn latest_version() -> u32 {
  migrate::latest_version(MIGRATIONS)
}

pub struct UsageDb {
  conn: Connection,
}

impl UsageDb {
  /// Open `usage.db` at `path`, applying any pending migrations. Pass the
  /// canonical filesystem path so `migrate::apply` can stage a `.bak`.
  pub fn open(path: &Path) -> Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    migrate::apply(
      &mut conn,
      path,
      "usage",
      migrate::Bootstrap { sql: BOOTSTRAP },
      MIGRATIONS,
    )?;
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
