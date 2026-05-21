//! End-to-end smoke test for the pre-Send pipeline.
//!
//! Assembles a [`Profile::without_send`] with [`DefaultExtract`], a fake
//! [`AccountSelector`], and the [`NoopBuildHeaders`]/[`NoopConvertRequest`]
//! stages (real impls land in PR2 follow-ups). Runs against a synthetic
//! [`RawInbound`] and asserts the event sequence. The pipeline halts at
//! Send via `PipelineError::stop`; subscribers fold the per-stage events
//! to reconstruct the partial outputs.
//!
//! The PR3b full-pipeline test (`full_pipeline_buffered_happy_path`)
//! additionally exercises the real `DefaultSend` + `DefaultConvertResponse`
//! against a canned `reqwest::Response`.

use async_trait::async_trait;
use bytes::Bytes;
use serde_json::Value;
use smol_str::SmolStr;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokn_accounts::AccountHandle;
use tokn_core::account::{AccountConfig, Secret};
use tokn_core::provider::{
  AuthKind, Endpoint, ModelCache, Provider, ProviderInfo, RequestCtx, Result as ProviderResult,
};
use tokn_headers::{HeaderMap, HeaderValue};
use tokn_mock_server::{MockAuthConfig, MockLlmConfig, MockLlmServer, MockRoute};
use tokn_requests::event::{EventPayload, Stage, StageEvent};
use tokn_requests::pipeline::stages::ConvertedBody;
use tokn_requests::stages::{
  AccountSelector, DefaultConvertRequest, DefaultConvertResponse, DefaultExtract, DefaultSend, NoopBuildHeaders,
  NoopConvertRequest, PersonaBuildHeaders, PoolResolve, SelectorOutcome,
};
use tokn_requests::{Event, EventBus, PipelineError, PipelineRunner, Profile, RawInbound, RetryPolicy};

/// Minimal `Provider` used only to satisfy the new typed
/// `AccountHandle` requirement on `SelectorOutcome::Selected`.
struct StubProvider {
  info: ProviderInfo,
}

#[async_trait]
impl Provider for StubProvider {
  fn id(&self) -> &str {
    &self.info.id
  }
  fn info(&self) -> &ProviderInfo {
    &self.info
  }
  async fn list_models(&self, _http: &reqwest::Client) -> ProviderResult<Value> {
    Ok(Value::Null)
  }
  async fn chat(&self, _ctx: RequestCtx<'_>) -> ProviderResult<reqwest::Response> {
    unreachable!("smoke test never reaches Send")
  }
}

fn stub_handle(provider_id: &str, account_id: &str) -> Arc<AccountHandle> {
  let info = ProviderInfo {
    id: provider_id.into(),
    aliases: &[],
    display_name: "stub",
    upstream_url: String::new(),
    auth_kind: AuthKind::StaticApiKey,
    default_models: vec![],
    default_endpoints: &[Endpoint::ChatCompletions],
    model_cache: Arc::new(ModelCache::default()),
  };
  let cfg = AccountConfig {
    id: account_id.to_string(),
    provider: provider_id.to_string(),
    enabled: true,
    tier: Default::default(),
    tags: Vec::new(),
    label: None,
    base_url: None,
    headers: Default::default(),
    auth_type: None,
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    provider_account_id: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: Default::default(),
  };
  Arc::new(AccountHandle::new(Arc::new(cfg), Arc::new(StubProvider { info })))
}

struct OkSelector;

#[async_trait]
impl AccountSelector for OkSelector {
  async fn select(
    &self,
    _ctx: &tokn_requests::pipeline::ctx::PipelineCtx,
    _ex: &tokn_requests::stage_traits::Extracted,
  ) -> Result<SelectorOutcome, PipelineError> {
    Ok(SelectorOutcome::Selected {
      account_id: SmolStr::new("acct-1"),
      provider_id: SmolStr::new("zai-coding-plan"),
      upstream_endpoint: Endpoint::ChatCompletions,
      upstream_model: SmolStr::new("glm-4"),
      account_handle: stub_handle("zai-coding-plan", "acct-1"),
    })
  }
}

