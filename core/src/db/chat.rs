use std::collections::{HashMap, HashSet};

use anyhow::{anyhow, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SharedConn;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationMessageView {
  pub seq: i64,
  pub role: String,
  pub content_text: String,
  pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationView {
  pub id: String,
  pub created_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub provider: String,
  pub account_id: Option<String>,
  pub model: String,
  pub latest_request_id: Option<String>,
  pub message_count: i64,
  pub preview: String,
  pub messages: Vec<ConversationMessageView>,
}

#[derive(Debug, Clone)]
pub struct ChatMessageRecord {
  pub role: String,
  pub content_text: String,
  pub raw_json: String,
}

#[derive(Debug, Clone)]
pub struct ChatHistoryRecord {
  pub conversation_id: String,
  pub created_at: DateTime<Utc>,
  pub provider: String,
  pub account_id: Option<String>,
  pub model: String,
  pub latest_request_id: String,
  pub messages: Vec<ChatMessageRecord>,
}

pub(super) struct ChatTable {
  conn: SharedConn,
}

impl ChatTable {
  pub(super) fn new(conn: SharedConn) -> Result<Self> {
    let this = Self { conn };
    this.init_schema()?;
    Ok(this)
  }

  fn init_schema(&self) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute_batch(
      "
      CREATE TABLE IF NOT EXISTS chat_conversations (
        id TEXT PRIMARY KEY,
        created_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        provider TEXT NOT NULL,
        account_id TEXT,
        model TEXT NOT NULL,
        latest_request_id TEXT,
        messages_json TEXT NOT NULL DEFAULT '[]'
      );

      CREATE TABLE IF NOT EXISTS chat_messages (
        hash TEXT PRIMARY KEY,
        role TEXT NOT NULL,
        content_text TEXT NOT NULL,
        raw_json TEXT NOT NULL,
        occurrence_count INTEGER NOT NULL,
        first_seen_at TEXT NOT NULL,
        last_seen_at TEXT NOT NULL
      );
      ",
    )?;

    let has_legacy_chat_messages =
      table_has_column(&conn, "chat_messages", "conversation_id")? || table_has_column(&conn, "chat_messages", "seq")?;
    let has_v2_columns = table_has_column(&conn, "chat_messages", "hash")?
      && table_has_column(&conn, "chat_conversations", "messages_json")?;
    let has_v1_hash_columns = table_has_column(&conn, "chat_messages", "raw_json_sha256")?
      || table_has_column(&conn, "chat_conversations", "message_hashes_json")?;

    if has_legacy_chat_messages {
      migrate_legacy_schema(&conn)?;
    } else if !has_v2_columns && has_v1_hash_columns {
      migrate_v1_hash_schema(&conn)?;
    } else if !has_v2_columns {
      // Unknown or partial shape: rebuild from whichever columns are available.
      migrate_v1_hash_schema(&conn)?;
    } else {
      ensure_column(
        &conn,
        "chat_conversations",
        "messages_json",
        "TEXT NOT NULL DEFAULT '[]'",
      )?;
    }
    conn.execute(
      "CREATE INDEX IF NOT EXISTS idx_chat_messages_last_seen_at ON chat_messages(last_seen_at DESC)",
      [],
    )?;

    Ok(())
  }

  pub(super) fn record_chat_history(&self, input: ChatHistoryRecord) -> Result<()> {
    let conn = self.conn.lock();
    let tx = conn.unchecked_transaction()?;

    if let Some(existing_hashes_json) = tx
      .query_row(
        "SELECT messages_json FROM chat_conversations WHERE id = ?1",
        params![input.conversation_id.clone()],
        |row| row.get::<_, String>(0),
      )
      .optional()?
    {
      let existing_hashes = parse_hashes_json(&existing_hashes_json);
      decrement_hash_occurrences_tx(&tx, &existing_hashes)?;
    }

    let mut message_hashes = Vec::with_capacity(input.messages.len());
    let now_ts = input.created_at.to_rfc3339();
    for message in input.messages {
      let hash = sha256_hex(&message.raw_json);
      message_hashes.push(hash.clone());
      upsert_message_hash_tx(
        &tx,
        &hash,
        &message.role,
        &message.content_text,
        &message.raw_json,
        &now_ts,
      )?;
    }

    tx.execute(
      "INSERT OR REPLACE INTO chat_conversations(
         id, created_at, updated_at, provider, account_id, model, latest_request_id, messages_json
       ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
      params![
        input.conversation_id,
        now_ts,
        now_ts,
        input.provider,
        input.account_id,
        input.model,
        input.latest_request_id,
        serde_json::to_string(&message_hashes)?,
      ],
    )?;

    tx.execute("DELETE FROM chat_messages WHERE occurrence_count <= 0", [])?;
    tx.commit()?;
    Ok(())
  }

  pub(super) fn append_chat_message(
    &self,
    conversation_id: &str,
    created_at: DateTime<Utc>,
    role: &str,
    content_text: &str,
    raw_json: &str,
  ) -> Result<()> {
    let conn = self.conn.lock();
    let tx = conn.unchecked_transaction()?;
    let created_at_ts = created_at.to_rfc3339();
    let hash = sha256_hex(raw_json);

    upsert_message_hash_tx(&tx, &hash, role, content_text, raw_json, &created_at_ts)?;

    let hashes_json: String = tx
      .query_row(
        "SELECT messages_json FROM chat_conversations WHERE id = ?1",
        params![conversation_id],
        |row| row.get(0),
      )
      .optional()?
      .ok_or_else(|| anyhow!("conversation not found: {conversation_id}"))?;
    let mut hashes = parse_hashes_json(&hashes_json);
    hashes.push(hash);

    tx.execute(
      "UPDATE chat_conversations
       SET messages_json = ?2, updated_at = ?3
       WHERE id = ?1",
      params![conversation_id, serde_json::to_string(&hashes)?, created_at_ts],
    )?;

    tx.commit()?;
    Ok(())
  }

  pub(super) fn prune_older_than(&self, cutoff_ts: &str) -> Result<()> {
    let conn = self.conn.lock();
    let tx = conn.unchecked_transaction()?;

    let mut old_hashes = Vec::new();
    {
      let mut stmt = tx.prepare("SELECT messages_json FROM chat_conversations WHERE updated_at < ?1")?;
      let rows = stmt.query_map(params![cutoff_ts], |row| row.get::<_, String>(0))?;
      for row in rows {
        old_hashes.extend(parse_hashes_json(&row?));
      }
    }

    decrement_hash_occurrences_tx(&tx, &old_hashes)?;
    tx.execute("DELETE FROM chat_messages WHERE occurrence_count <= 0", [])?;
    tx.execute(
      "DELETE FROM chat_conversations WHERE updated_at < ?1",
      params![cutoff_ts],
    )?;
    tx.commit()?;
    Ok(())
  }

  pub(super) fn query_conversations(&self, limit: usize) -> Result<Vec<ConversationView>> {
    let conn = self.conn.lock();
    let mut stmt = conn.prepare(
      "SELECT id, created_at, updated_at, provider, account_id, model, latest_request_id, messages_json
       FROM chat_conversations
       ORDER BY updated_at DESC
       LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit.clamp(1, 500) as i64], |row| {
      let created_at_raw: String = row.get(1)?;
      let updated_at_raw: String = row.get(2)?;
      Ok((
        row.get::<_, String>(0)?,
        parse_ts(&created_at_raw),
        parse_ts(&updated_at_raw),
        row.get::<_, String>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, String>(5)?,
        row.get::<_, Option<String>>(6)?,
        row.get::<_, String>(7)?,
      ))
    })?;

    let mut out = Vec::new();
    for row in rows {
      let (id, created_at, updated_at, provider, account_id, model, latest_request_id, hashes_json) = row?;
      let hashes = parse_hashes_json(&hashes_json);
      let message_map = load_message_map(&conn, &hashes)?;
      let mut messages = Vec::with_capacity(hashes.len());
      for (idx, hash) in hashes.iter().enumerate() {
        if let Some((role, content_text)) = message_map.get(hash) {
          messages.push(ConversationMessageView {
            seq: idx as i64,
            role: role.clone(),
            content_text: content_text.clone(),
            created_at,
          });
        }
      }

      let preview = messages
        .last()
        .map(|m| clip_preview(&m.content_text))
        .unwrap_or_default();
      out.push(ConversationView {
        id,
        created_at,
        updated_at,
        provider,
        account_id,
        model,
        latest_request_id,
        message_count: hashes.len() as i64,
        preview,
        messages,
      });
    }
    Ok(out)
  }
}

