use super::{CallRecord, MessageRecord, PartRecord, Result};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::Path;
use tracing::{debug, trace};

pub struct SessionsDb {
  conn: Connection,
}

impl SessionsDb {
  pub fn open(path: &Path) -> Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    drop_legacy_if_present(&conn)?;
    create_schema(&conn)?;
    Ok(Self { conn })
  }

  pub fn record(&mut self, r: &CallRecord) -> Result<()> {
    if r.messages.is_empty() {
      debug!(session_id = %r.session_id, "sessions.record: no messages, skipping");
      return Ok(());
    }
    trace!(
      session_id = %r.session_id,
      source = r.session_source.as_str(),
      message_count = r.messages.len(),
      "sessions.record: begin",
    );
    let tx = self.conn.transaction()?;
    let session_source = r.session_source.as_str();
    tx.execute(
      "INSERT INTO sessions (id, first_seen_ts, last_seen_ts, message_count, account_id, provider_id, model, source)
       VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7)
       ON CONFLICT(id) DO UPDATE SET
         last_seen_ts=excluded.last_seen_ts,
         message_count=message_count + excluded.message_count,
         account_id=excluded.account_id,
         provider_id=excluded.provider_id,
         model=excluded.model",
      params![
        r.session_id,
        r.ts,
        r.messages.len() as i64,
        r.account_id,
        r.provider_id,
        r.model,
        session_source,
      ],
    )?;

    for m in &r.messages {
      insert_message(&tx, r, m)?;
    }
    tx.commit()?;
    trace!(session_id = %r.session_id, "sessions.record: committed");
    Ok(())
  }
}

fn insert_message(tx: &rusqlite::Transaction<'_>, r: &CallRecord, m: &MessageRecord) -> Result<()> {
  tx.execute(
    "INSERT INTO messages (session_id, ts, endpoint, role, account_id, provider_id, model, status)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    params![
      r.session_id,
      r.ts,
      r.endpoint,
      m.role,
      r.account_id,
      r.provider_id,
      r.model,
      m.status.map(|v| v as i64),
    ],
  )?;
  let message_id = tx.last_insert_rowid();
  for (idx, part) in m.parts.iter().enumerate() {
    let hash = hash_part(&part.part_type, part.content.as_ref());
    tx.execute(
      "INSERT OR IGNORE INTO message_parts (hash, part_type, content) VALUES (?1, ?2, ?3)",
      params![hash, part.part_type, part.content.as_ref()],
    )?;
    tx.execute(
      "INSERT INTO message_part_refs (message_id, part_index, part_hash) VALUES (?1, ?2, ?3)",
      params![message_id, idx as i64, hash],
    )?;
  }
  Ok(())
}

fn hash_part(part_type: &str, content: &[u8]) -> String {
  let mut h = Sha256::new();
  h.update(part_type.as_bytes());
  h.update([0u8]);
  h.update(content);
  format!("{:x}", h.finalize())
}

fn create_schema(conn: &Connection) -> Result<()> {
  conn.execute_batch(
    r#"
    CREATE TABLE IF NOT EXISTS sessions (
      id TEXT PRIMARY KEY,
      first_seen_ts INTEGER NOT NULL,
      last_seen_ts  INTEGER NOT NULL,
      message_count INTEGER NOT NULL DEFAULT 0,
      account_id  TEXT,
      provider_id TEXT,
      model       TEXT,
      source      TEXT NOT NULL DEFAULT 'header'
    );
    CREATE INDEX IF NOT EXISTS idx_sessions_last ON sessions(last_seen_ts);

    CREATE TABLE IF NOT EXISTS message_parts (
      hash      TEXT PRIMARY KEY,
      part_type TEXT NOT NULL,
      content   BLOB NOT NULL
    );

    CREATE TABLE IF NOT EXISTS messages (
      id          INTEGER PRIMARY KEY,
      session_id  TEXT NOT NULL REFERENCES sessions(id),
      ts          INTEGER NOT NULL,
      endpoint    TEXT NOT NULL,
      role        TEXT NOT NULL,
      account_id  TEXT NOT NULL,
      provider_id TEXT NOT NULL,
      model       TEXT NOT NULL,
      status      INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, ts);

    CREATE TABLE IF NOT EXISTS message_part_refs (
      message_id INTEGER NOT NULL REFERENCES messages(id),
      part_index INTEGER NOT NULL,
      part_hash  TEXT NOT NULL REFERENCES message_parts(hash),
      PRIMARY KEY (message_id, part_index)
    );
    CREATE INDEX IF NOT EXISTS idx_part_refs_hash ON message_part_refs(part_hash);
    "#,
  )?;
  Ok(())
}