struct EmptySelector;

#[async_trait]
impl AccountSelector for EmptySelector {
  async fn select(
    &self,
    _ctx: &tokn_requests::pipeline::ctx::PipelineCtx,
    _ex: &tokn_requests::stage_traits::Extracted,
  ) -> Result<SelectorOutcome, PipelineError> {
    Ok(SelectorOutcome::NoAccount)
  }
}

fn capture_bus() -> (Arc<EventBus>, Arc<Mutex<Vec<Event>>>) {
  let bus = Arc::new(EventBus::new(256));
  let log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
  {
    let log = log.clone();
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
      loop {
        match rx.recv().await {
          Ok(arc) => {
            if let tokn_core::event::Event::Requests(ev) = &*arc {
              log.lock().unwrap().push(ev.clone());
            }
          }
          Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
          Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
        }
      }
    });
  }
  (bus, log)
}

fn raw_chat_with_headers(model: &str, header_pairs: &[(&str, &str)]) -> RawInbound {
  let body = serde_json::json!({"model": model, "messages": []});
  let decoded = Bytes::from(serde_json::to_vec(&body).unwrap());
  let mut headers = HeaderMap::new();
  for (name, value) in header_pairs {
    headers.insert(*name, HeaderValue::from_string((*value).to_string()));
  }
  RawInbound {
    endpoint: Endpoint::ChatCompletions,
    headers,
    raw_body: decoded.clone(),
    decoded_body: decoded,
    body_json: body,
    request_id: Some(SmolStr::new("req-smoke-1")),
  }
}

fn raw_chat(model: &str) -> RawInbound {
  raw_chat_with_headers(model, &[("x-behave-as", "codex")])
}

async fn drain_until_completed(log: &Arc<Mutex<Vec<Event>>>) -> std::sync::MutexGuard<'_, Vec<Event>> {
  // Subscribers run on a spawned tokio task; yield until a `Completed`
  // event is observed (every pipeline run ends with one).
  for _ in 0..1000 {
    {
      let guard = log.lock().unwrap();
      let done = guard
        .iter()
        .any(|e| matches!(&e.payload, EventPayload::Stage(StageEvent::Completed { .. })));
      if done {
        return guard;
      }
    }
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
  }
  panic!("timed out waiting for Completed event");
}

async fn drain_until_completed_attempts(
  log: &Arc<Mutex<Vec<Event>>>,
  expected_attempts: u32,
) -> std::sync::MutexGuard<'_, Vec<Event>> {
  for _ in 0..1000 {
    {
      let guard = log.lock().unwrap();
      let done = guard.iter().any(|e| {
        matches!(
          &e.payload,
          EventPayload::Stage(StageEvent::Completed {
            attempts,
            ..
          }) if *attempts == expected_attempts
        )
      });
      if done {
        return guard;
      }
    }
    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
  }
  panic!("timed out waiting for Completed event with attempts={expected_attempts}");
}

fn known_kinds(events: &[Event]) -> Vec<&'static str> {
  events
    .iter()
    .map(|e| match &e.payload {
      EventPayload::Stage(k) => match k {
        StageEvent::Started { .. } => "started",
        StageEvent::Extract(_) => "extract",
        StageEvent::Resolve(_) => "resolve",
        StageEvent::BuildHeaders(_) => "build_headers",
        StageEvent::ConvertRequest(_) => "convert_request",
        StageEvent::Send(_) => "send",
        StageEvent::ConvertResponse(_) => "convert_response",
        StageEvent::Error { .. } => "error",
        StageEvent::Completed { .. } => "completed",
      },
      EventPayload::Record(_) => "record",
      EventPayload::Custom(c) => c.kind,
    })
    .collect()
}

