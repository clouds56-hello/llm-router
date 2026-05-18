//! `gateway smoke send` — drives the router2 pipeline front-half end-to-end.
//!
//! PR3a scope:
//! - Builds a router2 [`Profile::without_send`] from the live `AppState`
//!   (account pool + route resolver). The four real stages are
//!   [`DefaultExtract`], [`PoolResolve`] backed by a [`PoolAccountSelector`],
//!   [`PersonaBuildHeaders::with_opencode_default`], and
//!   [`DefaultConvertRequest`].
//! - Subscribes a printer to the [`EventBus`] so every `StageEvent` is
//!   surfaced as a single labeled line on stdout. The legacy axum dispatch
//!   path is gone; the runner is the single source of truth.
//! - Prints the outbound preview (headers + body) from
//!   [`PipelineOutcome::built_headers`] + [`PipelineOutcome::converted_request`].
//! - The real network call (Send + ConvertResponse) is **not** wired yet —
//!   that lands in PR3b. Until then a non-`--dry-run` invocation exits with a
//!   "not yet implemented" error after the front-half completes.

use super::OutputFormat;
use crate::cli::config_cmd::RouteModeArg;
use crate::config::Config;
use crate::provider::Endpoint;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use clap::Args;
use llm_config::RouteMode;
use llm_router::api::AppState;
use llm_router2::stages::{
  DefaultConvertRequest, DefaultExtract, PersonaBuildHeaders, PoolAccountSelector, PoolResolve,
};
use llm_router2::{
  Event, EventBus, EventPayload, PipelineRunner, Profile, RawInbound, RunnerOptions, Stage, StageEvent,
};
use serde_json::Value;
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

#[derive(Args, Debug)]
pub struct SendArgs {
  /// Route mode (defaults to the serve route-mode from config).
  #[arg(long, value_enum)]
  pub route: Option<RouteModeArg>,

  /// Constrain account selection to this provider.
  #[arg(long)]
  pub provider: Option<String>,

  /// Pick a specific account by id (requires --provider).
  #[arg(long, requires = "provider")]
  pub account: Option<String>,

  /// Model to use for the smoke request.
  #[arg(long)]
  pub model: Option<String>,

  /// API endpoint to test.
  #[arg(long, value_enum, default_value_t = EndpointArg::ChatCompletions)]
  pub endpoint: EndpointArg,

  /// Request streaming SSE response. Currently informational only — the
  /// router2 front-half does not start a stream; PR3b will wire this through
  /// the real Send stage.
  #[arg(long)]
  pub stream: bool,

  /// Build and print outbound headers/body without contacting upstream.
  /// In PR3a this is the only supported mode; omitting it is reserved for
  /// PR3b.
  #[arg(long)]
  pub dry_run: bool,

  /// Output format.
  #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
  pub format: OutputFormat,

  /// Read the raw JSON request body from a file (or `-` for stdin) instead
  /// of the auto-built body. When set, `message` is optional and `--model`
  /// defaults to whatever the body declares.
  #[arg(long)]
  pub body_file: Option<PathBuf>,

  /// Inject a custom inbound header (`name=value`). Repeatable. Last wins
  /// per header name. Useful for replaying captured requests that depend on
  /// `accept`, `originator`, etc.
  #[arg(long = "header", value_parser = parse_header_kv, num_args = 0..)]
  pub headers: Vec<(String, String)>,

  /// Message to send. Optional when `--body-file` is provided.
  pub message: Option<String>,
}

fn parse_header_kv(raw: &str) -> std::result::Result<(String, String), String> {
  let (k, v) = raw
    .split_once('=')
    .or_else(|| raw.split_once(':').map(|(a, b)| (a, b.trim_start())))
    .ok_or_else(|| format!("expected `name=value` or `name: value`, got `{raw}`"))?;
  let k = k.trim().to_string();
  let v = v.trim().to_string();
  if k.is_empty() {
    return Err("header name must not be empty".into());
  }
  Ok((k, v))
}

