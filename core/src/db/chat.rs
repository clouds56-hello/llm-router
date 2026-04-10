use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};

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
        latest_request_id TEXT
      );

      CREATE TABLE IF NOT EXISTS chat_messages (
        id TEXT PRIMARY KEY,
        conversation_id TEXT NOT NULL,
        seq INTEGER NOT NULL,
        role TEXT NOT NULL,
        content_text TEXT NOT NULL,
        raw_json TEXT NOT NULL,
        created_at TEXT NOT NULL
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_messages_conversation_seq ON chat_messages(conversation_id, seq);
      CREATE INDEX IF NOT EXISTS idx_chat_messages_conversation ON chat_messages(conversation_id);
      ",
    )?;
    Ok(())
  }

  pub(super) fn record_chat_history(&self, input: ChatHistoryRecord) -> Result<()> {
    let conn = self.conn.lock();
    let tx = conn.unchecked_transaction()?;
    tx.execute(
      "INSERT OR REPLACE INTO chat_conversations(id, created_at, updated_at, provider, account_id, model, latest_request_id)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
      params![
        input.conversation_id,
        input.created_at.to_rfc3339(),
        input.created_at.to_rfc3339(),
        input.provider,
        input.account_id,
        input.model,
        input.latest_request_id
      ],
    )?;

    for (seq, message) in input.messages.into_iter().enumerate() {
      tx.execute(
        "INSERT OR REPLACE INTO chat_messages(id, conversation_id, seq, role, content_text, raw_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
          uuid::Uuid::new_v4().to_string(),
          input.conversation_id,
          seq as i64,
          message.role,
          message.content_text,
          message.raw_json,
          input.created_at.to_rfc3339(),
        ],
      )?;
    }

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
    let next_seq: i64 = tx.query_row(
      "SELECT COALESCE(MAX(seq), -1) + 1 FROM chat_messages WHERE conversation_id = ?1",
      params![conversation_id],
      |row| row.get(0),
    )?;
    tx.execute(
      "INSERT INTO chat_messages(id, conversation_id, seq, role, content_text, raw_json, created_at)
       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
      params![
        uuid::Uuid::new_v4().to_string(),
        conversation_id,
        next_seq,
        role,
        content_text,
        raw_json,
        created_at.to_rfc3339(),
      ],
    )?;
    tx.execute(
      "UPDATE chat_conversations SET updated_at = ?2 WHERE id = ?1",
      params![conversation_id, created_at.to_rfc3339()],
    )?;
    tx.commit()?;
    Ok(())
  }

  pub(super) fn prune_older_than(&self, cutoff_ts: &str) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "DELETE FROM chat_messages
       WHERE conversation_id IN (
         SELECT id FROM chat_conversations WHERE updated_at < ?1
       )",
      params![cutoff_ts],
    )?;
    conn.execute(
      "DELETE FROM chat_conversations WHERE updated_at < ?1",
      params![cutoff_ts],
    )?;
    Ok(())
  }

  pub(super) fn query_conversations(&self, limit: usize) -> Result<Vec<ConversationView>> {
    let conn = self.conn.lock();
    let mut stmt = conn.prepare(
      "SELECT id, created_at, updated_at, provider, account_id, model, latest_request_id
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
      ))
    })?;

    let mut out = Vec::new();
    for row in rows {
      let (id, created_at, updated_at, provider, account_id, model, latest_request_id) = row?;
      let mut msg_stmt = conn.prepare(
        "SELECT seq, role, content_text, created_at
         FROM chat_messages
         WHERE conversation_id = ?1
         ORDER BY seq ASC",
      )?;
      let msg_rows = msg_stmt.query_map(params![id.clone()], |row| {
        let created_at_raw: String = row.get(3)?;
        Ok(ConversationMessageView {
          seq: row.get(0)?,
          role: row.get(1)?,
          content_text: row.get(2)?,
          created_at: parse_ts(&created_at_raw),
        })
      })?;

      let mut messages = Vec::new();
      for msg in msg_rows {
        messages.push(msg?);
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
        message_count: messages.len() as i64,
        preview,
        messages,
      });
    }
    Ok(out)
  }
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
  use rusqlite::Connection;

  use crate::db::{ChatHistoryRecord, ChatMessageRecord, RequestStore};

  #[test]
  fn chat_messages_keep_sequence() {
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
        messages: vec![
          ChatMessageRecord {
            role: "system".to_string(),
            content_text: "s".to_string(),
            raw_json: r#"{"role":"system","content":"s"}"#.to_string(),
          },
          ChatMessageRecord {
            role: "user".to_string(),
            content_text: "u".to_string(),
            raw_json: r#"{"role":"user","content":"u"}"#.to_string(),
          },
          ChatMessageRecord {
            role: "assistant".to_string(),
            content_text: "a".to_string(),
            raw_json: r#"{"role":"assistant","content":"a"}"#.to_string(),
          },
        ],
      })
      .expect("chat history");

    let conn = Connection::open(db).expect("open");
    let mut stmt = conn
      .prepare("SELECT seq, role FROM chat_messages WHERE conversation_id = 'conv-1' ORDER BY seq ASC")
      .expect("prepare");
    let rows = stmt
      .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))
      .expect("rows")
      .collect::<rusqlite::Result<Vec<_>>>()
      .expect("collect");
    assert_eq!(
      rows,
      vec![
        (0, "system".to_string()),
        (1, "user".to_string()),
        (2, "assistant".to_string())
      ]
    );
  }
}
