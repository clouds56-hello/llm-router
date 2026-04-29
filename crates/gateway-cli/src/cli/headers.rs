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
    None => llm_provider_copilot::config::CopilotHeaders::from_value(&cfg.copilot)?,
    Some(id) => {
      let a = cfg
        .accounts
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| anyhow!("no account with id '{id}'"))?;
      if a.provider != crate::provider::ID_GITHUB_COPILOT {
        println!(
          "Account '{id}' uses provider '{}', which does not send Copilot identity headers.",
          a.provider
        );
        println!("(headers are only relevant for the github-copilot provider)");
        return Ok(());
      }
      let global = llm_provider_copilot::config::CopilotHeaders::from_value(&cfg.copilot)?;
      let account = a
        .copilot
        .as_ref()
        .map(llm_provider_copilot::config::CopilotHeaders::from_value)
        .transpose()?;
      global.merged(account.as_ref())
    }
  };
  println!("editor-version:         {}", headers.editor_version);
  println!("editor-plugin-version:  {}", headers.editor_plugin_version);
  println!("user-agent:             {}", headers.user_agent);
  println!("copilot-integration-id: {}", headers.copilot_integration_id);
  println!("openai-intent:          {}", headers.openai_intent);
  println!("initiator_mode:         {:?}", headers.initiator_mode);
  match &headers.behave_as {
    Some(p) => {
      let profiles = crate::provider::profiles::Profiles::global();
      let verified = profiles
        .resolve(p, crate::provider::ID_GITHUB_COPILOT)
        .map(|r| r.verified)
        .unwrap_or(false);
      let tag = if verified { "verified" } else { "UNVERIFIED" };
      println!("behave_as:              {p} ({tag})");
    }
    None => println!("behave_as:              -"),
  }
  if !headers.extra_headers.is_empty() {
    println!("extra:");
    for (k, v) in &headers.extra_headers {
      println!("  {k}: {v}");
    }
  }
  Ok(())
}
