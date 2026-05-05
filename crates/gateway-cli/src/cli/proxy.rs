use crate::config::{Config, ProxyConfig};
use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use llm_config::RouteMode;
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

#[derive(Args, Debug)]
pub struct ProxyArgs {
  /// Route intercepted requests directly to the original upstream with the
  /// client's own credentials.
  #[arg(long, global = true)]
  pub passthrough: bool,
  #[command(subcommand)]
  pub cmd: Option<ProxyCmd>,
}

#[derive(Subcommand, Debug)]
pub enum ProxyCmd {
  /// Run the local MITM forward proxy
  Start(StartArgs),
  /// Print shell environment exports for proxy + CA trust
  Env(EnvArgs),
  /// Enter a shell with proxy + CA env vars set
  Shell(ShellArgs),
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
pub struct ShellArgs {
  #[arg(long)]
  pub shell: Option<PathBuf>,
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
  Bash,
  Zsh,
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
  let ProxyArgs { passthrough, cmd } = args;
  match cmd.unwrap_or(ProxyCmd::Start(StartArgs::default())) {
    ProxyCmd::Start(args) => start(cfg_path, args, passthrough).await,
    ProxyCmd::Env(args) => env(cfg_path, args).await,
    ProxyCmd::Shell(args) => shell(cfg_path, args).await,
    ProxyCmd::Ca(args) => ca(cfg_path, args).await,
  }
}

async fn start(cfg_path: Option<PathBuf>, args: StartArgs, passthrough: bool) -> Result<()> {
  let (mut cfg, _) = Config::load(cfg_path.as_deref())?;
  if args.no_proxy {
    cfg.proxy = ProxyConfig::default();
  }
  if passthrough {
    cfg.server.route_mode = RouteMode::Passthrough;
  }

  let host = args.host.unwrap_or_else(|| cfg.proxy_mode.host.clone());
  let port = args.port.unwrap_or(cfg.proxy_mode.port);
  let ca_dir = args
    .ca_dir
    .clone()
    .map(Ok)
    .unwrap_or_else(|| cfg.proxy_mode.resolved_ca_dir())?;

  crate::server_runtime::ensure_bind_host(&host, args.allow_remote)?;

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
  if passthrough {
    println!("Route mode: passthrough");
  }
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
  let env = resolved_proxy_env(cfg_path.as_deref())?;
  match args.shell {
    Shell::Sh | Shell::Bash | Shell::Zsh => print_sh(&env),
    Shell::Fish => print_fish(&env),
    Shell::Pwsh => print_pwsh(&env),
  }
  Ok(())
}

async fn shell(cfg_path: Option<PathBuf>, args: ShellArgs) -> Result<()> {
  let env = resolved_proxy_env(cfg_path.as_deref())?;
  let shell = detect_shell(args.shell.as_deref())?;
  println!("Entering proxy shell: {}", shell.path.display());
  println!("HTTPS_PROXY={}", env.get("HTTPS_PROXY").unwrap_or(""));
  println!("SSL_CERT_FILE={}", env.get("SSL_CERT_FILE").unwrap_or(""));
  println!("Type 'exit' to leave this shell.");
  let mut cmd = Command::new(&shell.path);
  cmd.envs(env.vars.iter().map(|(k, v)| (k.as_str(), v.as_str())));
  if let Some(arg0) = shell.arg0 {
    cmd.arg0(arg0);
  }
  let status = cmd
    .status()
    .with_context(|| format!("launch shell {}", shell.path.display()))?;
  if !status.success() {
    anyhow::bail!("shell exited with status {status}");
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
      println!("bundle: {}", ca.ensure_bundle()?.display());
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

fn print_sh(env: &ProxyEnv) {
  for (key, value) in &env.vars {
    println!("export {key}={value}");
  }
}

fn print_fish(env: &ProxyEnv) {
  for (key, value) in &env.vars {
    println!("set -gx {key} {value}");
  }
}

fn print_pwsh(env: &ProxyEnv) {
  for (key, value) in &env.vars {
    println!("$Env:{key} = '{value}'");
  }
}

fn resolved_proxy_env(cfg_path: Option<&Path>) -> Result<ProxyEnv> {
  let (cfg, _) = Config::load(cfg_path)?;
  let ca_dir = cfg.proxy_mode.resolved_ca_dir()?;
  let ca = llm_router::proxy::load_or_generate_ca(&ca_dir, false)?;
  let proxy_url = format!("http://{}:{}", cfg.proxy_mode.host, cfg.proxy_mode.port);
  let cert = ca.cert_path().display().to_string();
  let bundle = ca.ensure_bundle()?.display().to_string();
  Ok(ProxyEnv {
    vars: vec![
      ("HTTPS_PROXY".into(), proxy_url.clone()),
      ("HTTP_PROXY".into(), proxy_url),
      ("NO_PROXY".into(), "localhost,127.0.0.1,::1".into()),
      ("SSL_CERT_FILE".into(), bundle.clone()),
      ("NODE_EXTRA_CA_CERTS".into(), cert),
      ("REQUESTS_CA_BUNDLE".into(), bundle.clone()),
      ("CURL_CA_BUNDLE".into(), bundle.clone()),
      ("GIT_SSL_CAINFO".into(), bundle),
    ],
  })
}

#[derive(Debug)]
struct ProxyEnv {
  vars: Vec<(String, String)>,
}

impl ProxyEnv {
  fn get(&self, key: &str) -> Option<&str> {
    self.vars.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
  }
}

#[derive(Debug)]
struct ShellExec {
  path: PathBuf,
  arg0: Option<String>,
}

fn detect_shell(explicit: Option<&Path>) -> Result<ShellExec> {
  if let Some(path) = explicit {
    return Ok(ShellExec {
      path: path.to_path_buf(),
      arg0: shell_arg0(path),
    });
  }

  if let Some(shell) = std::env::var_os("SHELL") {
    let path = PathBuf::from(shell);
    return Ok(ShellExec {
      arg0: shell_arg0(&path),
      path,
    });
  }

  if let Some(comspec) = std::env::var_os("COMSPEC") {
    let path = PathBuf::from(comspec);
    return Ok(ShellExec {
      arg0: shell_arg0(&path),
      path,
    });
  }

  let path = PathBuf::from("/bin/sh");
  Ok(ShellExec {
    arg0: shell_arg0(&path),
    path,
  })
}

fn shell_arg0(path: &Path) -> Option<String> {
  path.file_name().and_then(|name| name.to_str()).map(|s| s.to_string())
}