pub async fn run(cfg_path: Option<PathBuf>, args: SendArgs) -> Result<()> {
  let (mut cfg, resolved_cfg_path) = Config::load(cfg_path.as_deref())?;
  let mut accounts = crate::server_runtime::load_accounts(Some(&resolved_cfg_path))?;

  let route_mode = args.route.map(RouteMode::from).unwrap_or(cfg.server.route_mode);
  cfg.server.route_mode = route_mode;

  if route_mode == RouteMode::Passthrough {
    anyhow::bail!("passthrough mode requires the proxy; use a different --route mode");
  }

  filter_accounts(&mut accounts, args.provider.as_deref(), args.account.as_deref())?;

  // The legacy AppState still owns the account pool + route resolver + the
  // legacy `EventBus`. We use it as a config bag only; router2 gets its own
  // bus so its events stay out of the legacy archive pipeline.
  let (legacy_events, receiver, handlers, archive_runtime) = crate::server_runtime::build_event_bus(&cfg)?;
  let _event_thread = llm_core::event::spawn_event_loop(receiver, handlers);
  let state = crate::server_runtime::build_state(&cfg, &accounts, legacy_events.clone())?;

  let custom_body: Option<Value> = match args.body_file.as_deref() {
    Some(path) => Some(load_body_file(path)?),
    None => None,
  };

  let model = match (&args.model, custom_body.as_ref()) {
    (Some(m), _) => m.clone(),
    (None, Some(body)) => body
      .get("model")
      .and_then(|v| v.as_str())
      .map(str::to_string)
      .ok_or_else(|| anyhow!("--body-file does not contain a `model` field; pass --model"))?,
    (None, None) => pick_default_model(&state, args.provider.as_deref())?,
  };

  let endpoint: Endpoint = args.endpoint.into();

  if custom_body.is_none() && args.message.is_none() {
    anyhow::bail!("missing message: pass either a positional message or --body-file");
  }

  // Build the inbound body we'll feed to router2. We keep the body symmetrical
  // with the legacy CLI behaviour so existing fixtures still work.
  let body_value = match custom_body {
    Some(mut v) => {
      if let Some(obj) = v.as_object_mut() {
        if args.model.is_some() {
          obj.insert("model".into(), Value::String(model.clone()));
        }
        if args.stream {
          obj.insert("stream".into(), Value::Bool(true));
        }
      }
      v
    }
    None => build_request_body(
      endpoint,
      &model,
      args.message.as_deref().unwrap_or(""),
      args.stream,
    ),
  };
  let body_bytes = Bytes::from(serde_json::to_vec(&body_value)?);
  let headers = build_inbound_headers(&args.headers)?;

  if args.format == OutputFormat::Text {
    println!("provider: {}", args.provider.as_deref().unwrap_or("(any)"));
    println!("account:  {}", args.account.as_deref().unwrap_or("(any)"));
    println!("model:    {}", model);
    println!("endpoint: {}", endpoint);
    println!("route:    {}", route_mode_name(route_mode));
    println!("stream:   {}", args.stream);
    if args.body_file.is_some() {
      println!("body:     {}", args.body_file.as_ref().unwrap().display());
    }
    println!();
  }

  // ---- Build the router2 profile ----
  let bus = Arc::new(EventBus::new());
  subscribe_event_printer(&bus);

  let selector = Arc::new(PoolAccountSelector::new(state.pool.clone(), state.route.clone()));
  let profile = Arc::new(Profile::without_send(
    "gateway-smoke",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
  ));
  let runner = PipelineRunner::with_options(
    profile,
    bus.clone(),
    RunnerOptions::stop_after(Stage::ConvertRequest),
  );

  let raw = RawInbound {
    endpoint,
    headers,
    raw_body: body_bytes.clone(),
    decoded_body: body_bytes.clone(),
    body_json: body_value,
    request_id: None,
  };

  let outcome = runner.run(raw).await;

  // Shut down the legacy event plumbing — router2 events are independent.
  legacy_events.shutdown().await;
  if let Some(archive_runtime) = archive_runtime {
    archive_runtime.shutdown().await;
  }

  if !outcome.success {
    let err = outcome
      .error
      .as_ref()
      .map(|e| format!("{}: {}", e.stage, e.message))
      .unwrap_or_else(|| "pipeline failed (no error attached)".into());
    anyhow::bail!("pipeline failed: {err}");
  }

  print_outcome(&outcome, args.format)?;

  if !args.dry_run {
    anyhow::bail!(
      "real upstream send is not implemented yet (PR3b); rerun with --dry-run to inspect the outbound preview"
    );
  }

  Ok(())
}

