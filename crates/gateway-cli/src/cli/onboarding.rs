//! Interactive onboarding helpers shared by `account add`, `login`, and
//! `import`.
//!
//! All provider-specific work — device-flow OAuth, key verification —
//! is delegated to the [`ProviderAuth`] trait via [`auth_registry`]. The
//! CLI's job is purely to gather user input, render progress, and
//! assemble the resulting [`Account`].
//!
//! [`auth_registry`]: crate::auth_registry

use crate::auth_registry::{known_providers, provider_auth_for};
use crate::config::{Account, AuthType};
use crate::util::secret::Secret;
use anyhow::{anyhow, Context, Result};
use llm_auth::ProviderAuth;

// Re-export so existing call sites continue to use
// `crate::cli::onboarding::CredentialSource`. New code should import it
// directly from `llm_auth`.
pub use llm_auth::CredentialSource;

pub fn validate_provider(provider: &str) -> Result<()> {
  if provider_auth_for(provider).is_some() {
    return Ok(());
  }
  Err(anyhow!(
    "unknown provider '{provider}'. Try one of: {}",
    known_providers().join(" | ")
  ))
}

pub fn validate_provider_source(provider: &str, source: &CredentialSource) -> Result<()> {
  let auth = provider_auth_for(provider).ok_or_else(|| anyhow!("unknown provider '{provider}'"))?;
  if auth.supports_credential_source(source) {
    return Ok(());
  }
  // Provider-specific hint for the most common mistake.
  let hint = match source {
    CredentialSource::Env { .. } if auth.supports_device_flow() => {
      " — this provider needs a long-lived OAuth token; try `from=login|gh|copilot-plugin`."
    }
    CredentialSource::Gh | CredentialSource::CopilotPlugin | CredentialSource::RefreshToken { .. }
      if auth.supports_static_key() =>
    {
      " — this provider uses a static API key; try `from=login` or `from=env`."
    }
    _ => "",
  };
  Err(anyhow!(
    "credential source not supported by provider '{provider}'{hint}"
  ))
}

pub async fn resolve_account(
  client: &reqwest::Client,
  provider: &str,
  id_override: Option<String>,
  source: CredentialSource,
) -> Result<Account> {
  validate_provider(provider)?;
  validate_provider_source(provider, &source)?;
  let auth = provider_auth_for(provider).expect("validated above");

  match source {
    CredentialSource::Login => {
      if auth.supports_device_flow() {
        device_flow_login(client, auth, id_override).await
      } else {
        // Static-key provider: prompt for the key, verify, build account.
        static_key_login(client, auth, id_override).await
      }
    }
    CredentialSource::Gh => oauth_account_from_token(auth, id_override, from_gh()?),
    CredentialSource::CopilotPlugin => {
      oauth_account_from_token(auth, id_override, from_copilot_plugin()?)
    }
    CredentialSource::RefreshToken { token } => oauth_account_from_token(auth, id_override, token),
    CredentialSource::Env { env_var } => static_key_account(auth, id_override, from_env(&env_var)?),
  }
}

/// Build an [`Account`] for an OAuth provider given a long-lived refresh
/// token. Does not contact the upstream — the next refresh will do that.
fn oauth_account_from_token(
  auth: &dyn ProviderAuth,
  id_override: Option<String>,
  token: String,
) -> Result<Account> {
  Ok(Account {
    id: id_override.unwrap_or_else(|| "imported".into()),
    provider: auth.id().into(),
    enabled: true,
    tier: llm_core::account::AccountTier::Active,
    tags: Vec::new(),
    label: None,
    base_url: auth.default_base_url().map(str::to_string),
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
    refresh_url: auth.default_refresh_url().map(str::to_string),
    last_refresh: None,
    settings: toml::Table::new(),
  })
}

