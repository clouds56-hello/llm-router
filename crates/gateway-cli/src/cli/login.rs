use crate::auth_registry::known_providers;
use crate::cli::onboarding::{resolve_account, CredentialSource};
use crate::config::{Config, ProxyConfig};
use crate::util::http::build_client;
use anyhow::{anyhow, Context, Result};
use clap::Args;
use tokn_auth::AuthStore;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct LoginArgs {
  /// Provider to log in to. If omitted and stdin is a TTY, you'll be
  /// prompted to pick one.
  ///
  /// Accepted provider ids are shown by the interactive picker. Z.ai aliases
  /// route to the same backend; whichever you pick is preserved verbatim.
  #[arg(long)]
  pub provider: Option<String>,

  /// ID to assign to the new account. Defaults to the GitHub username for
  /// `github-copilot`, or to the provider id for static-key providers.
  #[arg(long)]
  pub id: Option<String>,

  /// Skip outbound proxy for this command (e.g. captive networks).
  #[arg(long)]
  pub no_proxy: bool,
}

pub async fn run(cfg_path: Option<PathBuf>, args: LoginArgs) -> Result<()> {
  let (cfg, path) = Config::load(cfg_path.as_deref())?;
  let mut store = AuthStore::load(None, Some(&path))?;
  let proxy = if args.no_proxy {
    ProxyConfig::default()
  } else {
    cfg.proxy.clone()
  };
  let client = build_client(&proxy)?;

  let provider = match args.provider {
    Some(p) => p,
    None => pick_provider_interactive()?,
  };
  let account = resolve_account(&client, &provider, args.id, CredentialSource::Login).await?;

  let id = account.id.clone();
  let provider = account.provider.clone();
  store.upsert(account);
  store.save()?;
  tracing::info!(account = %id, %provider, path = %store.path().display(), "account saved");
  println!("Saved account '{id}' to {}", store.path().display());
  Ok(())
}

/// Show an arrow-key picker over all five accepted provider ids. Errors out
/// (rather than silently defaulting) when stdin isn't a TTY — scripted use
/// must pass `--provider` explicitly.
fn pick_provider_interactive() -> Result<String> {
  if !std::io::stdin().is_terminal() {
    return Err(anyhow!(
      "no --provider given and stdin is not a TTY; pass --provider <id> (one of: {})",
      known_providers().join(" | ")
    ));
  }
  let options = known_providers().to_vec();

  let pick = inquire::Select::new("Pick a provider:", options)
    .with_starting_cursor(0) // github-copilot
    .with_help_message("↑/↓ to move · enter to select · esc to cancel")
    .prompt()
    .context("provider selection cancelled")?;
  Ok(pick.to_string())
}
