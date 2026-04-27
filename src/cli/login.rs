use crate::config::{Account, Config};
use crate::copilot;
use crate::util::http::build_client;
use anyhow::Result;
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// ID to assign to the new account (default: github username)
    #[arg(long)]
    pub id: Option<String>,
}

pub async fn run(cfg_path: Option<PathBuf>, args: LoginArgs) -> Result<()> {
    let (mut cfg, path) = Config::load(cfg_path.as_deref())?;
    let client = build_client()?;

    println!("Requesting device code from GitHub…");
    let dc = copilot::oauth::request_device_code(&client).await?;
    println!();
    println!("  Open: {}", dc.verification_uri);
    println!("  Code: {}", dc.user_code);
    println!();
    println!("Waiting for authorization (expires in {}s)…", dc.expires_in);

    let gh_token = copilot::oauth::poll_for_token(&client, &dc).await?;
    println!("Got GitHub token. Verifying Copilot access…");

    let resp = copilot::token::exchange(&client, &gh_token, &cfg.copilot).await?;

    let id = match args.id {
        Some(s) => s,
        None => fetch_username(&client, &gh_token).await.unwrap_or_else(|_| "default".into()),
    };

    cfg.upsert_account(Account {
        id: id.clone(),
        github_token: gh_token,
        api_token: Some(resp.token),
        api_token_expires_at: Some(resp.expires_at),
        copilot: None,
    });
    cfg.save(&path)?;
    println!("Saved account '{id}' to {}", path.display());
    Ok(())
}

async fn fetch_username(client: &reqwest::Client, gh_token: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    struct Me { login: String }
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
