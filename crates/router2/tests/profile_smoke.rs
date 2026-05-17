//! End-to-end smoke test for the pre-Send pipeline.
//!
//! Assembles a [`Profile::without_send`] with [`DefaultExtract`], a fake
//! [`AccountSelector`], and the [`NoopBuildHeaders`]/[`NoopConvertRequest`]
//! stages (real impls land in PR2 follow-ups). Runs against a synthetic
//! [`RawInbound`] and asserts the event sequence.

use bytes::Bytes;
use llm_core::provider::Endpoint;
use llm_headers::{HeaderMap, HeaderName, HeaderValue};
use llm_router2::event::{EventPayload, Stage, StageEvent};
use llm_router2::stages::{
  AccountSelector, DefaultExtract, NoopBuildHeaders, NoopConvertRequest, PoolResolve, SelectorOutcome,
};
use llm_router2::{Event, EventBus, PipelineError, PipelineRunner, Profile, RawInbound};
use smol_str::SmolStr;
use std::sync::{Arc, Mutex};

struct OkSelector;

#[async_trait::async_trait]
impl AccountSelector for OkSelector {
  async fn select(
    &self,
    _ex: &llm_router2::stage_traits::Extracted,
  ) -> Result<SelectorOutcome, PipelineError> {
    Ok(SelectorOutcome::Selected {
      account_id: SmolStr::new("acct-1"),
      provider_id: SmolStr::new("zai-coding-plan"),
      upstream_endpoint: Endpoint::ChatCompletions,
      upstream_model: SmolStr::new("glm-4"),
      account_handle: Arc::new(()),
    })
  }
}

struct EmptySelector;

#[async_trait::async_trait]
impl AccountSelector for EmptySelector {
  async fn select(
    &self,
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
  headers.insert(HeaderName::new("x-behave-as"), HeaderValue::from_static("codex"));
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
        StageEvent::Extract { .. } => "extract",
        StageEvent::Resolve { .. } => "resolve",
        StageEvent::BuildHeaders => "build_headers",
        StageEvent::ConvertRequest => "convert_request",
        StageEvent::Send => "send",
        StageEvent::ConvertResponse => "convert_response",
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
  let runner = PipelineRunner::new(profile, bus);

  let outcome = runner.run(raw_chat("input-model")).await;
  assert!(outcome.success, "outcome: {outcome:?}");
  assert_eq!(outcome.attempts, 1);

  let events = log.lock().unwrap();
  let kinds = known_kinds(&events);
  assert_eq!(
    kinds,
    ["started", "extract", "resolve", "build_headers", "convert_request", "completed"]
  );

  // Spot-check the Resolve event carries the upstream model and provider.
  let resolve = events.iter().find_map(|e| match &e.payload {
    EventPayload::Known(StageEvent::Resolve {
      upstream_model,
      provider_id,
      account_id,
      client_id,
      ..
    }) => Some((upstream_model.clone(), provider_id.clone(), account_id.clone(), client_id.clone())),
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
      EventPayload::Known(StageEvent::Error {
        stage, recoverable, ..
      }) => Some((*stage, *recoverable)),
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
