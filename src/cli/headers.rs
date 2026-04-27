use crate::config::Config;
use anyhow::{anyhow, Result};
use clap::Args;
use std::path::PathBuf;

#[derive(Args, Debug)]
pub struct HeadersArgs {
    /// Show resolved headers for a specific account (defaults to the global set)
    #[arg(long)]
    pub account: Option<String>,
}

pub async fn run(cfg_path: Option<PathBuf>, args: HeadersArgs) -> Result<()> {
    let (cfg, _) = Config::load(cfg_path.as_deref())?;
    let headers = match args.account {
        None => cfg.copilot.clone(),
        Some(id) => {
            let a = cfg
                .accounts
                .iter()
                .find(|a| a.id == id)
                .ok_or_else(|| anyhow!("no account with id '{id}'"))?;
            cfg.copilot.merged(a.copilot.as_ref())
        }
    };
    println!("editor-version:         {}", headers.editor_version);
    println!("editor-plugin-version:  {}", headers.editor_plugin_version);
    println!("user-agent:             {}", headers.user_agent);
    println!("copilot-integration-id: {}", headers.copilot_integration_id);
    println!("openai-intent:          {}", headers.openai_intent);
    if !headers.extra_headers.is_empty() {
        println!("extra:");
        for (k, v) in &headers.extra_headers {
            println!("  {k}: {v}");
        }
    }
    Ok(())
}