#[tokio::test]
async fn pre_send_happy_path_emits_expected_event_sequence() {
  let (bus, log) = capture_bus();
  let profile = Arc::new(Profile::without_send(
    "smoke",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(Arc::new(OkSelector))),
    Arc::new(NoopBuildHeaders),
    Arc::new(NoopConvertRequest),
  ));
  let runner = PipelineRunner::new(profile, bus);

  // `without_send` halts at the Send stage via `PipelineError::stop`.
  let err = runner
    .run(raw_chat("input-model"))
    .await
    .expect_err("without_send must return Err(stop) at Send");
  assert!(err.stop, "expected a stop error, got {err:?}");
  assert_eq!(err.stage, Stage::Send);

  let events = drain_until_completed(&log).await;
  let kinds = known_kinds(&events);
  assert_eq!(
    kinds,
    [
      "started",
      "extract",
      "resolve",
      "build_headers",
      "convert_request",
      "error",
      "completed",
    ]
  );

  // The Error event must carry the stop flag verbatim so subscribers can
  // distinguish a deliberate stop from a real failure.
  let (err_stage, stop_flag) = events
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Error { stage, stop, .. }) => Some((*stage, *stop)),
      _ => None,
    })
    .expect("Error event must be present");
  assert_eq!(err_stage, Stage::Send);
  assert!(stop_flag);

  // Spot-check the Resolve event carries the upstream model and provider.
  let resolve = events.iter().find_map(|e| match &e.payload {
    EventPayload::Stage(StageEvent::Resolve(r)) => Some((
      r.upstream_model.clone(),
      r.provider_id.clone(),
      r.account_id.clone(),
      r.client_id.clone(),
    )),
    _ => None,
  });
  let (upstream, provider, account, client) = resolve.expect("Resolve event must be present");
  assert_eq!(upstream, "glm-4");
  assert_eq!(provider, "zai-coding-plan");
  assert_eq!(account, "acct-1");
  assert_eq!(client.as_ref().map(|c| c.as_str().to_string()), Some("codex".into()));
}

#[tokio::test]
async fn pre_send_no_account_emits_error_then_completed_failure() {
  let (bus, log) = capture_bus();
  let profile = Arc::new(Profile::without_send(
    "smoke",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(Arc::new(EmptySelector))),
    Arc::new(NoopBuildHeaders),
    Arc::new(NoopConvertRequest),
  ));
  let runner = PipelineRunner::new(profile, bus);

  let err = runner
    .run(raw_chat("nope"))
    .await
    .expect_err("empty selector must fail at Resolve");
  assert_eq!(err.stage, Stage::Resolve);
  assert!(!err.recoverable);
  assert!(!err.stop, "no-account is a real failure, not a stop");

  let events = drain_until_completed(&log).await;
  let kinds = known_kinds(&events);
  assert_eq!(kinds, ["started", "extract", "error", "completed"]);

  // The error event must mirror the returned error's stage / flags.
  let (stage, recoverable, stop) = events
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Error {
        stage,
        recoverable,
        stop,
        ..
      }) => Some((*stage, *recoverable, *stop)),
      _ => None,
    })
    .expect("Error event must be present");
  assert_eq!(stage, Stage::Resolve);
  assert!(!recoverable);
  assert!(!stop);

  // The terminal Completed event must report success=false.
  let success = events.iter().find_map(|e| match &e.payload {
    EventPayload::Stage(StageEvent::Completed { success, .. }) => Some(*success),
    _ => None,
  });
  assert_eq!(success, Some(false));
}

// ---------- PR3b: full-pipeline (all six default stages) ----------

/// Stub provider whose `chat` returns a single pre-armed `reqwest::Response`.
/// The trait method takes `&self`, so the response sits behind a `Mutex`.
struct RespondingProvider {
  info: ProviderInfo,
  resp: Mutex<Option<reqwest::Response>>,
}

#[async_trait]
impl Provider for RespondingProvider {
  fn id(&self) -> &str {
    &self.info.id
  }
  fn info(&self) -> &ProviderInfo {
    &self.info
  }
  async fn list_models(&self, _http: &reqwest::Client) -> ProviderResult<Value> {
    Ok(Value::Null)
  }
  async fn chat(&self, _ctx: RequestCtx<'_>) -> ProviderResult<reqwest::Response> {
    Ok(
      self
        .resp
        .lock()
        .unwrap()
        .take()
        .expect("RespondingProvider::chat: no canned response armed"),
    )
  }
}

