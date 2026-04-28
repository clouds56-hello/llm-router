use super::{CallRecord, MessageRecord, Result};
use rusqlite::{params, Connection};
use std::path::Path;

pub struct SessionsDb {
  conn: Connection,
}

impl SessionsDb {
  pub fn open(path: &Path) -> Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch(
      r#"
      CREATE TABLE IF NOT EXISTS sessions (
        id TEXT PRIMARY KEY,
        first_seen_ts INTEGER NOT NULL,
        last_seen_ts INTEGER NOT NULL,
        message_count INTEGER NOT NULL DEFAULT 0,
        account_id TEXT,
        provider_id TEXT,
        model TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_sessions_last ON sessions(last_seen_ts);

      CREATE TABLE IF NOT EXISTS messages (
        id INTEGER PRIMARY KEY,
        session_id TEXT NOT NULL REFERENCES sessions(id),
        ts INTEGER NOT NULL,
        endpoint TEXT NOT NULL,
        role TEXT NOT NULL,
        account_id TEXT NOT NULL,
        provider_id TEXT NOT NULL,
        model TEXT NOT NULL,
        status INTEGER,
        body BLOB NOT NULL
      );
      CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, ts);
      "#,
    )?;
    Ok(Self { conn })
  }

  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    let Some(session_id) = r.session_id.as_deref() else {
      return Ok(());
    };
    if r.messages.is_empty() {
      return Ok(());
    }
    self.conn.execute(
      "INSERT INTO sessions (id, first_seen_ts, last_seen_ts, message_count, account_id, provider_id, model)
       VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6)
       ON CONFLICT(id) DO UPDATE SET
         last_seen_ts=excluded.last_seen_ts,
         message_count=message_count + excluded.message_count,
         account_id=excluded.account_id,
         provider_id=excluded.provider_id,
         model=excluded.model",
      params![
        session_id,
        r.ts,
        r.messages.len() as i64,
        r.account_id,
        r.provider_id,
        r.model,
      ],
    )?;
    for m in &r.messages {
      self.insert_message(r, session_id, m)?;
    }
    Ok(())
  }

  fn insert_message(&self, r: &CallRecord, session_id: &str, m: &MessageRecord) -> Result<()> {
    self.conn.execute(
      "INSERT INTO messages (session_id, ts, endpoint, role, account_id, provider_id, model, status, body)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
      params![
        session_id,
        r.ts,
        r.endpoint,
        m.role,
        r.account_id,
        r.provider_id,
        r.model,
        m.status.map(|v| v as i64),
        m.body.as_ref(),
      ],
    )?;
    Ok(())
  }
}
