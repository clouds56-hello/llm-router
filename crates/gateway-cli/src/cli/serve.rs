use crate::config::Config;
use crate::db::{DbOptions, DbPaths, DbStore};
use anyhow::{Context, Result};
use clap::Args;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Args, Debug)]
pub struct ServeArgs {
  #[arg(long)]
  pub host: Option<String>,
  #[arg(long)]
  pub port: Option<u16>,
  /// Allow binding to non-loopback addresses (insecure: there is no client auth in v1).
  #[arg(long)]
  pub allow_remote: bool,
  /// Skip outbound proxy for this run.
  #[arg(long)]
  pub no_proxy: bool,
}

pub async fn run(cfg_path: Option<PathBuf>, args: ServeArgs) -> Result<()> {
  let (mut cfg, _) = Config::load(cfg_path.as_deref())?;
  if args.no_proxy {
    cfg.proxy = crate::config::ProxyConfig::default();
  }

  let host = args.host.unwrap_or_else(|| cfg.server.host.clone());
  let port = args.port.unwrap_or(cfg.server.port);

  if !args.allow_remote && !is_loopback(&host) {
    anyhow::bail!("refusing to bind to non-loopback host '{host}' without --allow-remote (no client auth in v1)");
  }

  let db = build_db(&cfg)?;
  let core_cfg: llm_core::config::Config = cfg.clone().into();
  let state = llm_router::server::build_state(&core_cfg, db.clone().map(|db| db as Arc<dyn llm_core::db::DbStore>))?;
  let n = state.pool.len();
  let app = llm_router::server::router(state);

  let addr: SocketAddr = format!("{host}:{port}")
    .parse()
    .with_context(|| format!("parse bind addr {host}:{port}"))?;
  let listener = tokio::net::TcpListener::bind(addr)
    .await
    .with_context(|| format!("bind {addr}"))?;

  tracing::info!(%addr, accounts = n, "llm-router listening");

  axum::serve(listener, app)
    .with_graceful_shutdown(async {
      let _ = tokio::signal::ctrl_c().await;
    })
    .await?;
  if let Some(db) = db {
    db.shutdown().await?;
  }
  Ok(())
}

fn build_db(cfg: &Config) -> Result<Option<Arc<DbStore>>> {
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

fn is_loopback(host: &str) -> bool {
  matches!(host, "127.0.0.1" | "::1" | "localhost")
    || host
      .parse::<std::net::IpAddr>()
      .map(|ip| ip.is_loopback())
      .unwrap_or(false)
}
