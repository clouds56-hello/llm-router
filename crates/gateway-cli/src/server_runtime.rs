use crate::config::Config;
use crate::db::{DbEventHandler, DbPaths};
use crate::progress::{ProgressEventHandler, ProgressLogEventHandler};
use anyhow::Result;
use llm_core::event::{EventBus, EventHandler, EventReceiver};
use std::io::IsTerminal;
use std::sync::Arc;

/// Build the event bus and its handlers. The DB event handler is included
/// when usage recording is enabled. A TTY progress handler is attached
/// automatically when stdout is a terminal.
pub fn build_event_bus(cfg: &Config) -> Result<(Arc<EventBus>, EventReceiver, Vec<Box<dyn EventHandler>>)> {
  let capacity = cfg.db.write_queue_capacity.max(256);
  let (bus, receiver) = EventBus::new(capacity);
  let mut handlers: Vec<Box<dyn EventHandler>> = Vec::new();

  if cfg.db.enabled {
    let paths = cfg.db.resolve_paths()?;
    let db_handler = DbEventHandler::new(DbPaths {
      usage_db: paths.usage_db,
      sessions_db: paths.sessions_db,
      requests_dir: paths.requests_dir,
    })?;
    handlers.push(Box::new(db_handler));
  }

  match crate::logging::resolve_logs_dir(&cfg.logging) {
    Ok(dir) => match ProgressLogEventHandler::new(&dir) {
      Ok(handler) => handlers.push(Box::new(handler)),
      Err(e) => tracing::warn!(path = %dir.display(), error = %e, "progress log disabled"),
    },
    Err(e) => tracing::warn!(error = %e, "progress log disabled"),
  }

  if std::io::stdout().is_terminal() {
    handlers.push(Box::new(ProgressEventHandler::new()));
  }

  Ok((Arc::new(bus), receiver, handlers))
}

pub fn build_state(cfg: &Config, events: Arc<EventBus>) -> Result<llm_router::api::AppState> {
  llm_router::api::build_state(cfg, events)
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