fn responding_handle(provider_id: &str, account_id: &str, resp: reqwest::Response) -> Arc<AccountHandle> {
  let info = ProviderInfo {
    id: provider_id.into(),
    aliases: &[],
    display_name: "responding",
    upstream_url: String::new(),
    auth_kind: AuthKind::StaticApiKey,
    default_models: vec![],
    default_endpoints: &[Endpoint::ChatCompletions],
    model_cache: Arc::new(ModelCache::default()),
  };
  let cfg = AccountConfig {
    id: account_id.to_string(),
    provider: provider_id.to_string(),
    enabled: true,
    tier: Default::default(),
    tags: Vec::new(),
    label: None,
    base_url: None,
    headers: Default::default(),
    auth_type: None,
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    provider_account_id: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: Default::default(),
  };
  let provider = RespondingProvider {
    info,
    resp: Mutex::new(Some(resp)),
  };
  Arc::new(AccountHandle::new(Arc::new(cfg), Arc::new(provider)))
}

struct CannedSelector {
  handle: Arc<AccountHandle>,
  provider_id: SmolStr,
  upstream_endpoint: Endpoint,
  upstream_model: SmolStr,
}

#[async_trait]
impl AccountSelector for CannedSelector {
  async fn select(
    &self,
    _ctx: &tokn_requests::pipeline::ctx::PipelineCtx,
    _ex: &tokn_requests::stage_traits::Extracted,
  ) -> Result<SelectorOutcome, PipelineError> {
    Ok(SelectorOutcome::Selected {
      account_id: SmolStr::new(self.handle.config.load().id.clone()),
      provider_id: self.provider_id.clone(),
      upstream_endpoint: self.upstream_endpoint,
      upstream_model: self.upstream_model.clone(),
      account_handle: self.handle.clone(),
    })
  }
}

fn openai_handle(account_id: &str, base_url: &str, api_key: &str) -> Arc<AccountHandle> {
  let config = Arc::new(AccountConfig {
    id: account_id.to_string(),
    provider: "openai".to_string(),
    enabled: true,
    tier: Default::default(),
    tags: Vec::new(),
    label: None,
    base_url: Some(base_url.to_string()),
    headers: Default::default(),
    auth_type: None,
    username: None,
    api_key: Some(Secret::new(api_key.to_string())),
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    provider_account_id: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: Default::default(),
  });
  let provider = tokn_accounts::registry::build_for_account(config.clone()).expect("openai provider should build");
  Arc::new(AccountHandle::new(config, provider))
}

fn ok_response(status: u16, body: &'static str) -> reqwest::Response {
  let resp = http::Response::builder()
    .status(status)
    .header("content-type", "application/json")
    .body(body)
    .unwrap();
  reqwest::Response::from(resp)
}

