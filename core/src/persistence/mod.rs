use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
  pub prompt_tokens: i64,
  pub completion_tokens: i64,
  pub total_tokens: i64,
}

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
pub struct RequestRecordStart {
  pub request_id: String,
  pub created_at: DateTime<Utc>,
  pub endpoint: String,
  pub provider: String,
  pub adapter: String,
  pub model: String,
  pub account_id: Option<String>,
  pub is_stream: bool,
  pub request_body_json: String,
}

#[derive(Debug, Clone)]
pub struct RequestRecordCompleted {
  pub request_id: String,
  pub completed_at: DateTime<Utc>,
  pub response_body_json: Option<String>,
  pub response_sse_text: Option<String>,
  pub http_status: Option<u16>,
  pub usage: TokenUsage,
  pub latency_ms: i64,
}

#[derive(Debug, Clone)]
pub struct RequestRecordFailed {
  pub request_id: String,
  pub completed_at: DateTime<Utc>,
  pub http_status: Option<u16>,
  pub error_text: String,
  pub response_sse_text: Option<String>,
  pub latency_ms: i64,
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

#[derive(Debug, Clone)]
pub struct UsageRecord {
  pub used_at: DateTime<Utc>,
  pub provider: String,
  pub account_id: Option<String>,
  pub model: String,
  pub usage: TokenUsage,
}

#[derive(Clone)]
pub struct RequestStore {
  db: Arc<SqliteRequestStore>,
}

impl RequestStore {
  pub fn new(db_path: &Path) -> Result<Self> {
    Ok(Self {
      db: Arc::new(SqliteRequestStore::new(db_path)?),
    })
  }

  pub fn record_request_started(&self, input: RequestRecordStart) -> Result<()> {
    self.db.record_request_started(input)
  }

  pub fn record_request_completed(&self, input: RequestRecordCompleted) -> Result<()> {
    self.db.record_request_completed(input)
  }

  pub fn record_request_failed(&self, input: RequestRecordFailed) -> Result<()> {
    self.db.record_request_failed(input)
  }

  pub fn record_chat_history(&self, input: ChatHistoryRecord) -> Result<()> {
    self.db.record_chat_history(input)
  }

  pub fn append_chat_message(
    &self,
    conversation_id: &str,
    created_at: DateTime<Utc>,
    role: &str,
    content_text: &str,
    raw_json: &str,
  ) -> Result<()> {
    self
      .db
      .append_chat_message(conversation_id, created_at, role, content_text, raw_json)
  }

  pub fn apply_usage(&self, input: UsageRecord) -> Result<()> {
    self.db.apply_usage(input)
  }

  pub fn prune_older_than_days(&self, days: i64) -> Result<()> {
    self.db.prune_older_than_days(days)
  }

  pub fn query_conversations(&self, limit: usize) -> Result<Vec<ConversationView>> {
    self.db.query_conversations(limit)
  }

  pub fn start_retention_task(&self, days: i64, every: Duration) {
    let this = self.clone();
    tokio::spawn(async move {
      let mut timer = tokio::time::interval(every);
      loop {
        timer.tick().await;
        if let Err(err) = this.prune_older_than_days(days) {
          tracing::warn!(
            target: "persistence",
            error = %err,
            "failed to prune old request archive rows"
          );
        }
      }
    });
  }
}

struct SqliteRequestStore {
  conn: Mutex<Connection>,
}

impl SqliteRequestStore {
  fn new(db_path: &Path) -> Result<Self> {
    let conn = Connection::open(db_path)?;
    let this = Self { conn: Mutex::new(conn) };
    this.init_schema()?;
    Ok(this)
  }

  fn init_schema(&self) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute_batch(
      "
      CREATE TABLE IF NOT EXISTS llm_requests (
        request_id TEXT PRIMARY KEY,
        created_at TEXT NOT NULL,
        completed_at TEXT,
        endpoint TEXT NOT NULL,
        provider TEXT NOT NULL,
        adapter TEXT NOT NULL,
        model TEXT NOT NULL,
        account_id TEXT,
        is_stream INTEGER NOT NULL,
        request_body_json TEXT NOT NULL,
        response_body_json TEXT,
        response_sse_text TEXT,
        http_status INTEGER,
        error_text TEXT,
        prompt_tokens INTEGER,
        completion_tokens INTEGER,
        total_tokens INTEGER,
        latency_ms INTEGER
      );
      CREATE INDEX IF NOT EXISTS idx_llm_requests_created_at ON llm_requests(created_at DESC);
      CREATE INDEX IF NOT EXISTS idx_llm_requests_provider_account_created_at ON llm_requests(provider, account_id, created_at DESC);
      CREATE INDEX IF NOT EXISTS idx_llm_requests_model_created_at ON llm_requests(model, created_at DESC);

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

      CREATE TABLE IF NOT EXISTS account_usage_totals (
        provider TEXT NOT NULL,
        account_id TEXT NOT NULL,
        model TEXT NOT NULL,
        request_count INTEGER NOT NULL,
        prompt_tokens INTEGER NOT NULL,
        completion_tokens INTEGER NOT NULL,
        total_tokens INTEGER NOT NULL,
        first_used_at TEXT NOT NULL,
        last_used_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY(provider, account_id, model)
      );

      CREATE TABLE IF NOT EXISTS account_usage_daily (
        usage_date TEXT NOT NULL,
        provider TEXT NOT NULL,
        account_id TEXT NOT NULL,
        model TEXT NOT NULL,
        request_count INTEGER NOT NULL,
        prompt_tokens INTEGER NOT NULL,
        completion_tokens INTEGER NOT NULL,
        total_tokens INTEGER NOT NULL,
        last_used_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        PRIMARY KEY(usage_date, provider, account_id, model)
      );
      ",
    )?;

