//! `gateway smoke send` — drives the router2 pipeline end-to-end.
//!
//! Two modes:
//!
//! * `--dry-run` builds a [`Profile::without_send`] and stops after
//!   `ConvertRequest`, printing the outbound preview (headers + body) from
//!   [`PipelineOutcome::built_headers`] + [`PipelineOutcome::converted_request`].
//!   Useful for inspecting what would be sent without touching the network.
//! * Default (live) mode builds a [`Profile::full`] with [`DefaultSend`] and
//!   [`DefaultConvertResponse`] and contacts the upstream provider. The
//!   response is either printed as a buffered JSON payload or streamed
//!   chunk-by-chunk to stdout (curl `-N` style).
//!
//! In both modes every [`StageEvent`] is mirrored to stdout via a
//! subscriber on the router2 [`EventBus`], so the user can see the pipeline
//! progress in real time.

use super::OutputFormat;
use crate::cli::config_cmd::RouteModeArg;
use crate::config::Config;
use crate::provider::Endpoint;
use anyhow::{anyhow, Result};
use bytes::Bytes;
use clap::Args;
use futures_util::StreamExt;
use llm_config::RouteMode;
use llm_router::api::AppState;
use llm_router2::pipeline::stages::ConvertedResponse;
use llm_router2::stages::{
  DefaultConvertRequest, DefaultConvertResponse, DefaultExtract, DefaultSend, PersonaBuildHeaders, PoolAccountSelector,
  PoolResolve,
};
use llm_router2::{
  Event, EventBus, EventPayload, PipelineOutcome, PipelineRunner, Profile, RawInbound, RunnerOptions, Stage, StageEvent,
};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

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

  /// Request streaming SSE response. The response is forwarded chunk-by-
  /// chunk to stdout as it arrives from upstream.
  #[arg(long)]
  pub stream: bool,

  /// Build and print outbound headers/body without contacting upstream.
  /// Equivalent to running the pipeline up to (and including)
  /// `ConvertRequest` and stopping; nothing is sent.
  #[arg(long)]
  pub dry_run: bool,

  /// Output format.
  #[arg(long, value_enum, default_value_t = OutputFormat::Text)]
  pub format: OutputFormat,

  /// Print outbound and upstream headers verbatim instead of redacting
  /// sensitive values (authorization, cookies, api keys). Off by default
  /// because output is often pasted into bug reports — only set when you
  /// are actively debugging upstream auth and know what you're showing.
  #[arg(long)]
  pub no_redact: bool,

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
    None => build_request_body(endpoint, &model, args.message.as_deref().unwrap_or(""), args.stream),
  };
  let body_bytes = Bytes::from(serde_json::to_vec(&body_value)?);
  let headers = build_inbound_headers(&args.headers)?;

  if args.no_redact {
    eprintln!(
      "warning: --no-redact is set; outbound + upstream headers will be printed verbatim (including authorization, cookies, api keys). Do not paste this output into bug reports."
    );
  }

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
  let extract = Arc::new(DefaultExtract);
  let resolve = Arc::new(PoolResolve::new(selector));
  let build_headers = Arc::new(PersonaBuildHeaders::with_opencode_default());
  let convert_request = Arc::new(DefaultConvertRequest);

  let (profile, options) = if args.dry_run {
    let profile = Arc::new(Profile::without_send(
      "gateway-smoke",
      extract,
      resolve,
      build_headers,
      convert_request,
    ));
    (profile, RunnerOptions::stop_after(Stage::ConvertRequest))
  } else {
    let profile = Arc::new(Profile::full(
      "gateway-smoke",
      extract,
      resolve,
      build_headers,
      convert_request,
      Arc::new(DefaultSend::new(state.http.clone())),
      Arc::new(DefaultConvertResponse::new()),
    ));
    (profile, RunnerOptions::default())
  };
  let runner = PipelineRunner::with_options(profile, bus.clone(), options);

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
    print_failure_outcome(&outcome, args.format, !args.no_redact)?;
    let err = outcome
      .error
      .as_ref()
      .map(|e| format!("{}: {}", e.stage, e.message))
      .unwrap_or_else(|| "pipeline failed (no error attached)".into());
    anyhow::bail!("pipeline failed: {err}");
  }

  if args.dry_run {
    print_dry_run_outcome(&outcome, args.format, !args.no_redact)?;
  } else {
    print_live_outcome(outcome, args.format, !args.no_redact).await?;
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
    EventPayload::Known(StageEvent::Extract(e)) => {
      let cid = e.client_id.as_ref().map(|c| c.as_str()).unwrap_or("(none)");
      println!(
        "[extract]          model={} stream={} client_id={cid}",
        e.model, e.stream
      );
    }
    EventPayload::Known(StageEvent::Resolve(r)) => {
      let cid = r.client_id.as_ref().map(|c| c.as_str()).unwrap_or("(none)");
      println!(
        "[resolve]          model={} -> {} account={} provider={} upstream_endpoint={} client_id={cid}",
        r.model, r.upstream_model, r.account_id, r.provider_id, r.upstream_endpoint
      );
    }
    EventPayload::Known(StageEvent::BuildHeaders(_)) => {
      println!("[build_headers]    ok");
    }
    EventPayload::Known(StageEvent::ConvertRequest(_)) => {
      println!("[convert_request]  ok");
    }
    EventPayload::Known(StageEvent::Send(_)) => {
      println!("[send]             ok");
    }
    EventPayload::Known(StageEvent::ConvertResponse(_)) => {
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

fn print_dry_run_outcome(outcome: &llm_router2::PipelineOutcome, format: OutputFormat, redact: bool) -> Result<()> {
  let resolved = outcome.resolved.as_ref();
  let headers = outcome.built_headers.as_ref();
  let converted = outcome.converted_request.as_ref();

  match format {
    OutputFormat::Json => {
      let headers_json = headers
        .map(|h| headers_json_value(&h.headers, redact))
        .unwrap_or(Value::Null);
      let body_json = converted.map(|c| (*c.upstream_body).clone()).unwrap_or(Value::Null);
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
          println!("  {}: {}", name.as_str(), redact_header(name.as_str(), v, redact));
        }
      }
      if let Some(c) = converted {
        if let Some(enc) = c.content_encoding {
          println!("content-encoding: {}", enc.as_str());
        }
        println!("body:");
        println!("{}", serde_json::to_string_pretty(&*c.upstream_body)?);
      }
    }
  }
  Ok(())
}

