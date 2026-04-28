use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod account;
mod config_cmd;
mod headers;
mod import;
mod login;
mod serve;
mod update;
mod usage;

#[derive(Parser, Debug)]
#[command(name = "llm-router", version, about = "GitHub Copilot -> OpenAI-compatible API")]
pub struct Cli {
  /// Path to config file (default: ~/.config/llm-router/config.toml)
  #[arg(long, global = true)]
  pub config: Option<PathBuf>,

  #[command(subcommand)]
  pub cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
  /// Add a Copilot account via GitHub device-flow login
  Login(login::LoginArgs),
  /// Import an existing GitHub token (from `gh` or the Copilot plugin)
  Import(import::ImportArgs),
  /// Manage stored accounts
  #[command(subcommand)]
  Account(account::AccountCmd),
  /// Show the Copilot identity headers that will be sent upstream
  Headers(headers::HeadersArgs),
  /// Run the local OpenAI-compatible server
  Serve(serve::ServeArgs),
  /// Query usage statistics from the local SQLite log
  Usage(usage::UsageArgs),
  /// Get/set/list config values (git-style); preserves comments
  Config(config_cmd::ConfigArgs),
  /// Refresh the on-disk models.dev catalogue cache
  Update(update::UpdateArgs),
}

impl Cli {
  pub async fn run(self) -> Result<()> {
    let cfg_path = self.config.clone();
    match self.cmd {
      Cmd::Login(a) => login::run(cfg_path, a).await,
      Cmd::Import(a) => import::run(cfg_path, a).await,
      Cmd::Account(c) => account::run(cfg_path, c).await,
      Cmd::Headers(a) => headers::run(cfg_path, a).await,
      Cmd::Serve(a) => serve::run(cfg_path, a).await,
      Cmd::Usage(a) => usage::run(cfg_path, a).await,
      Cmd::Config(a) => config_cmd::run(cfg_path, a).await,
      Cmd::Update(a) => update::run(a).await,
    }
  }
}
