use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use rusqlite::{params, params_from_iter, types::Value, Connection};
use serde::{Deserialize, Serialize};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
  pub id: String,
  pub ts: DateTime<Utc>,
  pub level: String,
  pub target: String,
  pub message: String,
  pub request_id: Option<String>,
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LogQuery {
  pub limit: Option<usize>,
  pub level: Option<String>,
  pub request_id: Option<String>,
}

#[derive(Clone)]
pub struct LogStore {
  max: usize,
  memory: Arc<Mutex<VecDeque<LogEntry>>>,
  db: Arc<SqliteLogStore>,
}

impl LogStore {
  pub fn new(db_path: &Path, max: usize) -> Result<Self> {
    let db = SqliteLogStore::new(db_path)?;
    Ok(Self {
      max,
      memory: Arc::new(Mutex::new(VecDeque::with_capacity(max))),
      db: Arc::new(db),
    })
  }

  pub fn push(&self, entry: LogEntry) {
    {
      let mut guard = self.memory.lock();
      guard.push_front(entry.clone());
      while guard.len() > self.max {
        guard.pop_back();
      }
    }

    if let Err(err) = self.db.insert(&entry) {
      tracing::warn!(target: "logging", error = %err, "failed to persist log entry");
    }
  }

  pub fn query(&self, query: LogQuery) -> Result<Vec<LogEntry>> {
    self.db.query(query)
  }

  pub fn prune_older_than_days(&self, days: i64) -> Result<()> {
    self.db.prune_older_than_days(days)
  }

  pub fn start_retention_task(&self, days: i64, every: Duration) {
    let this = self.clone();
    tokio::spawn(async move {
      let mut timer = tokio::time::interval(every);
      loop {
        timer.tick().await;
        if let Err(err) = this.prune_older_than_days(days) {
          tracing::warn!(target: "logging", error = %err, "failed to prune old logs");
        }
      }
    });
  }
}

struct SqliteLogStore {
  conn: Mutex<Connection>,
}

impl SqliteLogStore {
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
      CREATE TABLE IF NOT EXISTS logs (
        id TEXT PRIMARY KEY,
        ts TEXT NOT NULL,
        level TEXT NOT NULL,
        target TEXT NOT NULL,
        message TEXT NOT NULL,
        request_id TEXT,
        metadata TEXT NOT NULL DEFAULT '{}'
      );
      CREATE INDEX IF NOT EXISTS idx_logs_ts ON logs(ts DESC);
      CREATE INDEX IF NOT EXISTS idx_logs_level_ts ON logs(level, ts DESC);
      CREATE INDEX IF NOT EXISTS idx_logs_request_id_ts ON logs(request_id, ts DESC);
      ",
    )?;
    let mut has_metadata = false;
    let mut stmt = conn.prepare("PRAGMA table_info(logs)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
      if row?.as_str() == "metadata" {
        has_metadata = true;
        break;
      }
    }
    if !has_metadata {
      conn.execute("ALTER TABLE logs ADD COLUMN metadata TEXT NOT NULL DEFAULT '{}'", [])?;
    }
    Ok(())
  }

  fn insert(&self, entry: &LogEntry) -> Result<()> {
    let conn = self.conn.lock();
    let metadata_json = serde_json::to_string(&entry.metadata)?;
    conn.execute(
      "INSERT INTO logs(id, ts, level, target, message, request_id, metadata) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
      params![
        entry.id,
        entry.ts.to_rfc3339(),
        entry.level,
        entry.target,
        entry.message,
        entry.request_id,
        metadata_json,
      ],
    )?;
    Ok(())
  }

  fn query(&self, query: LogQuery) -> Result<Vec<LogEntry>> {
    let mut sql = String::from("SELECT id, ts, level, target, message, request_id, metadata FROM logs");
    let mut where_parts = Vec::new();
    let mut values: Vec<Value> = Vec::new();

    if let Some(level) = query.level.filter(|s| !s.trim().is_empty()) {
      where_parts.push("level = ?".to_string());
      values.push(Value::from(level.to_uppercase()));
    }

    if let Some(request_id) = query.request_id.filter(|s| !s.trim().is_empty()) {
      where_parts.push("request_id = ?".to_string());
      values.push(Value::from(request_id));
    }

    if !where_parts.is_empty() {
      sql.push_str(" WHERE ");
      sql.push_str(&where_parts.join(" AND "));
    }

    sql.push_str(" ORDER BY ts DESC LIMIT ?");
    let limit = query.limit.unwrap_or(500).clamp(1, 5000) as i64;
    values.push(Value::from(limit));

    let conn = self.conn.lock();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(values), |row| {
      let ts_raw: String = row.get(1)?;
      let ts = DateTime::parse_from_rfc3339(&ts_raw)
        .map(|v| v.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
      Ok(LogEntry {
        id: row.get(0)?,
        ts,
        level: row.get(2)?,
        target: row.get(3)?,
        message: row.get(4)?,
        request_id: row.get(5)?,
        metadata: row
          .get::<_, String>(6)
          .ok()
          .and_then(|raw| serde_json::from_str::<BTreeMap<String, String>>(&raw).ok())
          .unwrap_or_default(),
      })
    })?;

    let mut out = Vec::new();
    for row in rows {
      out.push(row?);
    }
    Ok(out)
  }

  fn prune_older_than_days(&self, days: i64) -> Result<()> {
    let cutoff = Utc::now() - chrono::Duration::days(days);
    let conn = self.conn.lock();
    conn.execute("DELETE FROM logs WHERE ts < ?1", params![cutoff.to_rfc3339()])?;
    Ok(())
  }
}