#[tokio::test]
async fn full_pipeline_buffered_happy_path() {
  let (bus, log) = capture_bus();

  // Canned upstream payload: a tiny chat-completions response.
  let resp = ok_response(
    200,
    r#"{"id":"resp-1","choices":[{"message":{"role":"assistant","content":"hi"}}]}"#,
  );
  let handle = responding_handle("zai-coding-plan", "acct-1", resp);
  let selector = Arc::new(CannedSelector {
    handle,
    provider_id: SmolStr::new("zai-coding-plan"),
    upstream_endpoint: Endpoint::ChatCompletions,
    upstream_model: SmolStr::new("glm-4"),
  });

  let profile = Arc::new(Profile::full(
    "smoke-full",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(reqwest::Client::new())),
    Arc::new(DefaultConvertResponse::new()),
  ));
  let runner = PipelineRunner::new(profile, bus);

  let converted = runner
    .run(raw_chat("glm-4"))
    .await
    .expect("happy-path pipeline must succeed");

  // Full happy-path event sequence: every stage fires exactly once,
  // followed by the terminal Completed marker. The `record` entries are
  // wire-truth captures — the mock provider bypasses
  // `tokn_core::util::http::send`, so `Record::UpstreamReq` is skipped
  // and only `Record::UpstreamResp` (from Send) and
  // `Record::UpstreamBody` (from ConvertResponse) appear.
  let events = drain_until_completed(&log).await;
  let kinds = known_kinds(&events);
  assert_eq!(
    kinds,
    [
      "started",
      "extract",
      "resolve",
      "build_headers",
      "convert_request",
      "record",
      "send",
      "record",
      "convert_response",
      "record",
      "completed",
    ]
  );

  // Completed must report success=true with attempts=1.
  let (success, attempts) = events
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Completed { success, attempts }) => Some((*success, *attempts)),
      _ => None,
    })
    .expect("Completed event must be present");
  assert!(success);
  assert_eq!(attempts, 1);

  // The converted response must round-trip the canned upstream payload.
  assert_eq!(converted.status, 200);
  match converted.body {
    ConvertedBody::Buffered { body_json, .. } => {
      let body_json = body_json.unwrap();
      assert_eq!(body_json["id"], "resp-1");
      assert_eq!(body_json["choices"][0]["message"]["content"], "hi");
    }
    other => panic!("expected Buffered, got {other:?}"),
  }
}

#[tokio::test]
async fn full_pipeline_openai_mock_server_uses_opencode_headers() {
  let (bus, _log) = capture_bus();
  let server = MockLlmServer::start(
    MockLlmConfig {
      routes: vec![MockRoute::chat_completions()],
      ..Default::default()
    }
    .with_auth(MockAuthConfig::bearer(["sk-test"])),
  )
  .await;
  let handle = openai_handle("acct-openai", server.base_url(), "sk-test");
  let selector = Arc::new(CannedSelector {
    handle,
    provider_id: SmolStr::new("openai"),
    upstream_endpoint: Endpoint::ChatCompletions,
    upstream_model: SmolStr::new("gpt-4o-mini"),
  });

  let profile = Arc::new(Profile::full(
    "smoke-openai-opencode",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(reqwest::Client::new())),
    Arc::new(DefaultConvertResponse::new()),
  ));
  let runner = PipelineRunner::new(profile, bus);

  let converted = runner
    .run(raw_chat_with_headers(
      "gpt-4o-mini",
      &[
        ("x-behave-as", "opencode"),
        ("x-session-id", "sess-openai-1"),
        ("x-opencode-project", "/tmp/demo"),
      ],
    ))
    .await
    .expect("openai mock-server pipeline must succeed");

  assert_eq!(converted.status, 200);
  match converted.body {
    ConvertedBody::Buffered { body_json, .. } => {
      let body_json = body_json.unwrap();
      assert_eq!(body_json["choices"][0]["message"]["content"], "mock response");
    }
    other => panic!("expected Buffered, got {other:?}"),
  }

  let captured = server.last_request().expect("captured openai request");
  assert_eq!(captured.method, reqwest::Method::POST);
  assert_eq!(captured.path, "/chat/completions");
  assert_eq!(captured.header("authorization"), Some("Bearer sk-test"));
  assert_eq!(
    captured.header("user-agent"),
    Some("opencode/1.14.28 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13")
  );
  assert_eq!(captured.header("x-session-affinity"), Some("sess-openai-1"));
  assert_eq!(
    captured
      .header("content-type")
      .map(|value| value.split(';').next().unwrap_or(value)),
    Some("application/json")
  );

  let payload: Value = serde_json::from_slice(&captured.body).unwrap();
  assert_eq!(payload["model"], "gpt-4o-mini");
  assert_eq!(payload["messages"], serde_json::json!([]));
}

// ---------- PR3c: failure preserves partial outcome ----------

/// Provider whose `chat` always returns an upstream 401. Used to assert
/// that a Send-stage failure short-circuits the pipeline with the
/// matching `PipelineError`, that the terminal Error + Completed events
/// report the failing stage, and that subscribers can fold the prior
/// per-stage events to recover Resolve / BuildHeaders / ConvertRequest
/// outputs (the runner no longer carries partial state in its return
/// value).
struct FailingProvider {
  info: ProviderInfo,
}

