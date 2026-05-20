//! Integration test for the MITM proxy passthrough pipeline.
//!
//! This test drives [`proxy_passthrough_via_pipeline_inner`] (the
//! pre-body-read inner core) directly with a mock TCP upstream. Going
//! through the inner fn lets us bypass the full proxy TCP/CONNECT
//! machinery while still exercising:
//!
//! * `ProxyResolve` (reads `proxy.host` / `proxy.provider_id` /
//!   `proxy.account_id` from `RunConfig`).
//! * `PassthroughExtract` → `PassthroughBuildHeaders` (router-owned
//!   header stripping + client-auth preservation).
//! * `PassthroughConvertRequest` (verbatim body bytes).
//! * `ProxySend` (dispatch to `{scheme}://{host}{path}`).
//! * `PassthroughConvertResponse` (buffered response forwarding).
//! * `AccountIdentityResolver` integration (provider_id falls back to
//!   the intercepted host when no fingerprint match).
//! * `RecordEvent::UpstreamReq` emission with the right url + headers.

use axum::body::to_bytes;
use axum::http::{Method, Request, StatusCode};
use bytes::Bytes;
use llm_config::RouteMode;
use llm_core::event::EventBus;
use llm_router::api::build_state;
use llm_router::config::Config;
use llm_router::proxy::passthrough_pipeline::proxy_passthrough_via_pipeline_inner;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn proxy_passthrough_pipeline_forwards_request_and_preserves_client_auth() {
  use llm_core::event::Event as CoreEvent;
  use llm_core::request_event::{RecordEvent, RequestEventPayload};

  // Mock TCP upstream — captures the request, returns a known JSON.
  let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
  let addr = listener.local_addr().unwrap();

  let upstream_body = br#"{"id":"resp-proxy","ok":true}"#;
  let (req_tx, req_rx) = tokio::sync::oneshot::channel::<Vec<u8>>();

  let server = tokio::spawn(async move {
    let (mut stream, _) = listener.accept().await.unwrap();
    let mut buf = vec![0_u8; 16384];
    let n = stream.read(&mut buf).await.unwrap();
    buf.truncate(n);
    let resp = format!(
      "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
      upstream_body.len()
    );
    stream.write_all(resp.as_bytes()).await.unwrap();
    stream.write_all(upstream_body).await.unwrap();
    stream.flush().await.unwrap();
    let _ = req_tx.send(buf);
  });

  // Build router state in passthrough mode with zero accounts. The
  // proxy passthrough pipeline does no account resolution; identity
  // fallback for provider_id will be the intercepted host.
  let mut cfg = Config::default();
  cfg.server.route_mode = RouteMode::Passthrough;
  let events = Arc::new(EventBus::new(256));
  let mut rx = events.subscribe();
  let state = build_state(&cfg, &[], events.clone()).unwrap();

  let inbound_body = Bytes::from_static(
    br#"{"stream":false,"model":"glm-4.6","messages":[{"role":"user","content":"hi proxy"}]}"#,
  );

  // Use a non-default port + http scheme so the test exercises the
  // port-preservation path. The mock listener already bound to an
  // arbitrary high port; we pass the bare host and that port through
  // explicitly.
  let intercepted_host = addr.ip().to_string();
  let intercepted_port = addr.port();
  let expected_authority = format!("{intercepted_host}:{intercepted_port}");

  let req = Request::builder()
    .method(Method::POST)
    .uri("/v1/chat/completions")
    .header("content-type", "application/json")
    .header("authorization", "Bearer client-bearer-should-reach-upstream")
    .header("x-llm-router-local-addr", "127.0.0.1:9999")
    .body(())
    .unwrap();
  let (parts, ()) = req.into_parts();

  let resp = proxy_passthrough_via_pipeline_inner(
    &state,
    &intercepted_host,
    intercepted_port,
    "http",
    None,
    Some(addr.to_string()),
    parts,
    inbound_body.clone(),
  )
  .await;
  assert_eq!(resp.status(), StatusCode::OK, "proxy passthrough should succeed");

  let resp_body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
  assert_eq!(
    resp_body.as_ref(),
    upstream_body,
    "downstream body must be upstream body verbatim"
  );

  server.await.unwrap();
  let raw_req = req_rx.await.unwrap();
  let raw_req_str = String::from_utf8_lossy(&raw_req);
  let lower = raw_req_str.to_ascii_lowercase();

  // Inbound body bytes reach upstream verbatim.
  assert!(
    raw_req_str.contains(std::str::from_utf8(&inbound_body).unwrap()),
    "upstream must receive inbound body verbatim, got:\n{raw_req_str}"
  );

  // Client's own Authorization is preserved (no provider injection in
  // the proxy variant).
  assert!(
    lower.contains("authorization: bearer client-bearer-should-reach-upstream"),
    "client auth must reach upstream untouched, got:\n{raw_req_str}"
  );

  // Router-owned headers are stripped.
  assert!(
    !lower.contains("x-llm-router-local-addr"),
    "x-llm-router-* headers must be stripped before upstream send, got:\n{raw_req_str}"
  );

  // Upstream Host header is the resolved authority with the non-default
  // port preserved (since scheme=http and port != 80).
  assert!(
    lower.contains(&format!("host: {}", expected_authority.to_ascii_lowercase())),
    "Host header must be {expected_authority}, got:\n{raw_req_str}"
  );

  // Drain the full pipeline event stream (StageEvent + RecordEvent) so
  // we can assert ordering, content, and absence in one pass. The drain
  // stops as soon as we see `StageEvent::Completed` (the runner emits
  // it exactly once at the very end) or after a hard 2s budget.
  let mut events: Vec<llm_core::request_event::RequestEvent> = Vec::new();
  let drain_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
  loop {
    let remaining = drain_deadline.saturating_duration_since(tokio::time::Instant::now());
    if remaining.is_zero() {
      break;
    }
    let Ok(Ok(ev)) = tokio::time::timeout(remaining, rx.recv()).await else {
      break;
    };
    let CoreEvent::Requests(req) = &*ev else { continue };
    let req = req.clone();
    let done = matches!(&req.payload, RequestEventPayload::Stage(llm_core::request_event::StageEvent::Completed { .. }));
    events.push(req);
    if done {
      break;
    }
  }

  // Helper: locate the first event matching a predicate, or panic with
  // a debug-dump of the whole stream.
  let kinds: Vec<String> = events
    .iter()
    .map(|e| match &e.payload {
      RequestEventPayload::Stage(s) => format!("Stage::{s:?}").chars().take(40).collect(),
      RequestEventPayload::Record(r) => format!("Record::{r:?}").chars().take(40).collect(),
      RequestEventPayload::Custom(c) => format!("Custom::{}", c.kind),
    })
    .collect();
  let debug_dump = || format!("event stream was:\n  {}", kinds.join("\n  "));

  // --- Stage stream: presence + ordering ---
  use llm_core::request_event::StageEvent;
  let pos = |pred: &dyn Fn(&RequestEventPayload) -> bool| -> Option<usize> {
    events.iter().position(|e| pred(&e.payload))
  };

  let p_started = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::Started { .. })))
    .unwrap_or_else(|| panic!("missing StageEvent::Started; {}", debug_dump()));
  let p_extract = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::Extract(_))))
    .unwrap_or_else(|| panic!("missing StageEvent::Extract; {}", debug_dump()));
  let p_resolve = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::Resolve(_))))
    .unwrap_or_else(|| panic!("missing StageEvent::Resolve; {}", debug_dump()));
  let p_build = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::BuildHeaders(_))))
    .unwrap_or_else(|| panic!("missing StageEvent::BuildHeaders; {}", debug_dump()));
  let p_convreq = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::ConvertRequest(_))))
    .unwrap_or_else(|| panic!("missing StageEvent::ConvertRequest; {}", debug_dump()));
  let p_send = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::Send(_))))
    .unwrap_or_else(|| panic!("missing StageEvent::Send; {}", debug_dump()));
  let p_convresp = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::ConvertResponse(_))))
    .unwrap_or_else(|| panic!("missing StageEvent::ConvertResponse; {}", debug_dump()));
  let p_completed = pos(&|p| matches!(p, RequestEventPayload::Stage(StageEvent::Completed { .. })))
    .unwrap_or_else(|| panic!("missing StageEvent::Completed; {}", debug_dump()));

  assert!(
    p_started < p_extract
      && p_extract < p_resolve
      && p_resolve < p_build
      && p_build < p_convreq
      && p_convreq < p_send
      && p_send < p_convresp
      && p_convresp < p_completed,
    "stage events out of order; {}",
    debug_dump()
  );

  // StageEvent::Started carries the endpoint inferred from the path.
  if let RequestEventPayload::Stage(StageEvent::Started { endpoint }) = &events[p_started].payload {
    assert_eq!(*endpoint, llm_core::provider::Endpoint::ChatCompletions);
  }

  // StageEvent::Resolve: provider_id falls back to bare intercepted
  // host (no accounts configured → no fingerprint match). account_id
  // is the bearer-token fingerprint synthesised by
  // `AccountIdentityResolver` for the long `Authorization` header in
  // this test (≥32 chars triggers the `account_fp_<suffix>` fallback).
  if let RequestEventPayload::Stage(StageEvent::Resolve(r)) = &events[p_resolve].payload {
    assert_eq!(r.provider_id.as_str(), intercepted_host);
    assert!(
      r.account_id.as_str().starts_with("account_fp_"),
      "expected synthetic fingerprint account_id, got {:?}",
      r.account_id
    );
  } else {
    unreachable!()
  }

  // StageEvent::Send summary carries the upstream status.
  if let RequestEventPayload::Stage(StageEvent::Send(s)) = &events[p_send].payload {
    assert_eq!(s.status, 200, "send summary status; {}", debug_dump());
    assert!(!s.stream, "non-streaming");
  } else {
    unreachable!()
  }

  // StageEvent::Completed signals success.
  if let RequestEventPayload::Stage(StageEvent::Completed { success, attempts }) = &events[p_completed].payload {
    assert!(*success, "pipeline should report success; {}", debug_dump());
    assert!(*attempts >= 1, "at least one attempt");
  } else {
    unreachable!()
  }

  // --- Record stream: transport-truth captures ---
  let p_upreq = pos(&|p| matches!(p, RequestEventPayload::Record(RecordEvent::UpstreamReq { .. })))
    .unwrap_or_else(|| panic!("missing RecordEvent::UpstreamReq; {}", debug_dump()));
  let p_upresp = pos(&|p| matches!(p, RequestEventPayload::Record(RecordEvent::UpstreamResp { .. })))
    .unwrap_or_else(|| panic!("missing RecordEvent::UpstreamResp; {}", debug_dump()));
  let p_upbody = pos(&|p| matches!(p, RequestEventPayload::Record(RecordEvent::UpstreamBody { .. })))
    .unwrap_or_else(|| panic!("missing RecordEvent::UpstreamBody; {}", debug_dump()));

  // Wire-truth ordering: req → resp → body, all within the
  // Send/ConvertResponse window of the stage stream.
  assert!(
    p_upreq < p_upresp && p_upresp < p_upbody,
    "record events out of order; {}",
    debug_dump()
  );

  if let RequestEventPayload::Record(RecordEvent::UpstreamReq {
    method,
    url,
    headers,
    body,
  }) = &events[p_upreq].payload
  {
    assert_eq!(method.as_str(), "POST");
    assert_eq!(
      url.as_str(),
      &format!("http://{expected_authority}/v1/chat/completions"),
      "upstream url; {}",
      debug_dump()
    );
    assert_eq!(
      body.as_ref(),
      inbound_body.as_ref(),
      "upstream request body verbatim"
    );
    // Client-auth preserved on the wire-truth capture too.
    let auth = headers
      .get("authorization")
      .map(|v| v.as_str().to_string())
      .unwrap_or_default();
    assert!(
      auth.contains("Bearer client-bearer-should-reach-upstream"),
      "wire-truth authorization header preserved, got {auth:?}"
    );
    // Host header rewritten to the resolved authority.
    let host_hdr = headers
      .get("host")
      .map(|v| v.as_str().to_string())
      .unwrap_or_default();
    assert_eq!(host_hdr, expected_authority, "wire-truth Host header");
    // Router-owned headers stripped before send.
    assert!(
      headers.get("x-llm-router-local-addr").is_none(),
      "wire-truth must not include x-llm-router-* headers"
    );
  } else {
    unreachable!()
  }

  if let RequestEventPayload::Record(RecordEvent::UpstreamResp { status, headers }) = &events[p_upresp].payload {
    assert_eq!(*status, 200);
    let ct = headers
      .get("content-type")
      .map(|v| v.as_str().to_string())
      .unwrap_or_default();
    assert!(ct.starts_with("application/json"), "content-type; got {ct:?}");
  } else {
    unreachable!()
  }

  if let RequestEventPayload::Record(RecordEvent::UpstreamBody { body, error }) = &events[p_upbody].payload {
    assert!(error.is_none(), "no upstream body error");
    assert_eq!(body.as_ref(), upstream_body, "upstream body bytes");
  } else {
    unreachable!()
  }

  // Buffered (non-streaming) path: ConvertedBody is only emitted for
  // streams, so it must be absent here.
  assert!(
    !events
      .iter()
      .any(|e| matches!(&e.payload, RequestEventPayload::Record(RecordEvent::ConvertedBody { .. }))),
    "buffered proxy passthrough must not emit ConvertedBody; {}",
    debug_dump()
  );

  // InboundConnection MUST be emitted with the inbound transport facts
  // so persistence populates `local_addr`/`peer_addr`/`mode`/`method`/
  // `inbound_req_method`/`inbound_req_url`.
  let p_inbound = pos(&|p| matches!(p, RequestEventPayload::Record(RecordEvent::InboundConnection { .. })))
    .unwrap_or_else(|| panic!("missing RecordEvent::InboundConnection; {}", debug_dump()));
  if let RequestEventPayload::Record(RecordEvent::InboundConnection {
    local_addr,
    peer_addr,
    mode,
    method,
    inbound_method,
    url,
  }) = &events[p_inbound].payload
  {
    assert_eq!(local_addr.as_deref(), Some(addr.to_string().as_str()));
    assert_eq!(peer_addr.as_deref(), None);
    assert_eq!(mode.as_str(), "passthrough");
    assert_eq!(method.as_str(), "proxy");
    assert_eq!(inbound_method.as_str(), "POST");
    assert_eq!(
      url.as_deref(),
      Some(format!("http://{expected_authority}/v1/chat/completions").as_str())
    );
  } else {
    unreachable!()
  }

  // Stage::Error must not appear on a successful run.
  assert!(
    !events
      .iter()
      .any(|e| matches!(&e.payload, RequestEventPayload::Stage(StageEvent::Error { .. }))),
    "no StageEvent::Error on a successful run; {}",
    debug_dump()
  );
}
