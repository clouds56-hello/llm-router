use crate::config::{Config, ProxyConfig};
use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct ProxyArgs {
  #[command(subcommand)]
  pub cmd: Option<ProxyCmd>,
}

#[derive(Subcommand, Debug)]
pub enum ProxyCmd {
  /// Run the local MITM forward proxy
  Start(StartArgs),
  /// Print shell environment exports for proxy + CA trust
  Env(EnvArgs),
  /// Inspect or regenerate the local proxy CA
  Ca(CaArgs),
}

#[derive(Args, Debug)]
pub struct StartArgs {
  #[arg(long)]
  pub host: Option<String>,
  #[arg(long)]
  pub port: Option<u16>,
  #[arg(long)]
  pub ca_dir: Option<PathBuf>,
  /// Allow binding to non-loopback addresses (insecure: there is no client auth in v1).
  #[arg(long)]
  pub allow_remote: bool,
  /// Skip outbound proxy for this run.
  #[arg(long)]
  pub no_proxy: bool,
}

#[derive(Args, Debug)]
pub struct EnvArgs {
  #[arg(long, value_enum, default_value_t = Shell::Sh)]
  pub shell: Shell,
}

#[derive(Args, Debug)]
pub struct CaArgs {
  #[command(subcommand)]
  pub cmd: CaCmd,
}

#[derive(Subcommand, Debug)]
pub enum CaCmd {
  /// Print the CA cert path
  Path,
  /// Print CA details
  Show,
  /// Regenerate the CA and overwrite existing files
  Regenerate,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, ValueEnum)]
pub enum Shell {
  Sh,
  Fish,
  Pwsh,
}

impl Default for StartArgs {
  fn default() -> Self {
    Self {
      host: None,
      port: None,
      ca_dir: None,
      allow_remote: false,
      no_proxy: false,
    }
  }
}

pub async fn run(cfg_path: Option<PathBuf>, args: ProxyArgs) -> Result<()> {
  match args.cmd.unwrap_or(ProxyCmd::Start(StartArgs::default())) {
    ProxyCmd::Start(args) => start(cfg_path, args).await,
    ProxyCmd::Env(args) => env(cfg_path, args).await,
    ProxyCmd::Ca(args) => ca(cfg_path, args).await,
  }
}

async fn start(cfg_path: Option<PathBuf>, args: StartArgs) -> Result<()> {
  let (mut cfg, _) = Config::load(cfg_path.as_deref())?;
  if args.no_proxy {
    cfg.proxy = ProxyConfig::default();
  }

  let host = args.host.unwrap_or_else(|| cfg.proxy_mode.host.clone());
  let port = args.port.unwrap_or(cfg.proxy_mode.port);
  let ca_dir = args
    .ca_dir
    .clone()
    .map(Ok)
    .unwrap_or_else(|| cfg.proxy_mode.resolved_ca_dir())?;

  if !args.allow_remote && !crate::server_runtime::is_loopback(&host) {
    anyhow::bail!("refusing to bind to non-loopback host '{host}' without --allow-remote (no client auth in v1)");
  }

  let db = crate::server_runtime::build_db(&cfg)?;
  let state = crate::server_runtime::build_state(&cfg, &db)?;
  let n = state.pool.len();
  let addr: SocketAddr = format!("{host}:{port}")
    .parse()
    .with_context(|| format!("parse bind addr {host}:{port}"))?;

  let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, false)?;
  let ca_fingerprint = ca.fingerprint_sha256();
  println!("llm-router proxy listening on http://{addr}");
  println!("CA: {} (sha256:{ca_fingerprint})", ca.cert_path().display());
  println!("Trust this CA, then run: eval \"$(llm-gateway proxy env)\"");
  println!("Accounts: {n}");

  let options = llm_router::proxy::ProxyOptions {
    addr,
    ca_dir,
    intercept_hosts: cfg.proxy_mode.intercept_hosts.clone(),
    passthrough_hosts: cfg.proxy_mode.passthrough_hosts.clone(),
  };

  let result = llm_router::proxy::serve(state, options).await;
  crate::server_runtime::shutdown_db(db).await?;
  result
}

async fn env(cfg_path: Option<PathBuf>, args: EnvArgs) -> Result<()> {
  let (cfg, _) = Config::load(cfg_path.as_deref())?;
  let ca_dir = cfg.proxy_mode.resolved_ca_dir()?;
  let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, false)?;
  let proxy_url = format!("http://{}:{}", cfg.proxy_mode.host, cfg.proxy_mode.port);
  let cert = ca.cert_path().display().to_string();
  match args.shell {
    Shell::Sh => print_sh(&proxy_url, &cert),
    Shell::Fish => print_fish(&proxy_url, &cert),
    Shell::Pwsh => print_pwsh(&proxy_url, &cert),
  }
  Ok(())
}

async fn ca(cfg_path: Option<PathBuf>, args: CaArgs) -> Result<()> {
  let (cfg, _) = Config::load(cfg_path.as_deref())?;
  let ca_dir = cfg.proxy_mode.resolved_ca_dir()?;
  match args.cmd {
    CaCmd::Path => {
      let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, false)?;
      println!("{}", ca.cert_path().display());
    }
    CaCmd::Show => {
      let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, false)?;
      println!("cert: {}", ca.cert_path().display());
      println!("key: {}", ca.key_path().display());
      println!("sha256: {}", ca.fingerprint_sha256());
    }
    CaCmd::Regenerate => {
      let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, true)?;
      println!("regenerated CA at {}", ca.cert_path().display());
      println!("sha256: {}", ca.fingerprint_sha256());
    }
  }
  Ok(())
}

fn print_sh(proxy_url: &str, cert: &str) {
  println!("export HTTPS_PROXY={proxy_url}");
  println!("export HTTP_PROXY={proxy_url}");
  println!("export NO_PROXY=localhost,127.0.0.1,::1");
  println!("export SSL_CERT_FILE={cert}");
  println!("export NODE_EXTRA_CA_CERTS={cert}");
  println!("export REQUESTS_CA_BUNDLE={cert}");
  println!("export CURL_CA_BUNDLE={cert}");
  println!("export GIT_SSL_CAINFO={cert}");
}

fn print_fish(proxy_url: &str, cert: &str) {
  println!("set -gx HTTPS_PROXY {proxy_url}");
  println!("set -gx HTTP_PROXY {proxy_url}");
  println!("set -gx NO_PROXY localhost,127.0.0.1,::1");
  println!("set -gx SSL_CERT_FILE {cert}");
  println!("set -gx NODE_EXTRA_CA_CERTS {cert}");
  println!("set -gx REQUESTS_CA_BUNDLE {cert}");
  println!("set -gx CURL_CA_BUNDLE {cert}");
  println!("set -gx GIT_SSL_CAINFO {cert}");
}

fn print_pwsh(proxy_url: &str, cert: &str) {
  println!("$Env:HTTPS_PROXY = '{proxy_url}'");
  println!("$Env:HTTP_PROXY = '{proxy_url}'");
  println!("$Env:NO_PROXY = 'localhost,127.0.0.1,::1'");
  println!("$Env:SSL_CERT_FILE = '{cert}'");
  println!("$Env:NODE_EXTRA_CA_CERTS = '{cert}'");
  println!("$Env:REQUESTS_CA_BUNDLE = '{cert}'");
  println!("$Env:CURL_CA_BUNDLE = '{cert}'");
  println!("$Env:GIT_SSL_CAINFO = '{cert}'");
}
