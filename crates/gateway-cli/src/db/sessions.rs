use super::{migrate, CallRecord, MessageRecord, PartRecord, Result};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::path::Path;
use tracing::{debug, trace};

const BOOTSTRAP: &str = include_str!("../../../../scripts/migrations/sessions/000_bootstrap.sql");
const MIGRATIONS: &[migrate::Migration] = &[migrate::Migration {
  version: 1,
  name: "initial",
  sql: include_str!("../../../../scripts/migrations/sessions/001_initial.sql"),
}];

pub fn latest_version() -> u32 {
  migrate::latest_version(MIGRATIONS)
}

pub struct SessionsDb {
  conn: Connection,
}

impl SessionsDb {
  pub fn open(path: &Path) -> Result<Self> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)?;
    }
    let mut conn = Connection::open(path)?;
    migrate::apply(
      &mut conn,
      path,
      "sessions",
      migrate::Bootstrap { sql: BOOTSTRAP },
      MIGRATIONS,
    )?;
    Ok(Self { conn })
  }

  /// Append all messages of a single inbound call to the session log. Each
  /// `MessageRecord` becomes one logical "message" (a contiguous group of
  /// `session_parts` rows sharing `message_seq`); each `PartRecord` becomes
  /// one row, with the blob deduplicated in `part_blobs`.
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

    // Resolve the next free part_seq / message_seq for this session up
    // front so we can interleave appends without races (we hold the
    // sqlite write lock for the whole transaction).
    let (mut next_part_seq, mut next_message_seq) = next_seqs(&tx, &r.session_id)?;

    let new_message_count = r.messages.len() as i64;
    let new_part_count: i64 = r.messages.iter().map(|m| m.parts.len() as i64).sum();

    tx.execute(
      "INSERT INTO sessions (id, first_seen_ts, last_seen_ts, source, account_id, provider_id, model, message_count, part_count)
       VALUES (?1, ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
       ON CONFLICT(id) DO UPDATE SET
         last_seen_ts  = excluded.last_seen_ts,
         account_id    = excluded.account_id,
         provider_id   = excluded.provider_id,
         model         = excluded.model,
         message_count = message_count + excluded.message_count,
         part_count    = part_count + excluded.part_count",
      params![
        r.session_id,
        r.ts,
        r.session_source.as_str(),
        r.account_id,
        r.provider_id,
        r.model,
        new_message_count,
        new_part_count,
      ],
    )?;

    for m in &r.messages {
      append_message(&tx, r, m, &mut next_part_seq, next_message_seq)?;
      next_message_seq += 1;
    }
    tx.commit()?;
    trace!(session_id = %r.session_id, "sessions.record: committed");
    Ok(())
  }
}

fn next_seqs(tx: &rusqlite::Transaction<'_>, session_id: &str) -> Result<(i64, i64)> {
  let row: (Option<i64>, Option<i64>) = tx
    .prepare("SELECT MAX(part_seq), MAX(message_seq) FROM session_parts WHERE session_id = ?1")?
    .query_row(params![session_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
  Ok((row.0.map(|v| v + 1).unwrap_or(0), row.1.map(|v| v + 1).unwrap_or(0)))
}

fn append_message(
  tx: &rusqlite::Transaction<'_>,
  r: &CallRecord,
  m: &MessageRecord,
  next_part_seq: &mut i64,
  message_seq: i64,
) -> Result<()> {
  for (idx, part) in m.parts.iter().enumerate() {
    upsert_part_blob(tx, part)?;
    tx.execute(
      "INSERT INTO session_parts
         (session_id, part_seq, message_seq, part_index, ts, endpoint, role, status, part_hash)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
      params![
        r.session_id,
        *next_part_seq,
        message_seq,
        idx as i64,
        r.ts,
        r.endpoint,
        m.role,
        m.status.map(|v| v as i64),
        hash_part(&part.part_type, part.content.as_ref()),
      ],
    )?;
    *next_part_seq += 1;
  }
  Ok(())
}

fn upsert_part_blob(tx: &rusqlite::Transaction<'_>, part: &PartRecord) -> Result<()> {
  let hash = hash_part(&part.part_type, part.content.as_ref());
  tx.execute(
    "INSERT OR IGNORE INTO part_blobs (hash, part_type, content) VALUES (?1, ?2, ?3)",
    params![hash, part.part_type, part.content.as_ref()],
  )?;
  Ok(())
}

fn hash_part(part_type: &str, content: &[u8]) -> String {
  let mut h = Sha256::new();
  h.update(part_type.as_bytes());
  h.update([0u8]);
  h.update(content);
  format!("{:x}", h.finalize())
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
    let blobs: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM part_blobs", [], |r| r.get(0))
      .unwrap();
    assert_eq!(blobs, 1);
    let parts: i64 = db
      .conn
      .query_row("SELECT COUNT(*) FROM session_parts", [], |r| r.get(0))
      .unwrap();
    assert_eq!(parts, 2);
  }

  #[test]
  fn appending_advances_part_seq() {
    let dir = tempdir();
    let path = dir.join("sessions.db");
    let mut db = SessionsDb::open(&path).unwrap();
    db.record(&rec(
      "s1",
      vec![
        ("text".into(), Bytes::from_static(b"hello")),
        ("text".into(), Bytes::from_static(b"world")),
      ],
    ))
    .unwrap();
    db.record(&rec("s1", vec![("text".into(), Bytes::from_static(b"again"))]))
      .unwrap();
    let max_part_seq: i64 = db
      .conn
      .query_row(
        "SELECT MAX(part_seq) FROM session_parts WHERE session_id = 's1'",
        [],
        |r| r.get(0),
      )
      .unwrap();
    assert_eq!(max_part_seq, 2);
    let max_msg_seq: i64 = db
      .conn
      .query_row(
        "SELECT MAX(message_seq) FROM session_parts WHERE session_id = 's1'",
        [],
        |r| r.get(0),
      )
      .unwrap();
    assert_eq!(max_msg_seq, 1);
    let (mc, pc): (i64, i64) = db
      .conn
      .query_row(
        "SELECT message_count, part_count FROM sessions WHERE id = 's1'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
      )
      .unwrap();
    assert_eq!(mc, 2);
    assert_eq!(pc, 3);
  }

  fn tempdir() -> std::path::PathBuf {
    let p = std::env::temp_dir().join(format!("llm-router-sessions-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&p).unwrap();
    p
  }
}