/// Render the result of a live (non-dry-run) pipeline run. For buffered
/// responses we print a JSON report (`OutputFormat::Json`) or a friendly
/// text preview; for streaming responses we forward the SSE byte chunks
/// to stdout as they arrive (curl `-N` semantics) and finish with a
/// concise summary.
async fn print_live_outcome(outcome: llm_router2::PipelineOutcome, format: OutputFormat, redact: bool) -> Result<()> {
  let PipelineOutcome {
    attempts,
    resolved,
    converted_response,
    ..
  } = outcome;
  let converted = converted_response.ok_or_else(|| anyhow!("pipeline succeeded but produced no response"))?;

  match converted {
    ConvertedResponse::Buffered {
      status,
      headers,
      body_json,
      ..
    } => match format {
      OutputFormat::Json => {
        let report = serde_json::json!({
          "dry_run": false,
          "stream": false,
          "attempts": attempts,
          "account": resolved.as_ref().map(|r| r.account_id.as_str()),
          "provider": resolved.as_ref().map(|r| r.provider_id.as_str()),
          "model": resolved.as_ref().map(|r| r.model.as_str()),
          "upstream_model": resolved.as_ref().map(|r| r.upstream_model.as_str()),
          "upstream_endpoint": resolved.as_ref().map(|r| r.upstream_endpoint.to_string()),
          "status": status,
          "headers": headers_json_value(&headers, redact),
          "body": &*body_json,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
      }
      OutputFormat::Text => {
        println!();
        println!("--- response ---");
        println!("attempts: {}", attempts);
        if let Some(r) = &resolved {
          println!("account:  {}", r.account_id);
          println!("provider: {}", r.provider_id);
          println!("model:    {} -> {}", r.model, r.upstream_model);
          println!("upstream: {}", r.upstream_endpoint);
        }
        println!("status:   {}", status);
        println!("headers:");
        for (name, value) in headers.iter() {
          let v = value.as_str();
          println!("  {}: {}", name.as_str(), redact_header(name.as_str(), v, redact));
        }
        println!("body:");
        println!("{}", serde_json::to_string_pretty(&*body_json)?);
      }
    },
    ConvertedResponse::Stream {
      status,
      headers,
      mut body,
    } => {
      // For text format, print a short header banner first so the user sees
      // metadata before the stream body. For json format we still stream
      // the raw SSE bytes (already endpoint-translated, if applicable) —
      // wrapping that in a JSON envelope would require buffering, which
      // defeats the point of streaming. Tooling that wants structured
      // output should use `--dry-run` or buffered mode.
      if matches!(format, OutputFormat::Text) {
        println!();
        println!("--- response (stream) ---");
        println!("attempts: {}", attempts);
        if let Some(r) = &resolved {
          println!("account:  {}", r.account_id);
          println!("provider: {}", r.provider_id);
          println!("model:    {} -> {}", r.model, r.upstream_model);
          println!("upstream: {}", r.upstream_endpoint);
        }
        println!("status:   {}", status);
        println!("headers:");
        for (name, value) in headers.iter() {
          let v = value.as_str();
          println!("  {}: {}", name.as_str(), redact_header(name.as_str(), v, redact));
        }
        println!("body:");
      }

      let mut stdout = tokio::io::stdout();
      let mut total_bytes: usize = 0;
      while let Some(chunk) = body.next().await {
        let bytes = chunk.map_err(|e| anyhow!("stream read failed: {e}"))?;
        total_bytes += bytes.len();
        stdout
          .write_all(&bytes)
          .await
          .map_err(|e| anyhow!("stdout write failed: {e}"))?;
        stdout.flush().await.ok();
      }
      if matches!(format, OutputFormat::Text) {
        println!();
        println!("--- end of stream ({total_bytes} bytes) ---");
      }
    }
  }
  Ok(())
}

/// Render whatever partial state the pipeline managed to accumulate before
/// failing. Mirrors `print_dry_run_outcome` shape so the user sees the same
/// fields whether the run succeeded, was dry-run, or aborted mid-stream.
fn print_failure_outcome(outcome: &llm_router2::PipelineOutcome, format: OutputFormat, redact: bool) -> Result<()> {
  let resolved = outcome.resolved.as_ref();
  let headers = outcome.built_headers.as_ref();
  let converted_req = outcome.converted_request.as_ref();
  let err = outcome.error.as_ref();

  match format {
    OutputFormat::Json => {
      let headers_json = headers
        .map(|h| headers_json_value(&h.headers, redact))
        .unwrap_or(Value::Null);
      let body_json = converted_req.map(|c| (*c.upstream_body).clone()).unwrap_or(Value::Null);
      let report = serde_json::json!({
        "success": false,
        "attempts": outcome.attempts,
        "error": err.map(|e| serde_json::json!({
          "stage": e.stage.as_str(),
          "message": &e.message,
          "recoverable": e.recoverable,
        })),
        "account": resolved.map(|r| r.account_id.as_str()),
        "provider": resolved.map(|r| r.provider_id.as_str()),
        "model": resolved.map(|r| r.model.as_str()),
        "upstream_model": resolved.map(|r| r.upstream_model.as_str()),
        "upstream_endpoint": resolved.map(|r| r.upstream_endpoint.to_string()),
        "headers": headers_json,
        "body": body_json,
        "content_encoding": converted_req.and_then(|c| c.content_encoding.map(|e| e.as_str())),
      });
      println!("{}", serde_json::to_string_pretty(&report)?);
    }
    OutputFormat::Text => {
      println!();
      println!("--- failure ---");
      println!("attempts: {}", outcome.attempts);
      if let Some(e) = err {
        println!("stage:    {}", e.stage);
        println!("recoverable: {}", e.recoverable);
        println!("message:  {}", e.message);
      }
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
          println!("  {}: {}", name.as_str(), redact_header(name.as_str(), v, redact));
        }
      }
      if let Some(c) = converted_req {
        if let Some(enc) = c.content_encoding {
          println!("content-encoding: {}", enc.as_str());
        }
        println!("body:");
        println!("{}", serde_json::to_string_pretty(&*c.upstream_body)?);
      }
    }
  }
  Ok(())
}

