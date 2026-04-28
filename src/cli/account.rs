use crate::config::Config;
use anyhow::{anyhow, Result};
use clap::Subcommand;
use std::path::PathBuf;

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// List configured accounts
    List,
    /// Remove an account by id
    Remove { id: String },
    /// Show details for an account
    Show { id: String },
}

pub async fn run(cfg_path: Option<PathBuf>, cmd: AccountCmd) -> Result<()> {
    let (mut cfg, path) = Config::load(cfg_path.as_deref())?;
    match cmd {
        AccountCmd::List => {
            if cfg.accounts.is_empty() {
                println!("(no accounts)");
                return Ok(());
            }
            println!("{:<20}  {:<22}  {:<10}  expires_at", "id", "provider", "has_creds");
            for a in &cfg.accounts {
                // For OAuth providers we report on the cached short-lived
                // api_token; for static-key providers we report on api_key.
                // "-" expires_at is correct for static keys (never expires
                // from the daemon's perspective).
                let has = a.api_token.is_some() || a.api_key.is_some();
                let exp = a
                    .api_token_expires_at
                    .map(|t| t.to_string())
                    .unwrap_or_else(|| "-".into());
                println!("{:<20}  {:<22}  {:<10}  {}", a.id, a.provider, has, exp);
            }
        }
        AccountCmd::Remove { id } => {
            let before = cfg.accounts.len();
            cfg.accounts.retain(|a| a.id != id);
            if cfg.accounts.len() == before {
                return Err(anyhow!("no account with id '{id}'"));
            }
            cfg.save(&path)?;
            println!("Removed '{id}'");
        }
        AccountCmd::Show { id } => {
            let a = cfg
                .accounts
                .iter()
                .find(|a| a.id == id)
                .ok_or_else(|| anyhow!("no account with id '{id}'"))?;
            println!("id: {}", a.id);
            println!("provider: {}", a.provider);
            if let Some(gh) = a.github_token.as_deref() {
                println!("github_token: {}…", &gh[..gh.len().min(7)]);
            }
            if let Some(k) = a.api_key.as_deref() {
                println!("api_key: {}", mask(k));
            }
            if a.api_token.is_some() || a.api_token_expires_at.is_some() {
                println!("api_token: {}", a.api_token.as_deref().map(mask).unwrap_or("-".into()));
                println!("api_token_expires_at: {:?}", a.api_token_expires_at);
            }
            println!("override_headers: {}", a.copilot.is_some());
            if let Some(z) = &a.zai {
                if let Some(b) = &z.base_url {
                    println!("zai.base_url: {b}");
                }
            }
        }
    }
    Ok(())
}

fn mask(s: &str) -> String {
    let n = s.len();
    if n <= 8 { return "***".into(); }
    format!("{}…{}", &s[..4], &s[n - 4..])
}
