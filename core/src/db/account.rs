use std::collections::HashMap;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::SharedConn;

#[derive(Debug, Clone)]
pub struct AccountInformationRecord {
  pub observed_at: DateTime<Utc>,
  pub provider: String,
  pub account_id: String,
  pub user_id: Option<String>,
  pub name: Option<String>,
  pub email: Option<String>,
  pub plan: Option<String>,
  pub quota: Option<String>,
  pub reset_date: Option<String>,
  pub status: String,
  pub metadata: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountInformationView {
  pub provider: String,
  pub account_id: String,
  pub user_id: Option<String>,
  pub name: Option<String>,
  pub email: Option<String>,
  pub plan: Option<String>,
  pub quota: Option<String>,
  pub reset_date: Option<String>,
  pub status: String,
  pub metadata: HashMap<String, Value>,
  pub first_seen_at: DateTime<Utc>,
  pub last_seen_at: DateTime<Utc>,
  pub updated_at: DateTime<Utc>,
  pub disconnected_at: Option<DateTime<Utc>>,
}

pub(super) struct AccountInformationTable {
  conn: SharedConn,
}

impl AccountInformationTable {
  pub(super) fn new(conn: SharedConn) -> Result<Self> {
    let this = Self { conn };
    this.init_schema()?;
    Ok(this)
  }

  fn init_schema(&self) -> Result<()> {
    let conn = self.conn.lock();
    conn.execute_batch(
      "
      CREATE TABLE IF NOT EXISTS account_information (
        provider TEXT NOT NULL,
        account_id TEXT NOT NULL,
        user_id TEXT,
        name TEXT,
        email TEXT,
        plan TEXT,
        quota TEXT,
        reset_date TEXT,
        status TEXT NOT NULL,
        metadata_json TEXT NOT NULL DEFAULT '{}',
        first_seen_at TEXT NOT NULL,
        last_seen_at TEXT NOT NULL,
        updated_at TEXT NOT NULL,
        disconnected_at TEXT,
        PRIMARY KEY(provider, account_id)
      );

      CREATE INDEX IF NOT EXISTS idx_account_information_status_updated_at
      ON account_information(status, updated_at DESC);
      ",
    )?;
    Ok(())
  }

  pub(super) fn upsert(&self, input: AccountInformationRecord) -> Result<()> {
    let conn = self.conn.lock();
    let ts = input.observed_at.to_rfc3339();
    let metadata_json = serde_json::to_string(&input.metadata).context("failed to serialize account metadata")?;

    conn.execute(
      "INSERT INTO account_information(
         provider, account_id, user_id, name, email, plan, quota, reset_date,
         status, metadata_json, first_seen_at, last_seen_at, updated_at, disconnected_at
       ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?11, ?11, NULL)
       ON CONFLICT(provider, account_id)
       DO UPDATE SET
         user_id = excluded.user_id,
         name = excluded.name,
         email = excluded.email,
         plan = excluded.plan,
         quota = excluded.quota,
         reset_date = excluded.reset_date,
         status = excluded.status,
         metadata_json = excluded.metadata_json,
         last_seen_at = excluded.last_seen_at,
         updated_at = excluded.updated_at,
         disconnected_at = excluded.disconnected_at",
      params![
        input.provider,
        input.account_id,
        input.user_id,
        input.name,
        input.email,
        input.plan,
        input.quota,
        input.reset_date,
        input.status,
        metadata_json,
        ts,
      ],
    )?;
    Ok(())
  }

  pub(super) fn mark_disconnected(&self, provider: &str, account_id: &str) -> Result<()> {
    let conn = self.conn.lock();
    let ts = Utc::now().to_rfc3339();
    conn.execute(
      "UPDATE account_information
       SET status = 'disconnected', disconnected_at = ?3, updated_at = ?3
       WHERE provider = ?1 AND account_id = ?2",
      params![provider, account_id, ts],
    )?;
    Ok(())
  }

