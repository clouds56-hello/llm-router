use crate::config::Config;
use crate::server;
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
}

pub async fn run(cfg_path: Option<PathBuf>, args: ServeArgs) -> Result<()> {
    let (cfg, _) = Config::load(cfg_path.as_deref())?;

    let host = args.host.unwrap_or_else(|| cfg.server.host.clone());
    let port = args.port.unwrap_or(cfg.server.port);

    if !args.allow_remote && !is_loopback(&host) {
        anyhow::bail!(
            "refusing to bind to non-loopback host '{host}' without --allow-remote (no client auth in v1)"
        );
    }

    let state = server::build_state(&cfg)?;
    let n = state.pool.len();
    let app = server::router(state);

    let addr: SocketAddr = format!("{host}:{port}")
        .parse()
        .with_context(|| format!("parse bind addr {host}:{port}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind {addr}"))?;

    tracing::info!(%addr, accounts = n, "llm-router listening");
    println!("llm-router listening on http://{addr}  (accounts: {n})");

    axum::serve(listener, app).await?;
    Ok(())
}

fn is_loopback(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "::1" | "localhost")
        || host.parse::<std::net::IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}
