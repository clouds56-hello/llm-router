use clap::{Parser, Subcommand};
use std::path::PathBuf;

use crate::config::Config;
use crate::logging::{self, RunMode};

mod account;
mod config_cmd;
mod error;
mod headers;
mod import;
mod login;
mod migration;
mod onboarding;
mod proxy;
mod serve;
mod smoke;
mod update;
mod usage;

pub use error::{Error, Result};

#[derive(Parser, Debug)]
#[command(name = "tokn-router", about = "GitHub Copilot -> OpenAI-compatible API")]
pub struct Cli {
  /// Path to config file (default: ~/.config/tokn-router/config.toml)
  #[arg(long, global = true)]
  pub config: Option<PathBuf>,

  #[command(subcommand)]
  pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
  /// Manage stored accounts (add / login / import / list / switch / refresh / status / show / remove)
  #[command(subcommand)]
  Account(account::AccountCmd),
  /// Show the Copilot identity headers that will be sent upstream
  Headers(headers::HeadersArgs),
  /// Run the local OpenAI-compatible server
  Serve(serve::ServeArgs),
  /// Run the local MITM forward proxy or print proxy env exports
  Proxy(proxy::ProxyArgs),
  /// Query usage statistics from the local SQLite log
  Usage(usage::UsageArgs),
  /// Get/set/list config values (git-style); preserves comments
  Config(config_cmd::ConfigArgs),
  /// Refresh the on-disk models.dev catalogue cache
  Update(update::UpdateArgs),
  /// Apply pending DB migrations (or restore from `.bak` with --rollback)
  Migration(migration::MigrationArgs),
  /// Smoke-test commands (send a request, inspect a provider, …)
  #[command(subcommand)]
  Smoke(smoke::SmokeCmd),
}

impl Cli {
  pub async fn run(self) -> Result<()> {
    let cfg_path = self.config.clone();

    // Initialize logging *before* dispatching: load just enough config to
    // pick up the [logging] section, then install the real subscriber. If
    // config loading fails we fall back to a stderr-only emergency
    // subscriber so the resulting error still gets logged sanely.
    let mode = run_mode_for(&self.cmd);
    let _guard = match Config::load(cfg_path.as_deref()) {
      Ok((cfg, _)) => Some(logging::init(&cfg.logging, mode)),
      Err(_) => {
        logging::init_basic();
        None
      }
    };

    let r: anyhow::Result<()> = match self.cmd {
      Cmd::Account(c) => account::run(cfg_path, c).await,
      Cmd::Headers(a) => headers::run(cfg_path, a).await,
      Cmd::Serve(a) => serve::run(cfg_path, a).await,
      Cmd::Proxy(a) => proxy::run(cfg_path, a).await,
      Cmd::Usage(a) => usage::run(cfg_path, a).await,
      Cmd::Config(a) => config_cmd::run(cfg_path, a).await,
      Cmd::Update(a) => update::run(a).await,
      Cmd::Migration(a) => migration::run(cfg_path, a).await,
      Cmd::Smoke(c) => smoke::run_cmd(cfg_path, c).await,
    };
    r.map_err(Error::from)
  }
}

/// Read-only subcommands keep stdout uncluttered by suppressing
/// info-level chatter from `tokn_router`. Mutating commands surface
/// progress at info; the long-running server gets full info logging.
fn run_mode_for(cmd: &Cmd) -> RunMode {
  use account::AccountCmd;
  use config_cmd::ConfigCmd::*;
  match cmd {
    Cmd::Serve(_) | Cmd::Proxy(_) => RunMode::Server,
    Cmd::Update(_) | Cmd::Migration(_) => RunMode::MutatingCli,
    Cmd::Account(c) => match c {
      AccountCmd::List(_) | AccountCmd::Show { .. } | AccountCmd::Status { .. } => RunMode::ReadOnlyCli,
      AccountCmd::Add(_)
      | AccountCmd::Login(_)
      | AccountCmd::Import(_)
      | AccountCmd::Refresh { .. }
      | AccountCmd::Remove { .. }
      | AccountCmd::Switch(_) => RunMode::MutatingCli,
    },
    Cmd::Config(args) => match args.cmd {
      Set(_) | Unset(_) | Edit | EditProfiles | Init(_) => RunMode::MutatingCli,
      _ => RunMode::ReadOnlyCli,
    },
    _ => RunMode::ReadOnlyCli,
  }
}