/// If the file was created by the previous schema (messages.body BLOB), drop
/// the old tables so the new schema can be created in their place. We don't
/// migrate rows: sessions.db was only just introduced.
fn drop_legacy_if_present(conn: &Connection) -> Result<()> {
  let has_body_col: bool = conn
    .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'body'")?
    .exists([])?;
  if has_body_col {
    conn.execute_batch(
      r#"
      DROP TABLE IF EXISTS message_part_refs;
      DROP TABLE IF EXISTS messages;
      DROP TABLE IF EXISTS message_parts;
      DROP TABLE IF EXISTS sessions;
      "#,
    )?;
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::db::{CallRecord, HttpSnapshot, SessionSource};
  use bytes::Bytes;

  fn rec(session_id: &str, parts: Vec<(String, Bytes)>) -> CallRecord {
    CallRecord {
      ts: 100,
      session_id: session_id.into(),
      session_source: SessionSource::Header,
      endpoint: "chat_completions".into(),
      account_id: "a".into(),
      provider_id: "p".into(),
      model: "m".into(),
      initiator: "user".into(),
      status: 200,
      stream: false,
      latency_ms: 1,
      prompt_tokens: None,
      completion_tokens: None,
      inbound_req: HttpSnapshot::default(),
      outbound_req: None,
      outbound_resp: None,
      inbound_resp: HttpSnapshot::default(),
      messages: vec![MessageRecord {
        role: "user".into(),
        status: None,
        parts: parts
          .into_iter()
          .map(|(t, c)| PartRecord {
            part_type: t,
            content: c,
          })
          .collect(),
      }],
    }
  }

  #[test]
  fn dedupes_identical_parts_across_messages() {
    let dir = tempdir();
    let path = dir.join("sessions.db");
    let mut db = SessionsDb::open(&path).unwrap();
    let part = ("text".to_string(), Bytes::from_static(b"hello"));
    db.record(&rec("s1", vec![part.clone()])).unwrap();
    db.record(&rec("s2", vec![part.clone()])).unwrap();
    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM message_parts", [], |r| r.get(0))
      .unwrap();
    assert_eq!(count, 1);
    let refs: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM message_part_refs", [], |r| r.get(0))
      .unwrap();
    assert_eq!(refs, 2);
  }

  #[test]
  fn drops_legacy_schema_on_open() {
    let dir = tempdir();
    let path = dir.join("sessions.db");
    {
      let conn = Connection::open(&path).unwrap();
      conn
        .execute_batch(
          r#"
          CREATE TABLE sessions (id TEXT PRIMARY KEY);
          CREATE TABLE messages (id INTEGER PRIMARY KEY, body BLOB NOT NULL);
          INSERT INTO messages (body) VALUES (X'01');
          "#,
        )
        .unwrap();
    }
    let db = SessionsDb::open(&path).unwrap();
    let count: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))
      .unwrap();
    assert_eq!(count, 0);
    // new column exists
    let has_role: bool = db
      .conn
      .prepare("SELECT 1 FROM pragma_table_info('messages') WHERE name = 'role'")
      .unwrap()
      .exists([])
      .unwrap();
    assert!(has_role);
  }

  fn tempdir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("llm-router-sessions-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
  }
}
