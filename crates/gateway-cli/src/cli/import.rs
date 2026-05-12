use crate::cli::onboarding::{resolve_account, CredentialSource};
use crate::config::{Config, ProxyConfig};
use crate::provider::ID_GITHUB_COPILOT;
use crate::util::http::build_client;
use anyhow::Result;
use clap::{Args, ValueEnum};
use llm_auth::AuthStore;
use std::path::PathBuf;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Source {
  /// Use `gh auth token` (github-copilot only).
  Gh,
  /// Read ~/.config/github-copilot/{hosts,apps}.json (github-copilot only).
  CopilotPlugin,
  /// Use a raw GitHub Copilot refresh token (github-copilot only).
  RefreshToken,
  /// Read the API key from an environment variable. Use with `--env-var
  /// <NAME>`. Compatible with any static-API-key provider (zai*, zhipuai*).
  Env,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
  #[arg(long, value_enum, default_value_t = Source::Gh)]
  pub from: Source,

  /// Provider to associate the imported credential with.
  #[arg(long, default_value = ID_GITHUB_COPILOT)]
  pub provider: String,

  /// Environment variable name for `--from env`. Defaults to `ZAI_API_KEY`.
  #[arg(long, default_value = "ZAI_API_KEY")]
  pub env_var: String,

  /// Raw refresh token when `--from refresh-token`.
  #[arg(long)]
  pub refresh_token: Option<String>,

  /// Environment variable used when `--from refresh-token` and
  /// `--refresh-token` is omitted.
  #[arg(long, default_value = "GITHUB_COPILOT_REFRESH_TOKEN")]
  pub refresh_token_env_var: String,

  /// ID for the imported account.
  #[arg(long, default_value = "imported")]
  pub id: String,
}

pub async fn run(cfg_path: Option<PathBuf>, args: ImportArgs) -> Result<()> {
  let source = match args.from {
    Source::Gh => CredentialSource::Gh,
    Source::CopilotPlugin => CredentialSource::CopilotPlugin,
    Source::RefreshToken => CredentialSource::RefreshToken {
      token: resolve_refresh_token(&args)?,
    },
    Source::Env => CredentialSource::Env {
      env_var: args.env_var.clone(),
    },
  };
  let client = build_client(&ProxyConfig::default())?;
  let account = resolve_account(&client, &args.provider, Some(args.id.clone()), source).await?;

  let (_cfg, path) = Config::load(cfg_path.as_deref())?;
  let mut store = AuthStore::load(None, Some(&path))?;
  let provider = account.provider.clone();
  store.upsert(account);
  store.save()?;
  tracing::info!(
    account = %args.id,
    %provider,
    source = ?args.from,
    path = %store.path().display(),
    "account imported"
  );
  println!("Saved account '{}' to {}", args.id, store.path().display());
  Ok(())
}

fn resolve_refresh_token(args: &ImportArgs) -> Result<String> {
  if let Some(token) = &args.refresh_token {
    let trimmed = token.trim();
    if trimmed.is_empty() {
      anyhow::bail!("--refresh-token cannot be empty");
    }
    return Ok(trimmed.to_string());
  }
  let name = &args.refresh_token_env_var;
  let value = std::env::var(name)
    .map_err(|_| anyhow::anyhow!("environment variable `{name}` is not set; pass --refresh-token or set {name}"))?;
  let trimmed = value.trim();
  if trimmed.is_empty() {
    anyhow::bail!("environment variable `{name}` is empty");
  }
  Ok(trimmed.to_string())
}

#[cfg(test)]
mod tests {
  use super::*;

  fn args() -> ImportArgs {
    ImportArgs {
      from: Source::RefreshToken,
      provider: ID_GITHUB_COPILOT.to_string(),
      env_var: "ZAI_API_KEY".to_string(),
      refresh_token: None,
      refresh_token_env_var: "TEST_GH_REFRESH_TOKEN".to_string(),
      id: "imported".to_string(),
    }
  }

  #[test]
  fn resolve_refresh_token_prefers_flag() {
    let mut a = args();
    a.refresh_token = Some("  abc  ".to_string());
    let token = resolve_refresh_token(&a).unwrap();
    assert_eq!(token, "abc");
  }
}