fn migrate_legacy_schema(conn: &Connection) -> Result<()> {
  let tx = conn.unchecked_transaction()?;

  tx.execute_batch(
    "
    DROP TABLE IF EXISTS chat_conversations_legacy;
    DROP TABLE IF EXISTS chat_messages_legacy;
    ALTER TABLE chat_conversations RENAME TO chat_conversations_legacy;
    ALTER TABLE chat_messages RENAME TO chat_messages_legacy;
    ",
  )?;

  tx.execute_batch(
    "
    CREATE TABLE chat_conversations (
      id TEXT PRIMARY KEY,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      provider TEXT NOT NULL,
      account_id TEXT,
      model TEXT NOT NULL,
      latest_request_id TEXT,
      messages_json TEXT NOT NULL DEFAULT '[]'
    );

    CREATE TABLE chat_messages (
      hash TEXT PRIMARY KEY,
      role TEXT NOT NULL,
      content_text TEXT NOT NULL,
      raw_json TEXT NOT NULL,
      occurrence_count INTEGER NOT NULL,
      first_seen_at TEXT NOT NULL,
      last_seen_at TEXT NOT NULL
    );
    CREATE INDEX idx_chat_messages_last_seen_at ON chat_messages(last_seen_at DESC);
    ",
  )?;

  let mut conversations = Vec::new();
  {
    let mut conv_stmt = tx.prepare(
      "SELECT id, created_at, updated_at, provider, account_id, model, latest_request_id
       FROM chat_conversations_legacy",
    )?;
    let conv_rows = conv_stmt.query_map([], |row| {
      Ok((
        row.get::<_, String>(0)?,
        row.get::<_, String>(1)?,
        row.get::<_, String>(2)?,
        row.get::<_, String>(3)?,
        row.get::<_, Option<String>>(4)?,
        row.get::<_, String>(5)?,
        row.get::<_, Option<String>>(6)?,
      ))
    })?;
    for row in conv_rows {
      conversations.push(row?);
    }
  }

  for (id, created_at, updated_at, provider, account_id, model, latest_request_id) in conversations {
    let mut hashes = Vec::new();
    let mut msg_stmt = tx.prepare(
      "SELECT role, content_text, raw_json, created_at
       FROM chat_messages_legacy
       WHERE conversation_id = ?1
       ORDER BY seq ASC",
    )?;
    let msg_rows = msg_stmt.query_map(params![id.clone()], |msg_row| {
      Ok((
        msg_row.get::<_, String>(0)?,
        msg_row.get::<_, String>(1)?,
        msg_row.get::<_, String>(2)?,
        msg_row.get::<_, String>(3)?,
      ))
    })?;
    for msg in msg_rows {
      let (role, content_text, raw_json, seen_at) = msg?;
      let hash = sha256_hex(&raw_json);
      hashes.push(hash.clone());
      upsert_message_hash_tx(&tx, &hash, &role, &content_text, &raw_json, &seen_at)?;
    }

    tx.execute(
      "INSERT INTO chat_conversations(
         id, created_at, updated_at, provider, account_id, model, latest_request_id, messages_json
       ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
      params![
        id,
        created_at,
        updated_at,
        provider,
        account_id,
        model,
        latest_request_id,
        serde_json::to_string(&hashes)?,
      ],
    )?;
  }

  tx.execute_batch(
    "
    DROP TABLE chat_messages_legacy;
    DROP TABLE chat_conversations_legacy;
    ",
  )?;
  tx.commit()?;
  Ok(())
}

