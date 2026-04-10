use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::params;

use super::SharedConn;

#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
  pub prompt_tokens: i64,
  pub completion_tokens: i64,
  pub total_tokens: i64,
}

#[derive(Debug, Clone)]
pub struct UsageRecord {
  pub used_at: DateTime<Utc>,
  pub provider: String,
  pub account_id: Option<String>,
  pub model: String,
  pub usage: TokenUsage,
}

pub(super) struct UsageTable {
  conn: SharedConn,
}

impl UsageTable {
  pub(super) fn new(conn: SharedConn) -> Result<Self> {
    let this = Self { conn };
    this.init_schema()?;
    Ok(this)
  }

  fn init_schema(&self) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute_batch(
      "
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
    Ok(())
  }

  pub(super) fn apply_usage(&self, input: UsageRecord) -> Result<()> {
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
}

fn normalize_account_key(account_id: Option<&str>) -> String {
  account_id.unwrap_or_default().trim().to_string()
}

#[cfg(test)]
mod tests {
  use chrono::Utc;
  use rusqlite::Connection;

  use crate::db::{RequestStore, TokenUsage, UsageRecord};

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
}