#[async_trait]
impl Provider for FailingProvider {
  fn id(&self) -> &str {
    &self.info.id
  }
  fn info(&self) -> &ProviderInfo {
    &self.info
  }
  async fn list_models(&self, _http: &reqwest::Client) -> ProviderResult<Value> {
    Ok(Value::Null)
  }
  async fn chat(&self, _ctx: RequestCtx<'_>) -> ProviderResult<reqwest::Response> {
    let resp = http::Response::builder()
      .status(401)
      .header("content-type", "application/json")
      .body(r#"{"error":"unauthorized"}"#)
      .unwrap();
    Ok(reqwest::Response::from(resp))
  }
}

fn failing_handle(provider_id: &str, account_id: &str) -> Arc<AccountHandle> {
  let info = ProviderInfo {
    id: provider_id.into(),
    aliases: &[],
    display_name: "failing",
    upstream_url: String::new(),
    auth_kind: AuthKind::StaticApiKey,
    default_models: vec![],
    default_endpoints: &[Endpoint::ChatCompletions],
    model_cache: Arc::new(ModelCache::default()),
  };
  let cfg = AccountConfig {
    id: account_id.to_string(),
    provider: provider_id.to_string(),
    enabled: true,
    tier: Default::default(),
    tags: Vec::new(),
    label: None,
    base_url: None,
    headers: Default::default(),
    auth_type: None,
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    provider_account_id: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: Default::default(),
  };
  let provider = FailingProvider { info };
  Arc::new(AccountHandle::new(Arc::new(cfg), Arc::new(provider)))
}

enum ScriptedResponse {
  Http { status: u16, body: &'static str },
}

struct SequencedProvider {
  info: ProviderInfo,
  responses: Mutex<VecDeque<ScriptedResponse>>,
  calls: AtomicUsize,
}

#[async_trait]
impl Provider for SequencedProvider {
  fn id(&self) -> &str {
    &self.info.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  async fn list_models(&self, _http: &reqwest::Client) -> ProviderResult<Value> {
    Ok(Value::Null)
  }

  async fn chat(&self, _ctx: RequestCtx<'_>) -> ProviderResult<reqwest::Response> {
    self.calls.fetch_add(1, Ordering::Relaxed);
    let next = self
      .responses
      .lock()
      .unwrap()
      .pop_front()
      .expect("scripted provider should have a queued response");
    match next {
      ScriptedResponse::Http { status, body } => Ok(ok_response(status, body)),
    }
  }
}

fn sequenced_handle(provider_id: &str, account_id: &str, responses: Vec<ScriptedResponse>) -> Arc<AccountHandle> {
  let info = ProviderInfo {
    id: provider_id.into(),
    aliases: &[],
    display_name: "sequenced",
    upstream_url: String::new(),
    auth_kind: AuthKind::StaticApiKey,
    default_models: vec![],
    default_endpoints: &[Endpoint::ChatCompletions],
    model_cache: Arc::new(ModelCache::default()),
  };
  let cfg = AccountConfig {
    id: account_id.to_string(),
    provider: provider_id.to_string(),
    enabled: true,
    tier: Default::default(),
    tags: Vec::new(),
    label: None,
    base_url: None,
    headers: Default::default(),
    auth_type: None,
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    provider_account_id: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: Default::default(),
  };
  let provider = SequencedProvider {
    info,
    responses: Mutex::new(responses.into()),
    calls: AtomicUsize::new(0),
  };
  Arc::new(AccountHandle::new(Arc::new(cfg), Arc::new(provider)))
}

#[tokio::test]
async fn pipeline_send_failure_preserves_partial_outcome() {
  let (bus, log) = capture_bus();

  let handle = failing_handle("zai-coding-plan", "acct-1");
  let selector = Arc::new(CannedSelector {
    handle,
    provider_id: SmolStr::new("zai-coding-plan"),
    upstream_endpoint: Endpoint::ChatCompletions,
    upstream_model: SmolStr::new("glm-4"),
  });

  let profile = Arc::new(Profile::full(
    "smoke-fail",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(reqwest::Client::new())),
    Arc::new(DefaultConvertResponse::new()),
  ));
  let runner = PipelineRunner::new(profile, bus);