fn migrate_v1_hash_schema(conn: &Connection) -> Result<()> {
  let tx = conn.unchecked_transaction()?;

  tx.execute_batch(
    "
    DROP TABLE IF EXISTS chat_conversations_v1;
    DROP TABLE IF EXISTS chat_messages_v1;
    ALTER TABLE chat_conversations RENAME TO chat_conversations_v1;
    ALTER TABLE chat_messages RENAME TO chat_messages_v1;
    ",
  )?;

  tx.execute_batch(
    "
    CREATE TABLE chat_conversations (
      id TEXT PRIMARY KEY,
      created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL,
      provider TEXT NOT NULL,
      account_id TEXT,
      model TEXT NOT NULL,
      latest_request_id TEXT,
      messages_json TEXT NOT NULL DEFAULT '[]'
    );

    CREATE TABLE chat_messages (
      hash TEXT PRIMARY KEY,
      role TEXT NOT NULL,
      content_text TEXT NOT NULL,
      raw_json TEXT NOT NULL,
      occurrence_count INTEGER NOT NULL,
      first_seen_at TEXT NOT NULL,
      last_seen_at TEXT NOT NULL
    );
    CREATE INDEX idx_chat_messages_last_seen_at ON chat_messages(last_seen_at DESC);
    ",
  )?;

  tx.execute(
    "
    INSERT INTO chat_conversations(
      id, created_at, updated_at, provider, account_id, model, latest_request_id, messages_json
    )
    SELECT id, created_at, updated_at, provider, account_id, model, latest_request_id,
           COALESCE(message_hashes_json, '[]')
    FROM chat_conversations_v1
    ",
    [],
  )?;

  tx.execute(
    "
    INSERT INTO chat_messages(
      hash, role, content_text, raw_json, occurrence_count, first_seen_at, last_seen_at
    )
    SELECT raw_json_sha256, role, content_text, raw_json, occurrence_count, first_seen_at, last_seen_at
    FROM chat_messages_v1
    ",
    [],
  )?;

  tx.execute_batch(
    "
    DROP TABLE chat_messages_v1;
    DROP TABLE chat_conversations_v1;
    ",
  )?;
  tx.commit()?;
  Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
  let pragma = format!("PRAGMA table_info({table})");
  let mut stmt = conn.prepare(&pragma)?;
  let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
  for row in rows {
    if row?.as_str() == column {
      return Ok(true);
    }
  }
  Ok(false)
}

