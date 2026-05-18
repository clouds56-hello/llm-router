//! End-to-end smoke test for the pre-Send pipeline.
//!
//! Assembles a [`Profile::without_send`] with [`DefaultExtract`], a fake
//! [`AccountSelector`], and the [`NoopBuildHeaders`]/[`NoopConvertRequest`]
//! stages (real impls land in PR2 follow-ups). Runs against a synthetic
//! [`RawInbound`] and asserts the event sequence.
//!
//! The PR3b full-pipeline test (`full_pipeline_buffered_happy_path`)
//! additionally exercises the real `DefaultSend` + `DefaultConvertResponse`
//! against a canned `reqwest::Response`.

use async_trait::async_trait;
use bytes::Bytes;
use llm_accounts::AccountHandle;
use llm_core::account::AccountConfig;
use llm_core::provider::{
  AuthKind, Endpoint, ModelCache, Provider, ProviderInfo, RequestCtx, Result as ProviderResult,
};
use llm_headers::{HeaderMap, HeaderValue};
use llm_router2::event::{EventPayload, Stage, StageEvent};
use llm_router2::pipeline::stages::ConvertedResponse;
use llm_router2::stages::{
  AccountSelector, DefaultConvertRequest, DefaultConvertResponse, DefaultExtract, DefaultSend, NoopBuildHeaders,
  NoopConvertRequest, PersonaBuildHeaders, PoolResolve, SelectorOutcome,
};
use llm_router2::{Event, EventBus, PipelineError, PipelineRunner, Profile, RawInbound, RunnerOptions};
use serde_json::Value;
use smol_str::SmolStr;
use std::sync::{Arc, Mutex};

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
    _ctx: &llm_router2::pipeline::ctx::PipelineCtx,
    _ex: &llm_router2::stage_traits::Extracted,
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
    _ctx: &llm_router2::pipeline::ctx::PipelineCtx,
    _ex: &llm_router2::stage_traits::Extracted,
  ) -> Result<SelectorOutcome, PipelineError> {
    Ok(SelectorOutcome::NoAccount)
  }
}

fn capture_bus() -> (Arc<EventBus>, Arc<Mutex<Vec<Event>>>) {
  let bus = Arc::new(EventBus::new());
  let log: Arc<Mutex<Vec<Event>>> = Arc::new(Mutex::new(Vec::new()));
  {
    let log = log.clone();
    bus.subscribe(move |ev| log.lock().unwrap().push(ev.clone()));
  }
  (bus, log)
}

fn raw_chat(model: &str) -> RawInbound {
  let body = serde_json::json!({"model": model, "messages": []});
  let decoded = Bytes::from(serde_json::to_vec(&body).unwrap());
  let mut headers = HeaderMap::new();
  headers.insert("x-behave-as", HeaderValue::from_static("codex"));
  RawInbound {
    endpoint: Endpoint::ChatCompletions,
    headers,
    raw_body: decoded.clone(),
    decoded_body: decoded,
    body_json: body,
    request_id: Some(SmolStr::new("req-smoke-1")),
  }
}