/// Build an [`Account`] for a static-API-key provider given the raw key.
fn static_key_account(
  auth: &dyn ProviderAuth,
  id_override: Option<String>,
  key: String,
) -> Result<Account> {
  let id = id_override.unwrap_or_else(|| auth.default_account_id().to_string());
  Ok(Account {
    id,
    provider: auth.id().into(),
    enabled: true,
    tier: llm_core::account::AccountTier::Active,
    tags: Vec::new(),
    label: None,
    base_url: auth.default_base_url().map(str::to_string),
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
  })
}

/// Run a provider's device-flow login interactively. Splits the trait's
/// request/poll calls so the user code can be displayed *between* the
/// two — the polling step blocks for ~minutes waiting on the browser.
async fn device_flow_login(
  client: &reqwest::Client,
  auth: &dyn ProviderAuth,
  id_override: Option<String>,
) -> Result<Account> {
  println!("Requesting device code from {} …", auth.id());
  let handle = auth
    .request_device_code(client)
    .await
    .map_err(|e| anyhow!("device code request failed: {e}"))?;
  println!();
  println!("  Open: {}", handle.verification_uri);
  println!("  Code: {}", handle.user_code);
  println!();
  println!("Waiting for authorization (expires in {}s) …", handle.expires_in);

  let outcome = auth
    .poll_device_code(client, handle)
    .await
    .map_err(|e| anyhow!("device-flow polling failed: {e}"))?;
  println!("Got token. ");

  let id = id_override
    .or(outcome.username)
    .unwrap_or_else(|| auth.default_account_id().to_string());

  Ok(Account {
    id,
    provider: auth.id().into(),
    enabled: true,
    tier: llm_core::account::AccountTier::Active,
    tags: Vec::new(),
    label: None,
    base_url: auth.default_base_url().map(str::to_string),
    headers: Default::default(),
    auth_type: Some(AuthType::Bearer),
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: Some(Secret::new(outcome.access_token)),
    access_token_expires_at: Some(outcome.access_token_expires_at),
    id_token: None,
    refresh_token: Some(Secret::new(outcome.refresh_token)),
    extra: Default::default(),
    refresh_url: auth.default_refresh_url().map(str::to_string),
    last_refresh: Some(time::OffsetDateTime::now_utc().unix_timestamp()),
    settings: toml::Table::new(),
  })
}

/// Prompt for a static API key, verify it via the provider's
/// [`ProviderAuth::verify_credential`], and return the assembled account.
async fn static_key_login(
  client: &reqwest::Client,
  auth: &dyn ProviderAuth,
  id_override: Option<String>,
) -> Result<Account> {
  println!(
    "{} uses a static API key. Paste your key below.",
    auth.id()
  );
  let key = rpassword::prompt_password("API key: ")
    .context("reading API key from stdin")?
    .trim()
    .to_string();
  if key.is_empty() {
    return Err(anyhow!("empty API key"));
  }

  // Build a throwaway Account so the trait can verify against it.
  let probe = static_key_account(auth, Some("__probe__".into()), key.clone())?;
  println!(
    "Verifying key against {} …",
    probe.base_url.as_deref().unwrap_or("upstream")
  );
  auth
    .verify_credential(client, &probe)
    .await
    .map_err(|e| anyhow!("key verification failed: {e}"))?;
  println!("Key OK.");

  static_key_account(auth, id_override, key)
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
    let v: serde_json::Value = serde_json::from_str(&raw).with_context(|| format!("parse {}", path.display()))?;
    if let Some(t) = scan_token(&v) {
      return Ok(t);
    }
  }
  Err(anyhow!("no Copilot plugin token found in ~/.config/github-copilot/"))
}

fn scan_token(v: &serde_json::Value) -> Option<String> {
  match v {
    serde_json::Value::Object(m) => {
      for (k, val) in m {
        if (k == "oauth_token" || k == "token") && val.as_str().filter(|s| !s.is_empty()).is_some() {
          return val.as_str().map(|s| s.to_string());
        }
        if let Some(found) = scan_token(val) {
          return Some(found);
        }
      }
      None
    }
    serde_json::Value::Array(a) => a.iter().find_map(scan_token),
    _ => None,
  }
}

// ---------------------------------------------------------------------------
// Interactive helpers (used by `account add` and `config init`).
// ---------------------------------------------------------------------------