fn ensure_column(conn: &Connection, table: &str, column: &str, sql_ty: &str) -> Result<()> {
  if table_has_column(conn, table, column)? {
    return Ok(());
  }
  let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {sql_ty}");
  conn.execute(&sql, [])?;
  Ok(())
}

fn upsert_message_hash_tx(
  tx: &rusqlite::Transaction<'_>,
  hash: &str,
  role: &str,
  content_text: &str,
  raw_json: &str,
  seen_at: &str,
) -> Result<()> {
  tx.execute(
    "INSERT INTO chat_messages(
       hash, role, content_text, raw_json, occurrence_count, first_seen_at, last_seen_at
     ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?5)
     ON CONFLICT(hash) DO UPDATE SET
       occurrence_count = occurrence_count + 1,
       last_seen_at = excluded.last_seen_at",
    params![hash, role, content_text, raw_json, seen_at],
  )?;
  Ok(())
}

fn decrement_hash_occurrences_tx(tx: &rusqlite::Transaction<'_>, hashes: &[String]) -> Result<()> {
  for hash in hashes {
    tx.execute(
      "UPDATE chat_messages
       SET occurrence_count = occurrence_count - 1
       WHERE hash = ?1",
      params![hash],
    )?;
  }
  Ok(())
}

fn parse_hashes_json(raw: &str) -> Vec<String> {
  serde_json::from_str::<Vec<String>>(raw).unwrap_or_default()
}