#[derive(Clone)]
pub struct LogCaptureLayer {
  store: LogStore,
}

impl LogCaptureLayer {
  pub fn new(store: LogStore) -> Self {
    Self { store }
  }
}

#[derive(Debug, Clone)]
struct SpanRequestId(String);

impl<S> Layer<S> for LogCaptureLayer
where
  S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
  fn on_new_span(&self, attrs: &tracing::span::Attributes<'_>, id: &tracing::span::Id, ctx: Context<'_, S>) {
    let mut visitor = FieldVisitor::default();
    attrs.record(&mut visitor);
    if let Some(request_id) = visitor.request_id {
      if let Some(span) = ctx.span(id) {
        span.extensions_mut().insert(SpanRequestId(request_id));
      }
    }
  }

  fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
    let mut visitor = FieldVisitor::default();
    event.record(&mut visitor);

    let message = visitor.message.unwrap_or_default();
    let metadata = visitor.fields;

    let mut request_id = visitor.request_id;
    if request_id.is_none() {
      if let Some(scope) = ctx.event_scope(event) {
        for span in scope.from_root() {
          if let Some(found) = span.extensions().get::<SpanRequestId>() {
            request_id = Some(found.0.clone());
          }
        }
      }
    }

    let meta = event.metadata();
    self.store.push(LogEntry {
      id: uuid::Uuid::new_v4().to_string(),
      ts: Utc::now(),
      level: meta.level().as_str().to_string(),
      target: meta.target().to_string(),
      message,
      request_id,
      metadata,
    });
  }
}

#[derive(Default)]
struct FieldVisitor {
  message: Option<String>,
  request_id: Option<String>,
  fields: BTreeMap<String, String>,
}

impl tracing::field::Visit for FieldVisitor {
  fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
    if field.name() == "message" {
      self.message = Some(value.to_string());
      return;
    }
    if field.name() == "request_id" {
      self.request_id = Some(value.to_string());
      return;
    }
    self.fields.insert(field.name().to_string(), value.to_string());
  }

  fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
    if field.name() == "message" {
      self.message = Some(format!("{value:?}"));
      return;
    }
    if field.name() == "request_id" {
      self.request_id = Some(format!("{value:?}").trim_matches('"').to_string());
      return;
    }
    self.fields.insert(field.name().to_string(), format!("{value:?}"));
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use tracing_subscriber::layer::SubscriberExt;

  #[test]
  fn writes_and_filters_logs() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 100).expect("new store");
    store.push(LogEntry {
      id: "1".to_string(),
      ts: Utc::now(),
      level: "INFO".to_string(),
      target: "router".to_string(),
      message: "hello".to_string(),
      request_id: Some("r-1".to_string()),
      metadata: BTreeMap::new(),
    });
    store.push(LogEntry {
      id: "2".to_string(),
      ts: Utc::now(),
      level: "ERROR".to_string(),
      target: "router".to_string(),
      message: "boom".to_string(),
      request_id: Some("r-2".to_string()),
      metadata: BTreeMap::new(),
    });

    let only_error = store
      .query(LogQuery {
        limit: Some(10),
        level: Some("error".to_string()),
        request_id: None,
      })
      .expect("query");
    assert_eq!(only_error.len(), 1);
    assert_eq!(only_error[0].level, "ERROR");

    let only_r1 = store
      .query(LogQuery {
        limit: Some(10),
        level: None,
        request_id: Some("r-1".to_string()),
      })
      .expect("query");
    assert_eq!(only_r1.len(), 1);
    assert_eq!(only_r1[0].request_id.as_deref(), Some("r-1"));
  }

  #[test]
  fn captures_request_id_from_span() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 100).expect("new store");
    let layer = LogCaptureLayer::new(store.clone());

    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
      let span = tracing::info_span!("http.request", request_id = "req-42");
      let _guard = span.enter();
      tracing::info!(target: "router", "inside request");
    });

    let logs = store
      .query(LogQuery {
        limit: Some(10),
        level: Some("INFO".to_string()),
        request_id: Some("req-42".to_string()),
      })
      .expect("query");
    assert!(!logs.is_empty());
    assert!(logs.iter().any(|l| l.request_id.as_deref() == Some("req-42")));
  }

  #[test]
  fn stores_structured_metadata() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 100).expect("new store");
    let layer = LogCaptureLayer::new(store.clone());

    let subscriber = tracing_subscriber::registry().with(layer);
    tracing::subscriber::with_default(subscriber, || {
      tracing::info!(target: "router", model = "gpt-5-mini", provider = "github_copilot", "chat request");
    });

    let logs = store
      .query(LogQuery {
        limit: Some(10),
        level: Some("INFO".to_string()),
        request_id: None,
      })
      .expect("query");
    let row = logs.iter().find(|l| l.message == "chat request").expect("chat row");
    assert_eq!(row.metadata.get("model").map(String::as_str), Some("gpt-5-mini"));
    assert_eq!(row.metadata.get("provider").map(String::as_str), Some("github_copilot"));
  }
}
