use crate::config::Config;
use crate::db::{DbOptions, DbPaths, DbStore};
use anyhow::Result;
use std::sync::Arc;

pub fn build_db(cfg: &Config) -> Result<Option<Arc<DbStore>>> {
  if !cfg.db.enabled {
    return Ok(None);
  }
  let paths = cfg.db.resolve_paths()?;
  Ok(Some(Arc::new(DbStore::spawn(DbOptions {
    paths: DbPaths {
      usage_db: paths.usage_db,
      sessions_db: paths.sessions_db,
      requests_dir: paths.requests_dir,
    },
    queue_capacity: cfg.db.write_queue_capacity,
    body_max_bytes: cfg.db.body_max_bytes,
  })?)))
}

pub fn build_state(cfg: &Config, db: &Option<Arc<DbStore>>) -> Result<llm_router::server::AppState> {
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

pub fn ensure_bind_host(host: &str, allow_remote: bool) -> Result<()> {
  if !allow_remote && !is_loopback(host) {
    anyhow::bail!("refusing to bind to non-loopback host '{host}' without --allow-remote (no client auth in v1)");
  }
  Ok(())
}