fn subscribe_event_printer(bus: &EventBus) {
  bus.subscribe(|event: &Event| {
    print_event(event);
  });
}

fn print_event(event: &Event) {
  match &event.payload {
    EventPayload::Known(StageEvent::Started { endpoint }) => {
      println!("[started]          endpoint={endpoint}");
    }
    EventPayload::Known(StageEvent::Extract { client_id, model, stream }) => {
      let cid = client_id.as_ref().map(|c| c.as_str()).unwrap_or("(none)");
      println!("[extract]          model={model} stream={stream} client_id={cid}");
    }
    EventPayload::Known(StageEvent::Resolve {
      client_id,
      model,
      upstream_model,
      account_id,
      provider_id,
      upstream_endpoint,
    }) => {
      let cid = client_id.as_ref().map(|c| c.as_str()).unwrap_or("(none)");
      println!(
        "[resolve]          model={model} -> {upstream_model} account={account_id} provider={provider_id} upstream_endpoint={upstream_endpoint} client_id={cid}"
      );
    }
    EventPayload::Known(StageEvent::BuildHeaders) => {
      println!("[build_headers]    ok");
    }
    EventPayload::Known(StageEvent::ConvertRequest) => {
      println!("[convert_request]  ok");
    }
    EventPayload::Known(StageEvent::Send) => {
      println!("[send]             ok");
    }
    EventPayload::Known(StageEvent::ConvertResponse) => {
      println!("[convert_response] ok");
    }
    EventPayload::Known(StageEvent::Error {
      stage,
      message,
      recoverable,
    }) => {
      println!("[error]            stage={stage} recoverable={recoverable} message={message}");
    }
    EventPayload::Known(StageEvent::Completed { success, attempts }) => {
      println!("[completed]        success={success} attempts={attempts}");
    }
    EventPayload::Custom(c) => {
      println!("[custom]           kind={}", c.kind);
    }
  }
}

fn print_outcome(outcome: &llm_router2::PipelineOutcome, format: OutputFormat) -> Result<()> {
  let resolved = outcome.resolved.as_ref();
  let headers = outcome.built_headers.as_ref();
  let converted = outcome.converted_request.as_ref();

  match format {
    OutputFormat::Json => {
      let headers_json = headers
        .map(|h| headers_json_value(&h.headers))
        .unwrap_or(Value::Null);
      let body_json = converted
        .map(|c| c.upstream_body.clone())
        .unwrap_or(Value::Null);
      let report = serde_json::json!({
        "dry_run": true,
        "attempts": outcome.attempts,
        "account": resolved.map(|r| r.account_id.as_str()),
        "provider": resolved.map(|r| r.provider_id.as_str()),
        "model": resolved.map(|r| r.model.as_str()),
        "upstream_model": resolved.map(|r| r.upstream_model.as_str()),
        "upstream_endpoint": resolved.map(|r| r.upstream_endpoint.to_string()),
        "headers": headers_json,
        "body": body_json,
        "content_encoding": converted.and_then(|c| c.content_encoding.map(|e| e.as_str())),
      });
      println!("{}", serde_json::to_string_pretty(&report)?);
    }
    OutputFormat::Text => {
      println!();
      println!("--- outcome ---");
      println!("attempts: {}", outcome.attempts);
      if let Some(r) = resolved {
        println!("account:  {}", r.account_id);
        println!("provider: {}", r.provider_id);
        println!("model:    {} -> {}", r.model, r.upstream_model);
        println!("upstream: {}", r.upstream_endpoint);
      }
      if let Some(h) = headers {
        println!("headers:");
        for (name, value) in h.headers.iter() {
          let v = value.as_str();
          println!("  {}: {}", name.as_str(), redact_header(name.as_str(), v));
        }
      }
      if let Some(c) = converted {
        if let Some(enc) = c.content_encoding {
          println!("content-encoding: {}", enc.as_str());
        }
        println!("body:");
        println!("{}", serde_json::to_string_pretty(&c.upstream_body)?);
      }
    }
  }
  Ok(())
}