  pub(super) fn touch_connected(&self, provider: &str, account_id: &str) -> Result<()> {
    let conn = self.conn.lock();
    let ts = Utc::now().to_rfc3339();
    conn.execute(
      "INSERT INTO account_information(
         provider, account_id, user_id, name, email, plan, quota, reset_date,
         status, metadata_json, first_seen_at, last_seen_at, updated_at, disconnected_at
       ) VALUES (?1, ?2, NULL, NULL, NULL, NULL, NULL, NULL, 'connected', '{}', ?3, ?3, ?3, NULL)
       ON CONFLICT(provider, account_id)
       DO UPDATE SET
         status = 'connected',
         last_seen_at = excluded.last_seen_at,
         updated_at = excluded.updated_at,
         disconnected_at = NULL",
      params![provider, account_id, ts],
    )?;
    Ok(())
  }

  pub(super) fn list(&self, provider: Option<&str>, account_id: Option<&str>) -> Result<Vec<AccountInformationView>> {
    let conn = self.conn.lock();

    let mut rows = Vec::new();

    match (provider, account_id) {
      (Some(p), Some(a)) => {
        let mut stmt = conn.prepare(
          "SELECT provider, account_id, user_id, name, email, plan, quota, reset_date,
                  status, metadata_json, first_seen_at, last_seen_at, updated_at, disconnected_at
           FROM account_information
           WHERE provider = ?1 AND account_id = ?2
           ORDER BY updated_at DESC",
        )?;
        let iter = stmt.query_map(params![p, a], map_view_row)?;
        for row in iter {
          rows.push(row?);
        }
      }
      (Some(p), None) => {
        let mut stmt = conn.prepare(
          "SELECT provider, account_id, user_id, name, email, plan, quota, reset_date,
                  status, metadata_json, first_seen_at, last_seen_at, updated_at, disconnected_at
           FROM account_information
           WHERE provider = ?1
           ORDER BY updated_at DESC",
        )?;
        let iter = stmt.query_map(params![p], map_view_row)?;
        for row in iter {
          rows.push(row?);
        }
      }
      (None, Some(a)) => {
        let mut stmt = conn.prepare(
          "SELECT provider, account_id, user_id, name, email, plan, quota, reset_date,
                  status, metadata_json, first_seen_at, last_seen_at, updated_at, disconnected_at
           FROM account_information
           WHERE account_id = ?1
           ORDER BY updated_at DESC",
        )?;
        let iter = stmt.query_map(params![a], map_view_row)?;
        for row in iter {
          rows.push(row?);
        }
      }
      (None, None) => {
        let mut stmt = conn.prepare(
          "SELECT provider, account_id, user_id, name, email, plan, quota, reset_date,
                  status, metadata_json, first_seen_at, last_seen_at, updated_at, disconnected_at
           FROM account_information
           ORDER BY updated_at DESC",
        )?;
        let iter = stmt.query_map([], map_view_row)?;
        for row in iter {
          rows.push(row?);
        }
      }
    }

    Ok(rows)
  }
}

fn map_view_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AccountInformationView> {
  let metadata_json: String = row.get(9)?;
  let metadata = serde_json::from_str::<HashMap<String, Value>>(&metadata_json).unwrap_or_default();

  Ok(AccountInformationView {
    provider: row.get(0)?,
    account_id: row.get(1)?,
    user_id: row.get(2)?,
    name: row.get(3)?,
    email: row.get(4)?,
    plan: row.get(5)?,
    quota: row.get(6)?,
    reset_date: row.get(7)?,
    status: row.get(8)?,
    metadata,
    first_seen_at: parse_rfc3339(&row.get::<_, String>(10)?)
      .map_err(|e| rusqlite::Error::FromSqlConversionFailure(10, rusqlite::types::Type::Text, Box::new(e)))?,
    last_seen_at: parse_rfc3339(&row.get::<_, String>(11)?)
      .map_err(|e| rusqlite::Error::FromSqlConversionFailure(11, rusqlite::types::Type::Text, Box::new(e)))?,
    updated_at: parse_rfc3339(&row.get::<_, String>(12)?)
      .map_err(|e| rusqlite::Error::FromSqlConversionFailure(12, rusqlite::types::Type::Text, Box::new(e)))?,
    disconnected_at: row
      .get::<_, Option<String>>(13)?
      .as_deref()
      .map(parse_rfc3339)
      .transpose()
      .map_err(|e| rusqlite::Error::FromSqlConversionFailure(13, rusqlite::types::Type::Text, Box::new(e)))?,
  })
}

