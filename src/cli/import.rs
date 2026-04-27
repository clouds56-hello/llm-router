use crate::config::{Account, Config};
use anyhow::{anyhow, Context, Result};
use clap::{Args, ValueEnum};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Source {
    /// Use `gh auth token`
    Gh,
    /// Read ~/.config/github-copilot/{hosts,apps}.json
    CopilotPlugin,
}

#[derive(Args, Debug)]
pub struct ImportArgs {
    #[arg(long, value_enum, default_value_t = Source::Gh)]
    pub from: Source,

    /// ID for the imported account
    #[arg(long, default_value = "imported")]
    pub id: String,
}

pub async fn run(cfg_path: Option<PathBuf>, args: ImportArgs) -> Result<()> {
    let token = match args.from {
        Source::Gh => from_gh()?,
        Source::CopilotPlugin => from_copilot_plugin()?,
    };
    let (mut cfg, path) = Config::load(cfg_path.as_deref())?;
    cfg.upsert_account(Account {
        id: args.id.clone(),
        provider: crate::provider::ID_GITHUB_COPILOT.into(),
        github_token: Some(token),
        api_token: None,
        api_token_expires_at: None,
        copilot: None,
        behave_as: None,
    });
    cfg.save(&path)?;
    println!("Saved account '{}' to {}", args.id, path.display());
    Ok(())
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
        let v: Value = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", path.display()))?;
        if let Some(t) = scan_token(&v) {
            return Ok(t);
        }
    }
    Err(anyhow!(
        "no Copilot plugin token found in ~/.config/github-copilot/"
    ))
}

fn scan_token(v: &Value) -> Option<String> {
    match v {
        Value::Object(m) => {
            for (k, val) in m {
                if k == "oauth_token" || k == "token" {
                    if let Some(s) = val.as_str() {
                        if !s.is_empty() { return Some(s.to_string()); }
                    }
                }
                if let Some(found) = scan_token(val) { return Some(found); }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(scan_token),
        _ => None,
    }
}