fn headers_json_value(headers: &llm_headers::HeaderMap) -> Value {
  let mut map = serde_json::Map::new();
  for (name, value) in headers.iter() {
    let v = value.as_str();
    map.insert(
      name.as_str().to_string(),
      Value::String(redact_header(name.as_str(), v)),
    );
  }
  Value::Object(map)
}

fn redact_header(name: &str, value: &str) -> String {
  match name.to_ascii_lowercase().as_str() {
    "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key" => "<redacted>".into(),
    _ => value.to_string(),
  }
}

pub(super) fn filter_accounts(
  accounts: &mut Vec<llm_core::account::AccountConfig>,
  provider: Option<&str>,
  account: Option<&str>,
) -> Result<()> {
  if provider.is_none() && account.is_none() {
    return Ok(());
  }
  let before = accounts.len();
  accounts.retain(|a| {
    if let Some(p) = provider {
      if a.provider != p {
        return false;
      }
    }
    if let Some(id) = account {
      if a.id != id {
        return false;
      }
    }
    true
  });
  if accounts.is_empty() {
    anyhow::bail!(
      "no accounts match the requested filters (provider={:?}, account={:?}); had {before} configured",
      provider,
      account
    );
  }
  Ok(())
}

fn build_request_body(endpoint: Endpoint, model: &str, message: &str, stream: bool) -> Value {
  match endpoint {
    Endpoint::ChatCompletions | Endpoint::Messages => serde_json::json!({
      "model": model,
      "stream": stream,
      "messages": [{"role": "user", "content": message}],
    }),
    Endpoint::Responses => serde_json::json!({
      "model": model,
      "stream": stream,
      "input": message,
    }),
  }
}

fn build_inbound_headers(overrides: &[(String, String)]) -> Result<llm_headers::HeaderMap> {
  use llm_headers::{HeaderMap, HeaderName, HeaderValue};
  let mut h = HeaderMap::new();
  h.insert(
    HeaderName::new("content-type"),
    HeaderValue::from_static("application/json"),
  );
  h.insert(
    HeaderName::new("x-request-id"),
    HeaderValue::from_string(uuid::Uuid::new_v4().to_string()),
  );
  for (k, v) in overrides {
    h.insert(
      HeaderName::new(k.to_ascii_lowercase()),
      HeaderValue::from_string(v.clone()),
    );
  }
  Ok(h)
}

fn load_body_file(path: &std::path::Path) -> Result<Value> {
  use std::io::Read;
  let raw = if path == std::path::Path::new("-") {
    let mut buf = String::new();
    std::io::stdin()
      .read_to_string(&mut buf)
      .map_err(|e| anyhow!("read stdin: {e}"))?;
    buf
  } else {
    std::fs::read_to_string(path).map_err(|e| anyhow!("read {}: {e}", path.display()))?
  };
  let body_str = match raw.split_once("\nBody:\n") {
    Some((_, after)) => after.trim_start(),
    None => raw.trim_start(),
  };
  serde_json::from_str(body_str).map_err(|e| anyhow!("parse body file as JSON: {e}"))
}

fn pick_default_model(state: &AppState, provider_filter: Option<&str>) -> Result<String> {
  for acct in state.pool.all() {
    if let Some(p) = provider_filter {
      if acct.provider.info().id != p {
        continue;
      }
    }
    if let Some(m) = acct.provider.info().default_models.first() {
      return Ok(m.id.clone());
    }
  }
  match provider_filter {
    Some(p) => anyhow::bail!("no models available for provider '{}'; pass --model", p),
    None => anyhow::bail!("no models available; pass --model explicitly"),
  }
}

fn route_mode_name(mode: RouteMode) -> &'static str {
  match mode {
    RouteMode::Passthrough => "passthrough",
    RouteMode::Exact => "exact",
    RouteMode::Route => "route",
    RouteMode::Fuzzy => "fuzzy",
  }
}