fn parse_rfc3339(ts: &str) -> std::result::Result<DateTime<Utc>, chrono::ParseError> {
  Ok(DateTime::parse_from_rfc3339(&ts)?.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;

  use chrono::Utc;
  use serde_json::json;

  use crate::db::{AccountInformationRecord, RequestStore};

  #[test]
  fn schema_init_is_idempotent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let _store = RequestStore::new(&db).expect("first init");
    let _store2 = RequestStore::new(&db).expect("second init");
  }

  #[test]
  fn upsert_and_list_round_trip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");

    let mut metadata = HashMap::new();
    metadata.insert("github_login".to_string(), json!("alice"));
    metadata.insert("chat_enabled".to_string(), json!(true));

    store
      .upsert_account_information(AccountInformationRecord {
        observed_at: Utc::now(),
        provider: "github_copilot".to_string(),
        account_id: "copilot-github-com".to_string(),
        user_id: Some("123".to_string()),
        name: Some("Alice".to_string()),
        email: Some("alice@example.com".to_string()),
        plan: Some("pro".to_string()),
        quota: Some("{\"remaining\":10}".to_string()),
        reset_date: Some("2026-04-12T00:00:00Z".to_string()),
        status: "connected".to_string(),
        metadata,
      })
      .expect("upsert");

    let rows = store
      .list_account_information(Some("github_copilot"), Some("copilot-github-com"))
      .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].user_id.as_deref(), Some("123"));
    assert_eq!(rows[0].metadata.get("github_login"), Some(&json!("alice")));
  }

  #[test]
  fn upsert_updates_existing_row_and_keeps_first_seen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");

    let observed_first = Utc::now();
    store
      .upsert_account_information(AccountInformationRecord {
        observed_at: observed_first,
        provider: "github_copilot".to_string(),
        account_id: "copilot-github-com".to_string(),
        user_id: Some("1".to_string()),
        name: Some("Alice".to_string()),
        email: None,
        plan: None,
        quota: None,
        reset_date: None,
        status: "connected".to_string(),
        metadata: HashMap::new(),
      })
      .expect("upsert first");

    let observed_second = observed_first + chrono::Duration::minutes(5);
    store
      .upsert_account_information(AccountInformationRecord {
        observed_at: observed_second,
        provider: "github_copilot".to_string(),
        account_id: "copilot-github-com".to_string(),
        user_id: Some("1".to_string()),
        name: Some("Alice Updated".to_string()),
        email: Some("alice@example.com".to_string()),
        plan: Some("business".to_string()),
        quota: Some("quota".to_string()),
        reset_date: Some("2026-04-12".to_string()),
        status: "connected".to_string(),
        metadata: HashMap::new(),
      })
      .expect("upsert second");

    let rows = store.list_account_information(None, None).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].name.as_deref(), Some("Alice Updated"));
    assert_eq!(rows[0].first_seen_at, observed_first);
    assert_eq!(rows[0].last_seen_at, observed_second);
  }

  #[test]
  fn mark_disconnected_sets_status_without_deleting() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");

    store
      .upsert_account_information(AccountInformationRecord {
        observed_at: Utc::now(),
        provider: "github_copilot".to_string(),
        account_id: "copilot-github-com".to_string(),
        user_id: Some("1".to_string()),
        name: Some("Alice".to_string()),
        email: None,
        plan: None,
        quota: None,
        reset_date: None,
        status: "connected".to_string(),
        metadata: HashMap::new(),
      })
      .expect("upsert");

    store
      .mark_account_information_disconnected("github_copilot", "copilot-github-com")
      .expect("mark disconnected");

    let rows = store
      .list_account_information(Some("github_copilot"), Some("copilot-github-com"))
      .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "disconnected");
    assert!(rows[0].disconnected_at.is_some());
  }

  #[test]
  fn metadata_round_trip() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");

    let mut metadata = HashMap::new();
    metadata.insert("quota_snapshots".to_string(), json!({"chat": {"remaining": 42}}));

    store
      .upsert_account_information(AccountInformationRecord {
        observed_at: Utc::now(),
        provider: "github_copilot".to_string(),
        account_id: "copilot-github-com".to_string(),
        user_id: None,
        name: None,
        email: None,
        plan: None,
        quota: None,
        reset_date: None,
        status: "connected".to_string(),
        metadata,
      })
      .expect("upsert");

    let rows = store.list_account_information(None, None).expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(
      rows[0].metadata.get("quota_snapshots"),
      Some(&json!({"chat": {"remaining": 42}}))
    );
  }

  #[test]
  fn touch_connected_updates_status_without_erasing_fields() {
    let temp = tempfile::tempdir().expect("tempdir");
    let db = temp.path().join("state.db");
    let store = RequestStore::new(&db).expect("init");

    store
      .upsert_account_information(AccountInformationRecord {
        observed_at: Utc::now(),
        provider: "github_copilot".to_string(),
        account_id: "copilot-github-com".to_string(),
        user_id: Some("1".to_string()),
        name: Some("Alice".to_string()),
        email: None,
        plan: None,
        quota: None,
        reset_date: None,
        status: "connected".to_string(),
        metadata: HashMap::new(),
      })
      .expect("upsert");
    store
      .mark_account_information_disconnected("github_copilot", "copilot-github-com")
      .expect("disconnect");

    store
      .touch_account_information_connected("github_copilot", "copilot-github-com")
      .expect("touch connected");

    let rows = store
      .list_account_information(Some("github_copilot"), Some("copilot-github-com"))
      .expect("list");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status, "connected");
    assert_eq!(rows[0].user_id.as_deref(), Some("1"));
    assert_eq!(rows[0].name.as_deref(), Some("Alice"));
    assert_eq!(rows[0].disconnected_at, None);
  }
}