/// One-shot interactive flow: pick provider → pick credential source →
/// pick id → resolve account. Caller is responsible for upserting +
/// saving the resulting [`Account`].
pub(crate) async fn interactive_add_account(
  client: &reqwest::Client,
  provider_override: Option<String>,
  id_override: Option<String>,
) -> Result<Account> {
  let provider = match provider_override {
    Some(p) => p,
    None => pick_provider()?,
  };
  validate_provider(&provider)?;
  let source = pick_source_interactive(&provider)?;
  let id = match id_override {
    Some(s) => Some(s),
    None => pick_account_id(&provider, &source)?,
  };
  resolve_account(client, &provider, id, source).await
}

pub(crate) fn pick_provider() -> Result<String> {
  let options = known_providers().to_vec();
  let selected = inquire::Select::new("Pick account provider:", options)
    .with_starting_cursor(0)
    .prompt()
    .context("provider selection cancelled")?;
  Ok(selected.to_string())
}

pub(crate) fn pick_source_interactive(provider: &str) -> Result<CredentialSource> {
  // Build the menu from the trait capabilities so the list automatically
  // tracks any new provider.
  let auth = provider_auth_for(provider).ok_or_else(|| anyhow!("unknown provider '{provider}'"))?;
  let mut options: Vec<&str> = vec!["login"];
  if auth.supports_credential_source(&CredentialSource::Gh) {
    options.push("gh");
  }
  if auth.supports_credential_source(&CredentialSource::CopilotPlugin) {
    options.push("copilot-plugin");
  }
  if auth.supports_credential_source(&CredentialSource::RefreshToken { token: String::new() }) {
    options.push("refresh-token");
  }
  if auth.supports_credential_source(&CredentialSource::Env { env_var: String::new() }) {
    options.push("env");
  }

  let picked = inquire::Select::new("Credential source:", options)
    .with_starting_cursor(0)
    .prompt()
    .context("credential source selection cancelled")?;
  match picked {
    "login" => Ok(CredentialSource::Login),
    "gh" => Ok(CredentialSource::Gh),
    "copilot-plugin" => Ok(CredentialSource::CopilotPlugin),
    "refresh-token" => {
      let token = inquire::Text::new("Refresh token (leave empty to use env var):")
        .prompt()
        .context("refresh token prompt cancelled")?;
      let trimmed = token.trim().to_string();
      let token = if trimmed.is_empty() {
        let env_var = inquire::Text::new("Refresh token env var:")
          .with_initial_value("GITHUB_COPILOT_REFRESH_TOKEN")
          .prompt()
          .context("refresh token env var prompt cancelled")?;
        let value = std::env::var(&env_var).map_err(|_| anyhow!("environment variable `{env_var}` is not set"))?;
        let v = value.trim().to_string();
        if v.is_empty() {
          return Err(anyhow!("environment variable `{env_var}` is empty"));
        }
        v
      } else {
        trimmed
      };
      Ok(CredentialSource::RefreshToken { token })
    }
    "env" => {
      let env_var = inquire::Text::new("Environment variable containing API key:")
        .with_initial_value("ZAI_API_KEY")
        .prompt()
        .context("env var prompt cancelled")?;
      Ok(CredentialSource::Env { env_var })
    }
    _ => Err(anyhow!("unsupported credential source")),
  }
}

pub(crate) fn pick_account_id(provider: &str, source: &CredentialSource) -> Result<Option<String>> {
  let default_id = provider_auth_for(provider)
    .map(|a| a.default_account_id())
    .unwrap_or("imported");
  let prompt = match source {
    CredentialSource::Login => "Account id (leave empty for auto):",
    _ => "Account id:",
  };
  let text = inquire::Text::new(prompt)
    .with_initial_value(default_id)
    .prompt()
    .context("account id prompt cancelled")?;
  let trimmed = text.trim().to_string();
  if trimmed.is_empty() {
    return Ok(None);
  }
  Ok(Some(trimmed))
}
