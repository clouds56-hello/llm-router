use crate::config::Config;
use crate::db::archive::{ArchiveEventHandler, ArchiveRuntime};
use crate::db::{DbEventHandler, DbPaths};
use crate::progress::{ArchiveProgressEventHandler, ProgressEventHandler, ProgressLogEventHandler};
use anyhow::Result;
use axum::Router;
use llm_auth::AuthStore;
use llm_config::RouteMode;
use llm_core::account::AccountConfig;
use llm_core::event::{EventBus, EventHandler, EventReceiver};
use std::future::Future;
use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

/// Build the event bus and its handlers. The DB event handler is included
/// when usage recording is enabled. A TTY progress handler is attached
/// automatically when stdout is a terminal.
pub fn build_event_bus(
  cfg: &Config,
) -> Result<(
  Arc<EventBus>,
  EventReceiver,
  Vec<Box<dyn EventHandler>>,
  Option<ArchiveRuntime>,
)> {
  let capacity = cfg.db.write_queue_capacity.max(256);
  let (bus, receiver) = EventBus::new(capacity);
  let mut handlers: Vec<Box<dyn EventHandler>> = Vec::new();
  let mut archive_handlers: Vec<Box<dyn ArchiveEventHandler>> = Vec::new();
  let tty_progress = std::io::stdout().is_terminal();

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

  if tty_progress {
    handlers.push(Box::new(ProgressEventHandler::new()));
    archive_handlers.push(Box::new(ArchiveProgressEventHandler::new()));
  }

  let archive_runtime = if cfg.db.enabled {
    let paths = cfg.db.resolve_paths()?;
    crate::db::archive::start_request_archive_worker(
      paths.requests_dir,
      cfg.db.archive_extension.as_deref(),
      archive_handlers,
    )
  } else {
    None
  };

  Ok((Arc::new(bus), receiver, handlers, archive_runtime))
}

/// Load accounts from `auth.yaml`, falling back to the legacy
/// `[[accounts]]` block in `config.toml` (with a deprecation warning).
///
/// `config_path` is the effective path of `config.toml` so the legacy
/// migration can find it; pass `None` to disable the fallback.
pub fn load_accounts(config_path: Option<&Path>) -> Result<Vec<AccountConfig>> {
  let store = AuthStore::load(None, config_path)?;
  Ok(store.accounts)
}

pub fn build_state(
  cfg: &Config,
  accounts: &[AccountConfig],
  events: Arc<EventBus>,
) -> Result<llm_router::api::AppState> {
  llm_router::api::build_state(cfg, accounts, events)
}

pub fn build_state_for_route_mode(
  cfg: &Config,
  accounts: &[AccountConfig],
  events: Arc<EventBus>,
  route_mode: RouteMode,
) -> Result<llm_router::api::AppState> {
  let mut cfg = cfg.clone();
  cfg.server.route_mode = route_mode;
  build_state(&cfg, accounts, events)
}

pub fn state_with_route_mode(
  state: &llm_router::api::AppState,
  route_mode: RouteMode,
  cfg: &Config,
) -> llm_router::api::AppState {
  let mut state = state.clone();
  state.route = Arc::new(llm_router::api::routing::RouteResolver::new(
    route_mode,
    &cfg.model_families,
  ));
  state
}

pub fn resolve_bind_addr(host: &str, port: u16, allow_remote: bool) -> Result<SocketAddr> {
  ensure_bind_host(host, allow_remote)?;
  Ok(format!("{host}:{port}").parse()?)
}

pub async fn serve_http<F>(app: Router, addr: SocketAddr, shutdown: F) -> Result<()>
where
  F: Future<Output = ()> + Send + 'static,
{
  let listener = tokio::net::TcpListener::bind(addr).await?;
  tracing::info!(%addr, "llm-router listening");
  axum::serve(listener, app).with_graceful_shutdown(shutdown).await?;
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