    ensure_column(&conn, "llm_requests", "response_sse_text", "TEXT")?;
    ensure_column(&conn, "llm_requests", "prompt_tokens", "INTEGER")?;
    ensure_column(&conn, "llm_requests", "completion_tokens", "INTEGER")?;
    ensure_column(&conn, "llm_requests", "total_tokens", "INTEGER")?;
    ensure_column(&conn, "llm_requests", "latency_ms", "INTEGER")?;
    Ok(())
  }

  fn record_request_started(&self, input: RequestRecordStart) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "INSERT OR REPLACE INTO llm_requests(
        request_id, created_at, endpoint, provider, adapter, model, account_id, is_stream, request_body_json
      ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
      params![
        input.request_id,
        input.created_at.to_rfc3339(),
        input.endpoint,
        input.provider,
        input.adapter,
        input.model,
        input.account_id,
        bool_to_int(input.is_stream),
        input.request_body_json
      ],
    )?;
    Ok(())
  }

  fn record_request_completed(&self, input: RequestRecordCompleted) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "UPDATE llm_requests
       SET completed_at = ?2,
           response_body_json = ?3,
           response_sse_text = ?4,
           http_status = ?5,
           error_text = NULL,
           prompt_tokens = ?6,
           completion_tokens = ?7,
           total_tokens = ?8,
           latency_ms = ?9
       WHERE request_id = ?1",
      params![
        input.request_id,
        input.completed_at.to_rfc3339(),
        input.response_body_json,
        input.response_sse_text,
        input.http_status.map(i64::from),
        input.usage.prompt_tokens,
        input.usage.completion_tokens,
        input.usage.total_tokens,
        input.latency_ms
      ],
    )?;
    Ok(())
  }

  fn record_request_failed(&self, input: RequestRecordFailed) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute(
      "UPDATE llm_requests
       SET completed_at = ?2,
           response_sse_text = ?3,
           http_status = ?4,
           error_text = ?5,
           latency_ms = ?6
       WHERE request_id = ?1",
      params![
        input.request_id,
        input.completed_at.to_rfc3339(),
        input.response_sse_text,
        input.http_status.map(i64::from),
        input.error_text,
        input.latency_ms
      ],
    )?;
    Ok(())
  }

  fn record_chat_history(&self, input: ChatHistoryRecord) -> Result<()> {
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

  fn append_chat_message(
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

  fn apply_usage(&self, input: UsageRecord) -> Result<()> {
    let conn = self.conn.lock();
    let account_key = normalize_account_key(input.account_id.as_deref());
    let usage_date = input.used_at.format("%Y-%m-%d").to_string();
    let ts = input.used_at.to_rfc3339();

    conn.execute(
      "INSERT INTO account_usage_totals(
         provider, account_id, model,
         request_count, prompt_tokens, completion_tokens, total_tokens,
         first_used_at, last_used_at, updated_at
       ) VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6, ?7, ?7, ?7)
       ON CONFLICT(provider, account_id, model)
       DO UPDATE SET
         request_count = request_count + 1,
         prompt_tokens = prompt_tokens + excluded.prompt_tokens,
         completion_tokens = completion_tokens + excluded.completion_tokens,
         total_tokens = total_tokens + excluded.total_tokens,
         last_used_at = excluded.last_used_at,
         updated_at = excluded.updated_at",
      params![
        input.provider,
        account_key,
        input.model,
        input.usage.prompt_tokens,
        input.usage.completion_tokens,
        input.usage.total_tokens,
        ts,
      ],
    )?;

    conn.execute(
      "INSERT INTO account_usage_daily(
         usage_date, provider, account_id, model,
         request_count, prompt_tokens, completion_tokens, total_tokens,
         last_used_at, updated_at
       ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?8, ?8)
       ON CONFLICT(usage_date, provider, account_id, model)
       DO UPDATE SET
         request_count = request_count + 1,
         prompt_tokens = prompt_tokens + excluded.prompt_tokens,
         completion_tokens = completion_tokens + excluded.completion_tokens,
         total_tokens = total_tokens + excluded.total_tokens,
         last_used_at = excluded.last_used_at,
         updated_at = excluded.updated_at",
      params![
        usage_date,
        input.provider,
        account_key,
        input.model,
        input.usage.prompt_tokens,
        input.usage.completion_tokens,
        input.usage.total_tokens,
        ts,
      ],
    )?;
    Ok(())
  }

  fn prune_older_than_days(&self, days: i64) -> Result<()> {
    let cutoff = Utc::now() - chrono::Duration::days(days);
    let cutoff_ts = cutoff.to_rfc3339();
    let conn = self.conn.lock();
    conn.execute("DELETE FROM llm_requests WHERE created_at < ?1", params![cutoff_ts])?;
    conn.execute(
      "DELETE FROM chat_messages
       WHERE conversation_id IN (
         SELECT id FROM chat_conversations WHERE updated_at < ?1
       )",
      params![cutoff.to_rfc3339()],
    )?;
    conn.execute(
      "DELETE FROM chat_conversations WHERE updated_at < ?1",
      params![cutoff.to_rfc3339()],
    )?;
    Ok(())
  }

  fn query_conversations(&self, limit: usize) -> Result<Vec<ConversationView>> {
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

fn ensure_column(conn: &Connection, table: &str, column: &str, sql_ty: &str) -> Result<()> {
  let pragma = format!("PRAGMA table_info({table})");
  let mut has_column = false;
  let mut stmt = conn.prepare(&pragma)?;
  let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
  for row in rows {
    if row?.as_str() == column {
      has_column = true;
      break;
    }
  }
  if !has_column {
    let sql = format!("ALTER TABLE {table} ADD COLUMN {column} {sql_ty}");
    conn.execute(&sql, [])?;
  }
  Ok(())
}

fn bool_to_int(v: bool) -> i64 {
  if v {
    1
  } else {
    0
  }
}

pub fn normalize_account_key(account_id: Option<&str>) -> String {
  account_id.unwrap_or_default().trim().to_string()
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
  use super::*;

  #[test]
  fn schema_init_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let _store = RequestStore::new(&db).expect("first init");
    let _store2 = RequestStore::new(&db).expect("second init");
  }

  #[test]
  fn usage_upsert_adds_counts() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let now = Utc::now();

    store
      .apply_usage(UsageRecord {
        used_at: now,
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        model: "gpt-5".to_string(),
        usage: TokenUsage {
          prompt_tokens: 11,
          completion_tokens: 7,
          total_tokens: 18,
        },
      })
      .expect("usage 1");
    store
      .apply_usage(UsageRecord {
        used_at: now,
        provider: "openai".to_string(),
        account_id: Some("a1".to_string()),
        model: "gpt-5".to_string(),
        usage: TokenUsage::default(),
      })
      .expect("usage 2");

    let conn = Connection::open(db).expect("open");
    let mut stmt = conn
      .prepare(
        "SELECT request_count, prompt_tokens, completion_tokens, total_tokens
         FROM account_usage_totals
         WHERE provider = 'openai' AND account_id = 'a1' AND model = 'gpt-5'",
      )
      .expect("prepare");
    let row = stmt
      .query_row([], |row| {
        Ok((
          row.get::<_, i64>(0)?,
          row.get::<_, i64>(1)?,
          row.get::<_, i64>(2)?,
          row.get::<_, i64>(3)?,
        ))
      })
      .expect("row");
    assert_eq!(row, (2, 11, 7, 18));
  }

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

  #[test]
  fn prune_removes_old_request_rows() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");
    let old = Utc::now() - chrono::Duration::days(40);
    store
      .record_request_started(RequestRecordStart {
        request_id: "req-old".to_string(),
        created_at: old,
        endpoint: "/v1/chat/completions".to_string(),
        provider: "openai".to_string(),
        adapter: "openai".to_string(),
        model: "gpt-5".to_string(),
        account_id: Some("a1".to_string()),
        is_stream: false,
        request_body_json: "{}".to_string(),
      })
      .expect("start");

    store.prune_older_than_days(30).expect("prune");
    let conn = Connection::open(db).expect("open");
    let cnt: i64 = conn
      .query_row(
        "SELECT COUNT(*) FROM llm_requests WHERE request_id = 'req-old'",
        [],
        |row| row.get(0),
      )
      .expect("count");
    assert_eq!(cnt, 0);
  }
}
