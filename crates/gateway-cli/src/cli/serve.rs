use crate::config::Config;
use anyhow::{Context, Result};
use clap::Args;
use std::net::SocketAddr;
use std::path::PathBuf;

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

  if !args.allow_remote && !crate::server_runtime::is_loopback(&host) {
    anyhow::bail!("refusing to bind to non-loopback host '{host}' without --allow-remote (no client auth in v1)");
  }

  let db = crate::server_runtime::build_db(&cfg)?;
  let state = crate::server_runtime::build_state(&cfg, &db)?;
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
  crate::server_runtime::shutdown_db(db).await?;
  Ok(())
}
