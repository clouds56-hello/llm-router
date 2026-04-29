use crate::config::{Account, AuthType, Config};
use crate::provider::{ID_GITHUB_COPILOT, ZAI_PROVIDERS};
use crate::util::secret::Secret;
use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Source {
  /// Use `gh auth token` (github-copilot only).
  Gh,
  /// Read ~/.config/github-copilot/{hosts,apps}.json (github-copilot only).
  CopilotPlugin,
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

  /// ID for the imported account.
  #[arg(long, default_value = "imported")]
  pub id: String,
}

pub async fn run(cfg_path: Option<PathBuf>, args: ImportArgs) -> Result<()> {
  let is_zai = ZAI_PROVIDERS.contains(&args.provider.as_str());
  let is_copilot = args.provider == ID_GITHUB_COPILOT;

  if !is_copilot && !is_zai {
    return Err(anyhow!(
      "unknown provider '{}'. Try one of: {ID_GITHUB_COPILOT}, {}",
      args.provider,
      ZAI_PROVIDERS.join(" | ")
    ));
  }

  let account = match (args.from, is_copilot, is_zai) {
    (Source::Gh, true, _) => copilot_account(args.id.clone(), from_gh()?),
    (Source::CopilotPlugin, true, _) => copilot_account(args.id.clone(), from_copilot_plugin()?),
    (Source::Env, _, true) => zai_account(args.id.clone(), &args.provider, from_env(&args.env_var)?),
    (Source::Env, true, _) => {
      return Err(anyhow!(
                "`--from env` is not supported for github-copilot (it needs a long-lived OAuth token, not an API key). Use `llm-router login` instead."
            ));
    }
    (Source::Gh, _, true) | (Source::CopilotPlugin, _, true) => {
      return Err(anyhow!(
                "provider '{}' is a static-API-key provider. Use `--from env --env-var <NAME>` or `llm-router login --provider {}`.",
                args.provider, args.provider
            ));
    }
    // Should be unreachable given the early provider check above.
    _ => return Err(anyhow!("unsupported provider/source combination")),
  };

  let (mut cfg, path) = Config::load(cfg_path.as_deref())?;
  let provider = account.provider.clone();
  cfg.upsert_account(account);
  cfg.save(&path)?;
  tracing::info!(
    account = %args.id,
    %provider,
    source = ?args.from,
    path = %path.display(),
    "account imported"
  );
  println!("Saved account '{}' to {}", args.id, path.display());
  Ok(())
}

fn copilot_account(id: String, token: String) -> Account {
  Account {
    id,
    provider: ID_GITHUB_COPILOT.into(),
    enabled: true,
    tags: Vec::new(),
    label: None,
    base_url: None,
    headers: Default::default(),
    auth_type: Some(AuthType::Bearer),
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: Some(Secret::new(token)),
    extra: Default::default(),
    refresh_url: Some(crate::provider::github_copilot::TOKEN_EXCHANGE_URL.into()),
    last_refresh: None,
    settings: toml::Table::new(),
  }
}

fn zai_account(id: String, provider: &str, key: String) -> Account {
  Account {
    id,
    provider: provider.into(),
    enabled: true,
    tags: Vec::new(),
    label: None,
    base_url: Some(crate::provider::zai::default_base_url(provider).into()),
    headers: Default::default(),
    auth_type: Some(AuthType::Bearer),
    username: None,
    api_key: Some(Secret::new(key)),
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: toml::Table::new(),
  }
}

fn from_env(name: &str) -> Result<String> {
  let v = std::env::var(name).with_context(|| format!("environment variable `{name}` is not set"))?;
  let v = v.trim().to_string();
  if v.is_empty() {
    return Err(anyhow!("environment variable `{name}` is empty"));
  }
  Ok(v)
}

fn from_gh() -> Result<String> {
  let out = std::process::Command::new("gh")
    .args(["auth", "token"])
    .output()
    .context("running `gh auth token` (is the GitHub CLI installed?)")?;
  if !out.status.success() {
    return Err(anyhow!(
      "`gh auth token` failed: {}",
      String::from_utf8_lossy(&out.stderr)
    ));
  }
  let token = String::from_utf8_lossy(&out.stdout).trim().to_string();
  if token.is_empty() {
    return Err(anyhow!("`gh auth token` returned an empty token"));
  }
  Ok(token)
}

fn from_copilot_plugin() -> Result<String> {
  let home = directories::BaseDirs::new()
    .ok_or_else(|| anyhow!("cannot resolve home dir"))?
    .home_dir()
    .to_path_buf();
  let candidates = [
    home.join(".config/github-copilot/apps.json"),
    home.join(".config/github-copilot/hosts.json"),
  ];
  for path in &candidates {
    if !path.exists() {
      continue;
    }
    let raw = std::fs::read_to_string(path)?;
    let v: Value = serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    if let Some(t) = scan_token(&v) {
      return Ok(t);
    }
  }
  Err(anyhow!("no Copilot plugin token found in ~/.config/github-copilot/"))
}

fn scan_token(v: &Value) -> Option<String> {
  match v {
    Value::Object(m) => {
      for (k, val) in m {
        if k == "oauth_token" || k == "token" {
          if let Some(s) = val.as_str() {
            if !s.is_empty() {
              return Some(s.to_string());
            }
          }
        }
        if let Some(found) = scan_token(val) {
          return Some(found);
        }
      }
      None
    }
    Value::Array(a) => a.iter().find_map(scan_token),
    _ => None,
  }
}
