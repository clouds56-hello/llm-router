use crate::config::Config;
use crate::db::{DbOptions, DbPaths, DbStore};
use anyhow::Result;
use std::sync::Arc;

pub fn build_db(cfg: &Config) -> Result<Option<Arc<DbStore>>> {
  if !cfg.db.enabled {
    return Ok(None);
  }
  let usage_db = cfg
    .db
    .usage_db_path
    .clone()
    .map(Ok)
    .unwrap_or_else(crate::config::paths::default_usage_db)?;
  let sessions_db = cfg
    .db
    .sessions_db_path
    .clone()
    .map(Ok)
    .unwrap_or_else(crate::config::paths::default_sessions_db)?;
  let requests_dir = cfg
    .db
    .requests_dir
    .clone()
    .map(Ok)
    .unwrap_or_else(crate::config::paths::default_requests_dir)?;
  Ok(Some(Arc::new(DbStore::spawn(DbOptions {
    paths: DbPaths {
      usage_db,
      sessions_db,
      requests_dir,
    },
    queue_capacity: cfg.db.write_queue_capacity,
    body_max_bytes: cfg.db.body_max_bytes,
  })?)))
}

pub fn build_state(
  cfg: &Config,
  db: &Option<Arc<DbStore>>,
) -> Result<llm_router::server::AppState> {
  llm_router::server::build_state(cfg, db.clone().map(|db| db as Arc<dyn llm_core::db::DbStore>))
}

pub async fn shutdown_db(db: Option<Arc<DbStore>>) -> Result<()> {
  if let Some(db) = db {
    db.shutdown().await?;
  }
  Ok(())
}

pub fn is_loopback(host: &str) -> bool {
  matches!(host, "127.0.0.1" | "::1" | "localhost")
    || host
      .parse::<std::net::IpAddr>()
      .map(|ip| ip.is_loopback())
      .unwrap_or(false)
}
