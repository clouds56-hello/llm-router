use crate::config::{Account, Config, ProxyConfig};
use crate::provider::{github_copilot as gh, zai, ID_GITHUB_COPILOT, ID_ZAI_CODING_PLAN, ZAI_ALIASES};
use crate::util::http::build_client;
use anyhow::{anyhow, Context, Result};
use clap::Args;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct LoginArgs {
  /// Provider to log in to. If omitted and stdin is a TTY, you'll be
  /// prompted to pick one.
  ///
  /// Accepted: `github-copilot`, `zai-coding-plan`, `zai`,
  /// `zhipuai-coding-plan`, `zhipuai`. The four `zai*`/`zhipuai*` aliases
  /// all route to the same Z.ai backend; whichever you pick is preserved
  /// verbatim in the saved account so usage logs reflect operator intent.
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
  let (mut cfg, path) = Config::load(cfg_path.as_deref())?;
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

  let account = match provider.as_str() {
    ID_GITHUB_COPILOT => copilot_login(&client, &cfg, args.id).await?,
    p if ZAI_ALIASES.contains(&p) => zai_login(&client, p, args.id).await?,
    other => {
      return Err(anyhow!(
        "unknown provider '{other}'. Try one of: {}, {}",
        ID_GITHUB_COPILOT,
        ZAI_ALIASES.join(" | ")
      ));
    }
  };

  let id = account.id.clone();
  cfg.upsert_account(account);
  cfg.save(&path)?;
  println!("Saved account '{id}' to {}", path.display());
  Ok(())
}

/// Show an arrow-key picker over all five accepted provider ids. Errors out
/// (rather than silently defaulting) when stdin isn't a TTY — scripted use
/// must pass `--provider` explicitly.
fn pick_provider_interactive() -> Result<String> {
  if !std::io::stdin().is_terminal() {
    return Err(anyhow!(
      "no --provider given and stdin is not a TTY; pass --provider <id> (one of: {}, {})",
      ID_GITHUB_COPILOT,
      ZAI_ALIASES.join(" | ")
    ));
  }
  let mut options: Vec<&'static str> = Vec::with_capacity(1 + ZAI_ALIASES.len());
  options.push(ID_GITHUB_COPILOT);
  options.extend(ZAI_ALIASES.iter().copied());

  let pick = inquire::Select::new("Pick a provider:", options)
    .with_starting_cursor(0) // github-copilot
    .with_help_message("↑/↓ to move · enter to select · esc to cancel")
    .prompt()
    .context("provider selection cancelled")?;
  Ok(pick.to_string())
}

async fn copilot_login(client: &reqwest::Client, cfg: &Config, id_override: Option<String>) -> Result<Account> {
  println!("Requesting device code from GitHub…");
  let dc = gh::oauth::request_device_code(client).await?;
  println!();
  println!("  Open: {}", dc.verification_uri);
  println!("  Code: {}", dc.user_code);
  println!();
  println!("Waiting for authorization (expires in {}s)…", dc.expires_in);

  let gh_token = gh::oauth::poll_for_token(client, &dc).await?;
  println!("Got GitHub token. Verifying Copilot access…");

  let resp = gh::token::exchange(client, &gh_token, &cfg.copilot).await?;

  let id = match id_override {
    Some(s) => s,
    None => fetch_username(client, &gh_token)
      .await
      .unwrap_or_else(|_| "default".into()),
  };

  Ok(Account {
    id,
    provider: ID_GITHUB_COPILOT.into(),
    github_token: Some(gh_token),
    api_token: Some(resp.token),
    api_token_expires_at: Some(resp.expires_at),
    api_key: None,
    copilot: None,
    zai: None,
    behave_as: None,
  })
}

async fn zai_login(client: &reqwest::Client, provider_alias: &str, id_override: Option<String>) -> Result<Account> {
  println!("Z.ai uses a static API key. Create one at https://z.ai/manage-apikey/apikey-list");
  println!("(China endpoint: https://open.bigmodel.cn/usercenter/apikeys)");
  let key = rpassword::prompt_password("API key: ")
    .context("reading API key from stdin")?
    .trim()
    .to_string();
  if key.is_empty() {
    return Err(anyhow!("empty API key"));
  }

  println!("Verifying key against {} …", zai::DEFAULT_BASE_URL);
  verify_zai_key(client, &key).await?;
  println!("Key OK.");

  let id = id_override.unwrap_or_else(|| {
    // Default to the canonical id; users can pass --id to disambiguate.
    if provider_alias == ID_ZAI_CODING_PLAN {
      "coding-plan".into()
    } else {
      provider_alias.into()
    }
  });

  Ok(Account {
    id,
    provider: provider_alias.into(),
    github_token: None,
    api_token: None,
    api_token_expires_at: None,
    api_key: Some(key),
    copilot: None,
    zai: None,
    behave_as: None,
  })
}

async fn verify_zai_key(client: &reqwest::Client, key: &str) -> Result<()> {
  let url = format!("{}/models", zai::DEFAULT_BASE_URL.trim_end_matches('/'));
  let resp = client
    .get(&url)
    .header("authorization", format!("Bearer {key}"))
    .header("accept", "application/json")
    .send()
    .await
    .context("contacting Z.ai")?;
  let status = resp.status();
  if status.is_success() {
    return Ok(());
  }
  let body = resp.text().await.unwrap_or_default();
  Err(anyhow!(
    "Z.ai rejected the key (HTTP {status}). Body: {}",
    body.chars().take(200).collect::<String>()
  ))
}

async fn fetch_username(client: &reqwest::Client, gh_token: &str) -> Result<String> {
  #[derive(serde::Deserialize)]
  struct Me {
    login: String,
  }
  let me: Me = client
    .get("https://api.github.com/user")
    .header("authorization", format!("token {gh_token}"))
    .header("accept", "application/json")
    .header("user-agent", "llm-router")
    .send()
    .await?
    .error_for_status()?
    .json()
    .await?;
  Ok(me.login)
}