fn headers_json_value(headers: &llm_headers::HeaderMap, redact: bool) -> Value {
  let mut map = serde_json::Map::new();
  for (name, value) in headers.iter() {
    let v = value.as_str();
    let key = name.as_str().to_string();
    let value = Value::String(redact_header(name.as_str(), v, redact));
    match map.get_mut(&key) {
      Some(Value::Array(values)) => values.push(value),
      Some(_) => unreachable!("header json values are always arrays"),
      None => {
        map.insert(key, Value::Array(vec![value]));
      }
    }
  }
  Value::Object(map)
}

fn redact_header(name: &str, value: &str, redact: bool) -> String {
  // Preserve diagnostic sentinels: empty values and the `<missing>`
  // placeholder emitted by persona builders are not secrets, they are
  // signals that the upstream stage failed to populate something. Hiding
  // them defeats the entire point of dumping headers when debugging.
  if !redact || value.is_empty() || value == "<missing>" {
    return value.to_string();
  }
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

#[cfg(test)]
mod tests {
  use super::*;
  use llm_headers::HeaderMap;

  #[test]
  fn headers_json_value_preserves_multi_values_in_order() {
    let mut headers = HeaderMap::new();
    headers.append("set-cookie", "a=1");
    headers.append("x-test", "first");
    headers.append("set-cookie", "b=2");

    let json = headers_json_value(&headers, false);
    assert_eq!(
      json,
      serde_json::json!({
        "set-cookie": ["a=1", "b=2"],
        "x-test": ["first"],
      })
    );
  }
}