  let err = runner
    .run(raw_chat("glm-4"))
    .await
    .expect_err("upstream 401 must surface as Err");

  // The pipeline failed at Send.
  assert_eq!(err.stage, Stage::Send);
  assert!(
    err.message().contains("401"),
    "error message should mention upstream status: {}",
    err.message()
  );
  assert!(!err.stop, "401 is a real failure, not a stop");

  // Subscribers fold prior per-stage events to recover the partial state
  // — the runner does not carry it on the return value any more. Every
  // earlier stage must have fired exactly once before the Error. The
  // single `record` is `Record::UpstreamResp` (status+headers from the
  // 401) — the mock bypasses `util::http::send` so `Record::UpstreamReq`
  // is skipped, and the error returns before ConvertResponse runs so
  // there is no `Record::UpstreamBody` either.
  let events = drain_until_completed(&log).await;
  let kinds = known_kinds(&events);
  assert_eq!(
    kinds,
    [
      "started",
      "extract",
      "resolve",
      "build_headers",
      "convert_request",
      "record",
      "error",
      "completed",
    ]
  );

  // Spot-check that each pre-Send stage's event carries its full output.
  let resolved_seen = events
    .iter()
    .any(|e| matches!(&e.payload, EventPayload::Stage(StageEvent::Resolve(_))));
  let headers_seen = events
    .iter()
    .any(|e| matches!(&e.payload, EventPayload::Stage(StageEvent::BuildHeaders(_))));
  let req_seen = events
    .iter()
    .any(|e| matches!(&e.payload, EventPayload::Stage(StageEvent::ConvertRequest(_))));
  assert!(resolved_seen, "Resolve event must precede the Send failure");
  assert!(headers_seen, "BuildHeaders event must precede the Send failure");
  assert!(req_seen, "ConvertRequest event must precede the Send failure");

  // The terminal events mirror the failure: Error tags the originating
  // stage with stop=false; Completed reports success=false.
  let (err_stage, err_stop) = events
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Error { stage, stop, .. }) => Some((*stage, *stop)),
      _ => None,
    })
    .expect("Error event must be present");
  assert_eq!(err_stage, Stage::Send);
  assert!(!err_stop);
  let completed_success = events.iter().find_map(|e| match &e.payload {
    EventPayload::Stage(StageEvent::Completed { success, .. }) => Some(*success),
    _ => None,
  });
  assert_eq!(completed_success, Some(false));
}

#[tokio::test]
async fn pipeline_retries_recoverable_send_failures_and_succeeds() {
  let (bus, log) = capture_bus();

  let handle = sequenced_handle(
    "zai-coding-plan",
    "acct-1",
    vec![
      ScriptedResponse::Http {
        status: 503,
        body: "retry me",
      },
      ScriptedResponse::Http {
        status: 200,
        body: r#"{"id":"resp-retry","choices":[{"message":{"role":"assistant","content":"ok"}}]}"#,
      },
    ],
  );
  let selector = Arc::new(CannedSelector {
    handle,
    provider_id: SmolStr::new("zai-coding-plan"),
    upstream_endpoint: Endpoint::ChatCompletions,
    upstream_model: SmolStr::new("glm-4"),
  });

  let profile = Arc::new(Profile::full(
    "smoke-retry-success",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(reqwest::Client::new())),
    Arc::new(DefaultConvertResponse::new()),
  ));
  let runner = PipelineRunner::new_with_retry(profile, bus, RetryPolicy::new(2, Duration::from_millis(1)));

  let converted = runner
    .run(raw_chat("glm-4"))
    .await
    .expect("second attempt should succeed");
  let events = drain_until_completed_attempts(&log, 2).await;
  let error_attempts: Vec<u32> = events
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Error {
        stage: Stage::Send,
        recoverable,
        ..
      }) if *recoverable => Some(e.attempt),
      _ => None,
    })
    .collect();
  assert_eq!(error_attempts, vec![0]);

  let completed = events
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Completed { success, attempts }) => Some((e.attempt, *success, *attempts)),
      _ => None,
    })
    .collect::<Vec<_>>();
  let started_attempts: Vec<u32> = events
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Started { .. }) => Some(e.attempt),
      _ => None,
    })
    .collect();
  assert_eq!(started_attempts, vec![0, 1]);
  assert_eq!(completed, vec![(0, false, 1), (1, true, 2)]);

  match converted.body {
    ConvertedBody::Buffered { body_json, .. } => {
      let body_json = body_json.unwrap();
      assert_eq!(body_json["id"], "resp-retry");
    }
    other => panic!("expected Buffered, got {other:?}"),
  }
}

