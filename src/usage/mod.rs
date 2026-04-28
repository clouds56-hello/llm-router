use rusqlite::{params, Connection};
use snafu::{ResultExt, Snafu};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
  #[snafu(display("create directory `{}`", path.display()))]
  CreateDir { path: PathBuf, source: std::io::Error },

  #[snafu(display("open usage db `{}`", path.display()))]
  Open { path: PathBuf, source: rusqlite::Error },

  #[snafu(display("usage db migration"))]
  Migrate { source: rusqlite::Error },

  #[snafu(display("usage db: poisoned mutex"))]
  PoisonedLock,

  #[snafu(display("record usage row"))]
  Record { source: rusqlite::Error },

  #[snafu(display("query usage rows"))]
  Query { source: rusqlite::Error },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

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
      std::fs::create_dir_all(parent).context(CreateDirSnafu { path: parent.to_path_buf() })?;
    }
    let conn = Connection::open(path).context(OpenSnafu { path: path.to_path_buf() })?;
    conn
      .execute_batch(
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
      )
      .context(MigrateSnafu)?;
    // Idempotent migration: add `initiator` column if missing.
    let has_init: bool = conn
      .prepare("SELECT 1 FROM pragma_table_info('requests') WHERE name = 'initiator'")
      .context(MigrateSnafu)?
      .exists([])
      .context(MigrateSnafu)?;
    if !has_init {
      tracing::info!("usage db migration: adding 'initiator' column");
      conn
        .execute_batch("ALTER TABLE requests ADD COLUMN initiator TEXT NOT NULL DEFAULT 'user';")
        .context(MigrateSnafu)?;
    }
    tracing::debug!(path = %path.display(), "usage db opened");
    Ok(Self { conn: Mutex::new(conn) })
  }

  pub fn record(&self, r: Record<'_>) -> Result<()> {
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let conn = self.conn.lock().map_err(|_| Error::PoisonedLock)?;
    conn
      .execute(
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
      )
      .context(RecordSnafu)?;
    Ok(())
  }

  pub fn summary(&self, since_ts: i64, account: Option<&str>) -> Result<Vec<RowSummary>> {
    let conn = self.conn.lock().map_err(|_| Error::PoisonedLock)?;
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
    let mut stmt = conn.prepare(&sql).context(QuerySnafu)?;
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
        .query_map(params![since_ts, a], map_row)
        .context(QuerySnafu)?
        .collect::<rusqlite::Result<_>>()
        .context(QuerySnafu)?
    } else {
      stmt
        .query_map(params![since_ts], map_row)
        .context(QuerySnafu)?
        .collect::<rusqlite::Result<_>>()
        .context(QuerySnafu)?
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