fn known_kinds(events: &[Event]) -> Vec<&'static str> {
  events
    .iter()
    .map(|e| match &e.payload {
      EventPayload::Known(k) => match k {
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
  let runner = PipelineRunner::with_options(profile, bus, RunnerOptions::stop_after(Stage::ConvertRequest));

  let outcome = runner.run(raw_chat("input-model")).await;
  assert!(outcome.success, "outcome: {outcome:?}");
  assert_eq!(outcome.attempts, 1);

  let events = log.lock().unwrap();
  let kinds = known_kinds(&events);
  assert_eq!(
    kinds,
    [
      "started",
      "extract",
      "resolve",
      "build_headers",
      "convert_request",
      "completed"
    ]
  );

  // Spot-check the Resolve event carries the upstream model and provider.
  let resolve = events.iter().find_map(|e| match &e.payload {
    EventPayload::Known(StageEvent::Resolve(r)) => Some((
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

  let outcome = runner.run(raw_chat("nope")).await;
  assert!(!outcome.success);
  let err = outcome.error.expect("failure outcome must carry the underlying error");
  assert_eq!(err.stage, Stage::Resolve);
  assert!(!err.recoverable);

  let events = log.lock().unwrap();
  let kinds = known_kinds(&events);
  assert_eq!(kinds, ["started", "extract", "error", "completed"]);

  // The error event must carry the same stage + recoverable flag as the outcome.
  let (stage, recoverable) = events
    .iter()
    .find_map(|e| match &e.payload {
      EventPayload::Known(StageEvent::Error { stage, recoverable, .. }) => Some((*stage, *recoverable)),
      _ => None,
    })
    .expect("Error event must be present");
  assert_eq!(stage, Stage::Resolve);
  assert!(!recoverable);

  // The terminal Completed event must report success=false.
  let success = events.iter().find_map(|e| match &e.payload {
    EventPayload::Known(StageEvent::Completed { success, .. }) => Some(*success),
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
}

#[async_trait]
impl AccountSelector for CannedSelector {
  async fn select(
    &self,
    _ctx: &llm_router2::pipeline::ctx::PipelineCtx,
    _ex: &llm_router2::stage_traits::Extracted,
  ) -> Result<SelectorOutcome, PipelineError> {
    Ok(SelectorOutcome::Selected {
      account_id: SmolStr::new(self.handle.config.load().id.clone()),
      provider_id: SmolStr::new("zai-coding-plan"),
      upstream_endpoint: Endpoint::ChatCompletions,
      upstream_model: SmolStr::new("glm-4"),
      account_handle: self.handle.clone(),
    })
  }
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
  let selector = Arc::new(CannedSelector { handle });

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

  let outcome = runner.run(raw_chat("glm-4")).await;
  assert!(outcome.success, "outcome: {outcome:?}");
  assert_eq!(outcome.attempts, 1);

  // Full happy-path event sequence: every stage fires exactly once,
  // followed by the terminal Completed marker.
  let events = log.lock().unwrap();
  let kinds = known_kinds(&events);
  assert_eq!(
    kinds,
    [
      "started",
      "extract",
      "resolve",
      "build_headers",
      "convert_request",
      "send",
      "convert_response",
      "completed",
    ]
  );

  // The converted response must round-trip the canned upstream payload.
  let converted = outcome
    .converted_response
    .as_ref()
    .expect("converted_response must be present on success");
  match converted {
    ConvertedResponse::Buffered { status, body_json, .. } => {
      assert_eq!(*status, 200);
      assert_eq!(body_json["id"], "resp-1");
      assert_eq!(body_json["choices"][0]["message"]["content"], "hi");
    }
    other => panic!("expected Buffered, got {other:?}"),
  }
}

// ---------- PR3c: failure preserves partial outcome ----------

/// Provider whose `chat` always returns an upstream 401. Used to assert
/// that a Send-stage failure preserves the resolved account, built
/// headers, and converted request body on the returned `PipelineOutcome`,
/// and that the terminal Error + Completed events report the failing
/// stage without re-embedding the partial state (callers read it from
/// the returned outcome).
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

#[tokio::test]
async fn pipeline_send_failure_preserves_partial_outcome() {
  let (bus, log) = capture_bus();

  let handle = failing_handle("zai-coding-plan", "acct-1");
  let selector = Arc::new(CannedSelector { handle });

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

  let outcome = runner.run(raw_chat("glm-4")).await;

  // The pipeline failed at Send — but every earlier stage's output must
  // survive on the returned outcome so the CLI / caller can render
  // diagnostic context.
  assert!(!outcome.success, "expected failure, got {outcome:?}");
  let err = outcome.error.as_ref().expect("failure must carry error");
  assert_eq!(err.stage, Stage::Send);
  assert!(
    err.message.contains("401"),
    "error message should mention upstream status: {}",
    err.message
  );
  assert!(outcome.resolved.is_some(), "Resolve output must survive Send failure");
  assert!(
    outcome.built_headers.is_some(),
    "BuildHeaders output must survive Send failure"
  );
  assert!(
    outcome.converted_request.is_some(),
    "ConvertRequest output must survive Send failure"
  );
  assert!(
    outcome.converted_response.is_none(),
    "ConvertResponse must not run after Send failure"
  );

  // Event sequence stops at Send (no convert_response), then error +
  // completed.
  let events = log.lock().unwrap();
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

  // The terminal events mirror the failure: Error tags the originating
  // stage; Completed reports success=false. Subscribers that need the
  // partial state read it from the returned PipelineOutcome above (no
  // snapshot is embedded in the events themselves).
  let err_stage = events.iter().find_map(|e| match &e.payload {
    EventPayload::Known(StageEvent::Error { stage, .. }) => Some(*stage),
    _ => None,
  });
  assert_eq!(err_stage, Some(Stage::Send));
  let completed_success = events.iter().find_map(|e| match &e.payload {
    EventPayload::Known(StageEvent::Completed { success, .. }) => Some(*success),
    _ => None,
  });
  assert_eq!(completed_success, Some(false));
}
