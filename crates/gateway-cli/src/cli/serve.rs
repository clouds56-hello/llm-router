use crate::cli::config_cmd::RouteModeArg;
use crate::config::Config;
use anyhow::{Context, Result};
use clap::Args;
use llm_config::RouteMode;
use std::path::PathBuf;
use tokio::sync::watch;

#[derive(Args, Debug)]
pub struct ServeArgs {
  #[arg(long)]
  pub host: Option<String>,
  #[arg(long)]
  pub port: Option<u16>,
  /// Also run the local MITM proxy in the same process.
  #[arg(long)]
  pub with_proxy: bool,
  /// Override the proxy listener's default route mode when `--with-proxy` is enabled.
  #[arg(long, value_enum, requires = "with_proxy")]
  pub proxy_route_mode: Option<RouteModeArg>,
  /// Allow binding to non-loopback addresses (insecure: there is no client auth in v1).
  #[arg(long)]
  pub allow_remote: bool,
  /// Skip outbound proxy for this run.
  #[arg(long)]
  pub no_proxy: bool,
}

pub async fn run(cfg_path: Option<PathBuf>, args: ServeArgs) -> Result<()> {
  let (mut cfg, resolved_cfg_path) = Config::load(cfg_path.as_deref())?;
  if args.no_proxy {
    cfg.proxy = crate::config::ProxyConfig::default();
  }
  let accounts = crate::server_runtime::load_accounts(Some(&resolved_cfg_path))?;

  let host = args.host.unwrap_or_else(|| cfg.server.host.clone());
  let port = args.port.unwrap_or(cfg.server.port);
  let addr = crate::server_runtime::resolve_bind_addr(&host, port, args.allow_remote)
    .with_context(|| format!("parse bind addr {host}:{port}"))?;

  let (events, receiver, handlers, archive_runtime) = crate::server_runtime::build_event_bus(&cfg)?;
  let _event_thread = llm_core::event::spawn_event_loop(receiver, handlers);
  let server_mode = cfg.server.route_mode;
  let proxy_mode = args
    .proxy_route_mode
    .map(Into::into)
    .unwrap_or(cfg.proxy_mode.route_mode);
  let shared_mode = shared_route_mode(server_mode, proxy_mode, args.with_proxy);
  let shared_state = crate::server_runtime::build_state_for_route_mode(&cfg, &accounts, events.clone(), shared_mode)?;
  let n = shared_state.pool.len();
  let app_state = crate::server_runtime::state_with_route_mode(&shared_state, server_mode, &cfg);
  let app = llm_router::api::router(app_state);

  tracing::info!(%addr, accounts = n, route_mode = route_mode_name(server_mode), "llm-router listening");

  let result = if args.with_proxy {
    let proxy_host = cfg.proxy_mode.host.clone();
    let proxy_port = cfg.proxy_mode.port;
    let proxy_addr = crate::server_runtime::resolve_bind_addr(&proxy_host, proxy_port, args.allow_remote)
      .with_context(|| format!("parse bind addr {proxy_host}:{proxy_port}"))?;
    let ca_dir = cfg.proxy_mode.resolved_ca_dir()?;
    let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, false)?;
    let ca_fingerprint = ca.fingerprint_sha256();
    println!("llm-router proxy listening on http://{proxy_addr}");
    println!("CA: {} (sha256:{ca_fingerprint})", ca.cert_path().display());
    println!("Proxy route mode: {}", route_mode_name(proxy_mode));

    let proxy_state = crate::server_runtime::state_with_route_mode(&shared_state, proxy_mode, &cfg);
    let proxy_options = llm_router::proxy::ProxyOptions {
      addr: proxy_addr,
      ca_dir,
      intercept_hosts: cfg.proxy_mode.intercept_hosts.clone(),
      passthrough_hosts: cfg.proxy_mode.passthrough_hosts.clone(),
    };
    let shutdown = shutdown_channel();
    tokio::try_join!(
      crate::server_runtime::serve_http(app, addr, wait_for_shutdown(shutdown.clone())),
      llm_router::proxy::serve(proxy_state, proxy_options, wait_for_shutdown(shutdown)),
    )
    .map(|_| ())
  } else {
    crate::server_runtime::serve_http(app, addr, async {
      let _ = tokio::signal::ctrl_c().await;
    })
    .await
  };

  if let Some(archive_runtime) = archive_runtime {
    archive_runtime.shutdown().await;
  }
  events.shutdown().await;
  result
}

fn shared_route_mode(server_mode: RouteMode, proxy_mode: RouteMode, with_proxy: bool) -> RouteMode {
  if !with_proxy || server_mode != RouteMode::Passthrough {
    server_mode
  } else {
    proxy_mode
  }
}

fn shutdown_channel() -> watch::Receiver<bool> {
  let (tx, rx) = watch::channel(false);
  tokio::spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    let _ = tx.send(true);
  });
  rx
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
  if *shutdown.borrow() {
    return;
  }
  let _ = shutdown.changed().await;
}

fn route_mode_name(mode: RouteMode) -> &'static str {
  match mode {
    RouteMode::Passthrough => "passthrough",
    RouteMode::Exact => "exact",
    RouteMode::Route => "route",
    RouteMode::Fuzzy => "fuzzy",
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn shared_mode_prefers_non_passthrough_listener_when_needed() {
    assert_eq!(
      shared_route_mode(RouteMode::Passthrough, RouteMode::Exact, true),
      RouteMode::Exact
    );
    assert_eq!(
      shared_route_mode(RouteMode::Route, RouteMode::Passthrough, true),
      RouteMode::Route
    );
    assert_eq!(
      shared_route_mode(RouteMode::Passthrough, RouteMode::Passthrough, true),
      RouteMode::Passthrough
    );
  }
}
