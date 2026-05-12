use crate::config::Config;
use crate::provider::{Endpoint, RequestCtx};
use anyhow::{anyhow, Result};
use clap::{Args, ValueEnum};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum EndpointArg {
  ChatCompletions,
  Responses,
  Messages,
}

impl From<EndpointArg> for Endpoint {
  fn from(val: EndpointArg) -> Self {
    match val {
      EndpointArg::ChatCompletions => Endpoint::ChatCompletions,
      EndpointArg::Responses => Endpoint::Responses,
      EndpointArg::Messages => Endpoint::Messages,
    }
  }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
  Text,
  Json,
}

#[derive(Args, Debug)]
pub struct SmokeArgs {
  /// Filter by provider id.
  #[arg(long)]
  pub provider: Option<String>,

  /// Filter by account id.
  #[arg(long)]
  pub account: Option<String>,

  /// Model to use for the smoke request.
  #[arg(long)]
  pub model: Option<String>,

  /// API endpoint to test.
  #[arg(long, value_enum, default_value_t = EndpointArg::ChatCompletions)]
  pub endpoint: EndpointArg,

  /// Output format.
  #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
  pub format: OutputFormat,

  /// Message to send.
  pub message: String,
}

pub async fn run(cfg_path: Option<PathBuf>, args: SmokeArgs) -> Result<()> {
  let (cfg, _) = Config::load(cfg_path.as_deref())?;

  let account_cfg = resolve_account(&cfg, args.account.as_deref(), args.provider.as_deref())?;
  let provider = llm_router::accounts::registry::build_for_account(Arc::new(account_cfg.clone()))?;

  let model = match &args.model {
    Some(m) => m.clone(),
    None => provider
      .info()
      .default_models
      .first()
      .map(|m| m.id.clone())
      .ok_or_else(|| anyhow!("no default model for provider '{}'; pass --model", account_cfg.provider))?,
  };

  let http = crate::util::http::build_client(&cfg.proxy)?;
  let endpoint: Endpoint = args.endpoint.into();

  if !provider.supports(&model, endpoint) {
    anyhow::bail!(
      "provider '{}' does not support model '{}' on endpoint {}",
      provider.info().id,
      model,
      endpoint,
    );
  }

  if args.format == OutputFormat::Text {
    println!("account:  {}", account_cfg.id);
    println!("provider: {}", account_cfg.provider);
    println!("model:    {model}");
    println!("endpoint: {endpoint}");
    println!();
  }

  let body = build_request_body(&model, &args.message);

  let ctx = RequestCtx {
    endpoint,
    http: &http,
    body: &body,
    body_bytes: None,
    content_encoding: None,
    stream: false,
    initiator: "smoke",
    inbound_headers: &Default::default(),
    behave_as: None,
    outbound: None,
  };

  let resp = match endpoint {
    Endpoint::ChatCompletions => provider.chat(ctx).await?,
    Endpoint::Responses => provider.responses(ctx).await?,
    Endpoint::Messages => provider.messages(ctx).await?,
  };

  let status = resp.status();
  let body_bytes = resp.bytes().await?;

  if args.format == OutputFormat::Json {
    print_json_response(status, &body_bytes)?;
  } else {
    print_text_response(status, &body_bytes)?;
  }

  if !status.is_success() {
    std::process::exit(1);
  }

  Ok(())
}

fn resolve_account<'a>(
  cfg: &'a Config,
  account_id: Option<&str>,
  provider_id: Option<&str>,
) -> Result<crate::config::AccountConfig> {
  let matches: Vec<&crate::config::AccountConfig> = cfg
    .accounts
    .iter()
    .filter(|a| a.enabled)
    .filter(|a| match account_id {
      Some(id) => a.id == id,
      None => true,
    })
    .filter(|a| match provider_id {
      Some(id) => a.provider == id,
      None => true,
    })
    .collect();

  match matches.len() {
    0 => Err(anyhow!(
      "no matching account found (--account={:?}, --provider={:?})",
      account_id,
      provider_id,
    )),
    1 => Ok(matches[0].clone()),
    n => {
      let names: Vec<&str> = matches.iter().map(|a| a.id.as_str()).collect();
      Err(anyhow!(
        "{n} accounts match; narrow with --account or --provider: {}",
        names.join(", "),
      ))
    }
  }
}

fn build_request_body(model: &str, message: &str) -> serde_json::Value {
  serde_json::json!({
    "model": model,
    "stream": false,
    "messages": [{"role": "user", "content": message}],
  })
}

fn print_text_response(status: reqwest::StatusCode, body: &[u8]) -> Result<()> {
  println!("status: {}", status.as_u16());
  let text = String::from_utf8_lossy(body);
  let json: serde_json::Value = match serde_json::from_slice(body) {
    Ok(v) => v,
    Err(_) => {
      println!("{text}");
      return Ok(());
    }
  };

  if let Some(choices) = json.get("choices").and_then(|c| c.as_array()) {
    for (i, choice) in choices.iter().enumerate() {
      let content = choice
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("(no content)");
      println!("--- choice {} ---", i);
      println!("{content}");
    }
  } else if let Some(output) = json.get("output").and_then(|o| o.as_array()) {
    for item in output {
      if let Some(content) = item.get("content") {
        if let Some(text) = content.get("text").and_then(|t| t.as_str()) {
          println!("{text}");
        } else if let Some(arr) = content.as_array() {
          for part in arr {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
              println!("{text}");
            }
          }
        }
      }
    }
  } else if let Some(content) = json.get("content").and_then(|c| c.as_array()) {
    for block in content {
      if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
        println!("{text}");
      }
    }
  } else {
    println!("{text}");
  }

  Ok(())
}

fn print_json_response(status: reqwest::StatusCode, body: &[u8]) -> Result<()> {
  let json: serde_json::Value = serde_json::from_slice(body).unwrap_or_else(|_| {
    serde_json::json!({
      "status": status.as_u16(),
      "body": String::from_utf8_lossy(body),
    })
  });
  let output = serde_json::to_string_pretty(&json)?;
  println!("{output}");
  Ok(())
}