fn load_message_map(conn: &Connection, hashes: &[String]) -> Result<HashMap<String, (String, String)>> {
  let unique_hashes: HashSet<String> = hashes.iter().cloned().collect();
  if unique_hashes.is_empty() {
    return Ok(HashMap::new());
  }
  let mut sql = String::from("SELECT hash, role, content_text FROM chat_messages WHERE hash IN (");
  for (idx, _) in unique_hashes.iter().enumerate() {
    if idx > 0 {
      sql.push(',');
    }
    sql.push('?');
    sql.push_str(&(idx + 1).to_string());
  }
  sql.push(')');

  let mut stmt = conn.prepare(&sql)?;
  let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(unique_hashes.len());
  let sorted_hashes: Vec<String> = unique_hashes.into_iter().collect();
  for hash in &sorted_hashes {
    values.push(hash);
  }
  let rows = stmt.query_map(values.as_slice(), |row| {
    Ok((
      row.get::<_, String>(0)?,
      row.get::<_, String>(1)?,
      row.get::<_, String>(2)?,
    ))
  })?;

  let mut out = HashMap::new();
  for row in rows {
    let (hash, role, content_text) = row?;
    out.insert(hash, (role, content_text));
  }
  Ok(out)
}

fn sha256_hex(input: &str) -> String {
  let digest = Sha256::digest(input.as_bytes());
  let mut out = String::with_capacity(64);
  for b in digest {
    out.push_str(&format!("{b:02x}"));
  }
  out
}

fn parse_ts(raw: &str) -> DateTime<Utc> {
  DateTime::parse_from_rfc3339(raw)
    .map(|v| v.with_timezone(&Utc))
    .unwrap_or_else(|_| Utc::now())
}

fn clip_preview(input: &str) -> String {
  const MAX: usize = 140;
  if input.chars().count() <= MAX {
    return input.to_string();
  }
  let mut out: String = input.chars().take(MAX).collect();
  out.push('…');
  out
}

#[cfg(test)]
mod tests {
  use chrono::Utc;
  use rusqlite::{params, Connection};

  use crate::db::{ChatHistoryRecord, ChatMessageRecord, RequestStore};

  fn msg(role: &str, content: &str) -> ChatMessageRecord {
    ChatMessageRecord {
      role: role.to_string(),
      content_text: content.to_string(),
      raw_json: format!(r#"{{"role":"{role}","content":"{content}"}}"#),
    }
  }

  #[test]
  fn dedup_keeps_single_raw_json_row_with_occurrence_count() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let now = Utc::now();

    store
      .record_chat_history(ChatHistoryRecord {
        conversation_id: "conv-1".to_string(),
        created_at: now,
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        model: "gpt-5".to_string(),
        latest_request_id: "req-1".to_string(),
        messages: vec![msg("user", "same")],
      })
      .expect("history 1");
    store
      .record_chat_history(ChatHistoryRecord {
        conversation_id: "conv-2".to_string(),
        created_at: now,
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        model: "gpt-5".to_string(),
        latest_request_id: "req-2".to_string(),
        messages: vec![msg("user", "same")],
      })
      .expect("history 2");

    let conn = Connection::open(db).expect("open");
    let (rows, occurrences): (i64, i64) = conn
      .query_row(
        "SELECT COUNT(*), SUM(occurrence_count) FROM chat_messages WHERE content_text = 'same'",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
      )
      .expect("count");
    assert_eq!(rows, 1);
    assert_eq!(occurrences, 2);
  }

  #[test]
  fn query_preserves_message_order_and_sequence() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let now = Utc::now();

    store
      .record_chat_history(ChatHistoryRecord {
        conversation_id: "conv-1".to_string(),
        created_at: now,
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        model: "gpt-5".to_string(),
        latest_request_id: "req-1".to_string(),
        messages: vec![msg("system", "s"), msg("user", "u"), msg("assistant", "a")],
      })
      .expect("history");