#[tokio::test]
async fn pipeline_stops_after_retry_budget_exhausted() {
  let (bus, log) = capture_bus();

  let handle = sequenced_handle(
    "zai-coding-plan",
    "acct-1",
    vec![
      ScriptedResponse::Http {
        status: 503,
        body: "one",
      },
      ScriptedResponse::Http {
        status: 503,
        body: "two",
      },
      ScriptedResponse::Http {
        status: 503,
        body: "three",
      },
    ],
  );
  let selector = Arc::new(CannedSelector {
    handle,
    provider_id: SmolStr::new("zai-coding-plan"),
    upstream_endpoint: Endpoint::ChatCompletions,
    upstream_model: SmolStr::new("glm-4"),
  });

  let profile = Arc::new(Profile::full(
    "smoke-retry-exhausted",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(reqwest::Client::new())),
    Arc::new(DefaultConvertResponse::new()),
  ));
  let runner = PipelineRunner::new_with_retry(profile, bus, RetryPolicy::new(2, Duration::from_millis(1)));

  let err = runner
    .run(raw_chat("glm-4"))
    .await
    .expect_err("retry budget should exhaust");
  assert_eq!(err.stage, Stage::Send);
  assert!(err.recoverable);

  let events = drain_until_completed_attempts(&log, 3).await;
  let completed = events
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Completed { success, attempts }) => Some((e.attempt, *success, *attempts)),
      _ => None,
    })
    .collect::<Vec<_>>();
  let started_attempts: Vec<u32> = events
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Started { .. }) => Some(e.attempt),
      _ => None,
    })
    .collect();
  assert_eq!(started_attempts, vec![0, 1, 2]);
  assert_eq!(completed, vec![(0, false, 1), (1, false, 2), (2, false, 3)]);
}

#[tokio::test]
async fn pipeline_does_not_retry_permanent_send_failures() {
  let (bus, log) = capture_bus();

  let handle = sequenced_handle(
    "zai-coding-plan",
    "acct-1",
    vec![ScriptedResponse::Http {
      status: 401,
      body: "nope",
    }],
  );
  let selector = Arc::new(CannedSelector {
    handle,
    provider_id: SmolStr::new("zai-coding-plan"),
    upstream_endpoint: Endpoint::ChatCompletions,
    upstream_model: SmolStr::new("glm-4"),
  });

  let profile = Arc::new(Profile::full(
    "smoke-retry-permanent",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PersonaBuildHeaders::with_opencode_default()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(reqwest::Client::new())),
    Arc::new(DefaultConvertResponse::new()),
  ));
  let runner = PipelineRunner::new_with_retry(profile, bus, RetryPolicy::new(2, Duration::from_millis(1)));

  let err = runner
    .run(raw_chat("glm-4"))
    .await
    .expect_err("401 should remain permanent");
  assert_eq!(err.stage, Stage::Send);
  assert!(!err.recoverable);

  let events = drain_until_completed(&log).await;
  let started_attempts: Vec<u32> = events
    .iter()
    .filter_map(|e| match &e.payload {
      EventPayload::Stage(StageEvent::Started { .. }) => Some(e.attempt),
      _ => None,
    })
    .collect();
  assert_eq!(started_attempts, vec![0]);
}
