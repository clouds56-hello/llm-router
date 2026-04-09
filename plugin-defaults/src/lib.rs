use std::sync::Arc;

use chrono::{Duration, Utc};
use parking_lot::Mutex;
use plugin_api::{EventRecord, Plugin};
use router_core::RouterError;
use rusqlite::{params, Connection};
use serde::Serialize;
use serde_json::json;
use tokio::sync::RwLock;
use tracing::info;

#[derive(Clone)]
pub struct SqliteStore {
  conn: Arc<Mutex<Connection>>,
  pub max_rows: u64,
  pub max_age_seconds: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct RecentEvent {
  pub id: i64,
  pub request_id: String,
  pub ts: String,
  pub event_type: String,
  pub provider_attempts: String,
  pub route_name: Option<String>,
  pub status_code: Option<i64>,
  pub latency_ms: Option<i64>,
  pub message: Option<String>,
}

#[derive(Debug, Serialize, Clone)]
pub struct StatsSnapshot {
  pub total_requests: i64,
  pub errors: i64,
  pub avg_latency_ms: f64,
}

impl SqliteStore {
  pub fn new(path: &str, max_rows: u64, max_age_seconds: u64) -> Result<Self, RouterError> {
    let conn = Connection::open(path).map_err(|e| RouterError::Internal(format!("open sqlite failed: {}", e)))?;
    conn
      .execute_batch(
        r#"
            CREATE TABLE IF NOT EXISTS events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                request_id TEXT NOT NULL,
                ts TEXT NOT NULL,
                event_type TEXT NOT NULL,
                provider_attempts TEXT NOT NULL,
                route_name TEXT,
                status_code INTEGER,
                latency_ms INTEGER,
                message TEXT
            );
            CREATE INDEX IF NOT EXISTS idx_events_ts ON events(ts);
            CREATE INDEX IF NOT EXISTS idx_events_request_id ON events(request_id);
            "#,
      )
      .map_err(|e| RouterError::Internal(format!("sqlite init failed: {}", e)))?;

    Ok(Self {
      conn: Arc::new(Mutex::new(conn)),
      max_rows,
      max_age_seconds,
    })
  }

  pub fn insert_event(
    &self,
    request_id: &str,
    event_type: &str,
    provider_attempts: &str,
    route_name: Option<&str>,
    status_code: Option<i64>,
    latency_ms: Option<i64>,
    message: Option<&str>,
  ) -> Result<(), RouterError> {
    let conn = self.conn.lock();
    conn.execute(
            "INSERT INTO events (request_id, ts, event_type, provider_attempts, route_name, status_code, latency_ms, message) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                request_id,
                Utc::now().to_rfc3339(),
                event_type,
                provider_attempts,
                route_name,
                status_code,
                latency_ms,
                message
            ],
        )
        .map_err(|e| RouterError::Internal(format!("insert event failed: {}", e)))?;

    Ok(())
  }

  pub fn prune(&self) -> Result<(), RouterError> {
    let conn = self.conn.lock();

    let cutoff = (Utc::now() - Duration::seconds(self.max_age_seconds as i64)).to_rfc3339();
    conn
      .execute("DELETE FROM events WHERE ts < ?1", params![cutoff])
      .map_err(|e| RouterError::Internal(format!("prune by age failed: {}", e)))?;

    let total: i64 = conn
      .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
      .map_err(|e| RouterError::Internal(format!("count events failed: {}", e)))?;
    let overflow = total.saturating_sub(self.max_rows as i64);
    if overflow > 0 {
      conn
        .execute(
          "DELETE FROM events WHERE id IN (SELECT id FROM events ORDER BY id ASC LIMIT ?1)",
          params![overflow],
        )
        .map_err(|e| RouterError::Internal(format!("prune by rows failed: {}", e)))?;
    }
    Ok(())
  }

  pub fn recent_events(&self, limit: usize) -> Result<Vec<RecentEvent>, RouterError> {
    let conn = self.conn.lock();
    let mut stmt = conn
            .prepare(
                "SELECT id, request_id, ts, event_type, provider_attempts, route_name, status_code, latency_ms, message FROM events ORDER BY id DESC LIMIT ?1",
            )
            .map_err(|e| RouterError::Internal(format!("prepare recent query failed: {}", e)))?;

    let rows = stmt
      .query_map(params![limit as i64], |row| {
        Ok(RecentEvent {
          id: row.get(0)?,
          request_id: row.get(1)?,
          ts: row.get(2)?,
          event_type: row.get(3)?,
          provider_attempts: row.get(4)?,
          route_name: row.get(5)?,
          status_code: row.get(6)?,
          latency_ms: row.get(7)?,
          message: row.get(8)?,
        })
      })
      .map_err(|e| RouterError::Internal(format!("query recent events failed: {}", e)))?;

    let mut out = Vec::new();
    for row in rows {
      out.push(row.map_err(|e| RouterError::Internal(format!("decode event row failed: {}", e)))?);
    }
    Ok(out)
  }

  pub fn stats(&self) -> Result<StatsSnapshot, RouterError> {
    let conn = self.conn.lock();
    let total_requests: i64 = conn
      .query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'request_end'",
        [],
        |r| r.get(0),
      )
      .map_err(|e| RouterError::Internal(format!("stats total failed: {}", e)))?;

    let errors: i64 = conn
      .query_row(
        "SELECT COUNT(*) FROM events WHERE event_type = 'request_error'",
        [],
        |r| r.get(0),
      )
      .map_err(|e| RouterError::Internal(format!("stats errors failed: {}", e)))?;

    let avg_latency_ms: f64 = conn
      .query_row(
        "SELECT COALESCE(AVG(latency_ms), 0) FROM events WHERE event_type = 'request_end'",
        [],
        |r| r.get(0),
      )
      .map_err(|e| RouterError::Internal(format!("stats avg failed: {}", e)))?;

    Ok(StatsSnapshot {
      total_requests,
      errors,
      avg_latency_ms,
    })
  }
}