    let conversations = store.query_conversations(10).expect("query");
    let conv = conversations.first().expect("conversation");
    let got: Vec<(i64, String, String)> = conv
      .messages
      .iter()
      .map(|m| (m.seq, m.role.clone(), m.content_text.clone()))
      .collect();
    assert_eq!(
      got,
      vec![
        (0, "system".to_string(), "s".to_string()),
        (1, "user".to_string(), "u".to_string()),
        (2, "assistant".to_string(), "a".to_string())
      ]
    );
    assert_eq!(conv.message_count, 3);
  }

  #[test]
  fn append_updates_hash_array_and_occurrence_counter() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let now = Utc::now();

    store
      .record_chat_history(ChatHistoryRecord {
        conversation_id: "conv-1".to_string(),
        created_at: now,
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        model: "gpt-5".to_string(),
        latest_request_id: "req-1".to_string(),
        messages: vec![msg("user", "hi")],
      })
      .expect("history");
    store
      .append_chat_message(
        "conv-1",
        now,
        "assistant",
        "hello",
        r#"{"role":"assistant","content":"hello"}"#,
      )
      .expect("append");

    let conv = store
      .query_conversations(10)
      .expect("query")
      .into_iter()
      .find(|c| c.id == "conv-1")
      .expect("conv");
    assert_eq!(conv.message_count, 2);
    assert_eq!(conv.messages.len(), 2);
    assert_eq!(conv.messages[1].content_text, "hello");

    let conn = Connection::open(db).expect("open");
    let cnt: i64 = conn
      .query_row(
        "SELECT occurrence_count FROM chat_messages WHERE raw_json = ?1",
        params![r#"{"role":"assistant","content":"hello"}"#],
        |row| row.get(0),
      )
      .expect("count");
    assert_eq!(cnt, 1);
  }

  #[test]
  fn prune_decrements_occurrence_and_removes_unreferenced_messages() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let old = Utc::now() - chrono::Duration::days(40);
    let fresh = Utc::now();

    store
      .record_chat_history(ChatHistoryRecord {
        conversation_id: "conv-old".to_string(),
        created_at: old,
        provider: "openai".to_string(),
        account_id: None,
        model: "gpt-5".to_string(),
        latest_request_id: "req-old".to_string(),
        messages: vec![msg("user", "shared"), msg("assistant", "unique-old")],
      })
      .expect("old");
    store
      .record_chat_history(ChatHistoryRecord {
        conversation_id: "conv-fresh".to_string(),
        created_at: fresh,
        provider: "openai".to_string(),
        account_id: None,
        model: "gpt-5".to_string(),
        latest_request_id: "req-fresh".to_string(),
        messages: vec![msg("user", "shared")],
      })
      .expect("fresh");

    store.prune_older_than_days(30).expect("prune");

    let conn = Connection::open(db).expect("open");
    let shared_occ: i64 = conn
      .query_row(
        "SELECT occurrence_count FROM chat_messages WHERE content_text = 'shared'",
        [],
        |row| row.get(0),
      )
      .expect("shared");
    assert_eq!(shared_occ, 1);

    let unique_old_cnt: i64 = conn
      .query_row(
        "SELECT COUNT(*) FROM chat_messages WHERE content_text = 'unique-old'",
        [],
        |row| row.get(0),
      )
      .expect("unique old");
    assert_eq!(unique_old_cnt, 0);
  }

  #[test]
  fn migrates_legacy_rows_in_place() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let conn = Connection::open(&db).expect("open");
    conn
      .execute_batch(
        "
        CREATE TABLE chat_conversations (
          id TEXT PRIMARY KEY,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          provider TEXT NOT NULL,
          account_id TEXT,
          model TEXT NOT NULL,
          latest_request_id TEXT
        );
        CREATE TABLE chat_messages (
          id TEXT PRIMARY KEY,
          conversation_id TEXT NOT NULL,
          seq INTEGER NOT NULL,
          role TEXT NOT NULL,
          content_text TEXT NOT NULL,
          raw_json TEXT NOT NULL,
          created_at TEXT NOT NULL
        );
        ",
      )
      .expect("legacy schema");
    let ts = Utc::now().to_rfc3339();
    conn
      .execute(
        "INSERT INTO chat_conversations(id, created_at, updated_at, provider, account_id, model, latest_request_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params!["conv-1", ts, ts, "openai", "a1", "gpt-5", "req-1"],
      )
      .expect("insert conv");
    conn
      .execute(
        "INSERT INTO chat_messages(id, conversation_id, seq, role, content_text, raw_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
          "m-1",
          "conv-1",
          0i64,
          "user",
          "hello",
          r#"{"role":"user","content":"hello"}"#,
          Utc::now().to_rfc3339(),
        ],
      )
      .expect("insert msg");
    drop(conn);

    let store = RequestStore::new(&db).expect("init + migrate");
    let convs = store.query_conversations(10).expect("query");
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].message_count, 1);
    assert_eq!(convs[0].messages[0].content_text, "hello");

    let conn2 = Connection::open(&db).expect("open 2");
    let has_hash_col: i64 = conn2
      .query_row(
        "SELECT COUNT(*) FROM pragma_table_info('chat_messages') WHERE name = 'hash'",
        [],
        |row| row.get(0),
      )
      .expect("hash col");
    assert_eq!(has_hash_col, 1);
  }

  #[test]
  fn migrates_v1_hash_columns_to_new_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let conn = Connection::open(&db).expect("open");
    conn
      .execute_batch(
        "
        CREATE TABLE chat_conversations (
          id TEXT PRIMARY KEY,
          created_at TEXT NOT NULL,
          updated_at TEXT NOT NULL,
          provider TEXT NOT NULL,
          account_id TEXT,
          model TEXT NOT NULL,
          latest_request_id TEXT,
          message_hashes_json TEXT NOT NULL DEFAULT '[]'
        );
        CREATE TABLE chat_messages (
          raw_json_sha256 TEXT PRIMARY KEY,
          role TEXT NOT NULL,
          content_text TEXT NOT NULL,
          raw_json TEXT NOT NULL,
          occurrence_count INTEGER NOT NULL,
          first_seen_at TEXT NOT NULL,
          last_seen_at TEXT NOT NULL
        );
        ",
      )
      .expect("v1 schema");
    let ts = Utc::now().to_rfc3339();
    conn
      .execute(
        "INSERT INTO chat_conversations(
           id, created_at, updated_at, provider, account_id, model, latest_request_id, message_hashes_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params!["conv-1", ts, ts, "openai", "a1", "gpt-5", "req-1", "[\"h1\"]"],
      )
      .expect("insert conv");
    conn
      .execute(
        "INSERT INTO chat_messages(
           raw_json_sha256, role, content_text, raw_json, occurrence_count, first_seen_at, last_seen_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
          "h1",
          "user",
          "hello",
          r#"{"role":"user","content":"hello"}"#,
          1i64,
          ts,
          ts
        ],
      )
      .expect("insert msg");
    drop(conn);

    let store = RequestStore::new(&db).expect("init + migrate");
    let convs = store.query_conversations(10).expect("query");
    assert_eq!(convs.len(), 1);
    assert_eq!(convs[0].message_count, 1);
    assert_eq!(convs[0].messages[0].content_text, "hello");

    let conn2 = Connection::open(&db).expect("open 2");
    let has_hash_col: i64 = conn2
      .query_row(
        "SELECT COUNT(*) FROM pragma_table_info('chat_messages') WHERE name = 'hash'",
        [],
        |row| row.get(0),
      )
      .expect("hash col");
    let has_messages_json_col: i64 = conn2
      .query_row(
        "SELECT COUNT(*) FROM pragma_table_info('chat_conversations') WHERE name = 'messages_json'",
        [],
        |row| row.get(0),
      )
      .expect("messages json col");
    assert_eq!(has_hash_col, 1);
    assert_eq!(has_messages_json_col, 1);
  }

  #[test]
  fn schema_init_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let _store = RequestStore::new(&db).expect("first init");
    let _store2 = RequestStore::new(&db).expect("second init");
  }
}