pub struct JsonLoggerPlugin;

#[async_trait::async_trait]
impl Plugin for JsonLoggerPlugin {
  fn name(&self) -> &'static str {
    "json_logger"
  }

  async fn on_event(&self, event: EventRecord) {
    info!("{}", serialize_event(&event));
  }
}

pub struct MetricsPlugin {
  counters: Arc<RwLock<MetricsCounters>>,
}

#[derive(Default)]
struct MetricsCounters {
  requests: u64,
  errors: u64,
  stream_chunks: u64,
}

impl MetricsPlugin {
  pub fn new() -> Self {
    Self {
      counters: Arc::new(RwLock::new(MetricsCounters::default())),
    }
  }
}

#[async_trait::async_trait]
impl Plugin for MetricsPlugin {
  fn name(&self) -> &'static str {
    "metrics"
  }

  async fn on_event(&self, event: EventRecord) {
    let mut c = self.counters.write().await;
    match event {
      EventRecord::RequestStart { .. } => c.requests += 1,
      EventRecord::StreamChunk { .. } => c.stream_chunks += 1,
      EventRecord::RequestError { .. } => c.errors += 1,
      EventRecord::RequestEnd { .. } => {}
    }
  }
}

pub struct SqliteSinkPlugin {
  store: SqliteStore,
}

impl SqliteSinkPlugin {
  pub fn new(store: SqliteStore) -> Self {
    Self { store }
  }

  pub fn store(&self) -> SqliteStore {
    self.store.clone()
  }
}

#[async_trait::async_trait]
impl Plugin for SqliteSinkPlugin {
  fn name(&self) -> &'static str {
    "sqlite_sink"
  }

  async fn on_event(&self, event: EventRecord) {
    let res = match event {
      EventRecord::RequestStart { ctx, .. } => self.store.insert_event(
        &ctx.request_id,
        "request_start",
        &ctx.provider_attempts.join(","),
        ctx.route_name.as_deref(),
        None,
        None,
        None,
      ),
      EventRecord::StreamChunk { ctx, chunk } => self.store.insert_event(
        &ctx.request_id,
        "stream_chunk",
        &ctx.provider_attempts.join(","),
        ctx.route_name.as_deref(),
        None,
        None,
        Some(&chunk.choices.first().map(|c| c.delta.to_string()).unwrap_or_default()),
      ),
      EventRecord::RequestEnd {
        ctx,
        status_code,
        latency_ms,
      } => self.store.insert_event(
        &ctx.request_id,
        "request_end",
        &ctx.provider_attempts.join(","),
        ctx.route_name.as_deref(),
        Some(status_code as i64),
        Some(latency_ms as i64),
        None,
      ),
      EventRecord::RequestError { ctx, error, latency_ms } => self.store.insert_event(
        &ctx.request_id,
        "request_error",
        &ctx.provider_attempts.join(","),
        ctx.route_name.as_deref(),
        Some(error.status_code() as i64),
        Some(latency_ms as i64),
        Some(&error.to_string()),
      ),
    };

    if let Err(err) = res {
      tracing::warn!("sqlite sink event write failed: {}", err);
    }
  }
}

fn serialize_event(event: &EventRecord) -> String {
  match event {
    EventRecord::RequestStart { ctx, req } => json!({
        "event": "request_start",
        "request_id": ctx.request_id,
        "model": req.model,
        "route": ctx.route_name,
        "provider_attempts": ctx.provider_attempts,
        "timestamp": Utc::now().to_rfc3339(),
    })
    .to_string(),
    EventRecord::StreamChunk { ctx, chunk } => json!({
        "event": "stream_chunk",
        "request_id": ctx.request_id,
        "model": chunk.model,
        "timestamp": Utc::now().to_rfc3339(),
    })
    .to_string(),
    EventRecord::RequestEnd {
      ctx,
      status_code,
      latency_ms,
    } => json!({
        "event": "request_end",
        "request_id": ctx.request_id,
        "status_code": status_code,
        "latency_ms": latency_ms,
        "provider_attempts": ctx.provider_attempts,
        "timestamp": Utc::now().to_rfc3339(),
    })
    .to_string(),
    EventRecord::RequestError { ctx, error, latency_ms } => json!({
        "event": "request_error",
        "request_id": ctx.request_id,
        "status_code": error.status_code(),
        "error": error.to_string(),
        "latency_ms": latency_ms,
        "provider_attempts": ctx.provider_attempts,
        "timestamp": Utc::now().to_rfc3339(),
    })
    .to_string(),
  }
}
