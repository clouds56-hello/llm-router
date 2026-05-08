//! Format-agnostic upstream-response forwarders.
//!
//! Both buffered and streaming variants forward upstream bytes verbatim. Token
//! usage extraction is format-aware so usage logging stays uniform across chat,
//! responses, and messages endpoints.

mod buffered;
pub(crate) mod context;
mod observers;
mod passthrough;
mod recording;
mod stream;
mod usage;

pub(crate) use buffered::buffered_response;
pub(crate) use context::ForwardContext;
pub(crate) use passthrough::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response};
pub(crate) use stream::stream_response;

#[cfg(test)]
mod tests {
  use super::context::ForwardContext;
  use super::passthrough::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response};
  use super::recording::{extract_request_messages, CompletedEventBuilder};
  use super::usage::parse_usage_any_value;
  use crate::config::{Account as AccountCfg, AuthType, Config};
  use crate::db::{CallRecord, SessionSource};
  use crate::provider::Endpoint;
  use crate::server::build_state;
  use llm_core::event::{Event, EventBus, EventHandler};
  use crate::util::secret::Secret;
  use axum::body::to_bytes;
  use axum::http::{HeaderMap, Method};
  use bytes::Bytes;
  use serde_json::json;
  use std::sync::{Arc, Mutex};
  use std::time::Instant;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};

  /// Shared record collector for tests.
  type Records = Arc<Mutex<Vec<CallRecord>>>;

  /// Event handler that accumulates lifecycle events and produces CallRecords on completion.
  struct CollectingHandler {
    records: Records,
    pending: std::collections::HashMap<String, CallRecord>,
  }

  impl CollectingHandler {
    fn new(records: Records) -> Self {
      Self { records, pending: std::collections::HashMap::new() }
    }
  }

  impl EventHandler for CollectingHandler {
    fn handle(&mut self, event: &Event) {
      match event {
        Event::RequestStarted { request_id, ts, endpoint, initiator, session_id, project_id, inbound_req } => {
          self.pending.insert(request_id.clone(), CallRecord {
            ts: *ts,
            session_id: session_id.clone().unwrap_or_default(),
            session_source: SessionSource::Auto,
            request_id: Some(request_id.clone()),
            request_error: None,
            project_id: project_id.clone(),
            endpoint: endpoint.clone(),
            account_id: String::new(),
            provider_id: String::new(),
            model: String::new(),
            initiator: initiator.clone().unwrap_or_default(),
            status: 0,
            stream: false,
            latency_ms: 0,
            prompt_tokens: None,
            completion_tokens: None,
            inbound_req: inbound_req.clone(),
            outbound_req: None,
            outbound_resp: None,
            inbound_resp: Default::default(),
            messages: Vec::new(),
          });
        }
        Event::RequestParsed { request_id, account_id, provider_id, model, stream, initiator, outbound_req } => {
          if let Some(r) = self.pending.get_mut(request_id) {
            r.account_id = account_id.clone();
            r.provider_id = provider_id.clone();
            r.model = model.clone();
            r.stream = *stream;
            r.initiator = initiator.clone();
            r.outbound_req = outbound_req.clone();
          }
        }
        Event::RequestCompleted { request_id, session_source, latency_ms, status, prompt_tokens, completion_tokens, request_error, inbound_resp, outbound_resp, messages } => {
          let mut r = self.pending.remove(request_id).unwrap_or_else(|| CallRecord {
            ts: 0,
            session_id: String::new(),
            session_source: SessionSource::Auto,
            request_id: Some(request_id.clone()),
            request_error: None,
            project_id: None,
            endpoint: String::new(),
            account_id: String::new(),
            provider_id: String::new(),
            model: String::new(),
            initiator: String::new(),
            status: 0,
            stream: false,
            latency_ms: 0,
            prompt_tokens: None,
            completion_tokens: None,
            inbound_req: Default::default(),
            outbound_req: None,
            outbound_resp: None,
            inbound_resp: Default::default(),
            messages: Vec::new(),
          });
          r.session_source = *session_source;
          r.latency_ms = *latency_ms;
          r.status = *status;
          r.prompt_tokens = *prompt_tokens;
          r.completion_tokens = *completion_tokens;
          r.request_error = request_error.clone();
          r.inbound_resp = inbound_resp.clone();
          r.outbound_resp = outbound_resp.clone();
          r.messages = messages.clone();
          self.records.lock().unwrap().push(r);
        }
        _ => {}
      }
    }
  }

  /// Create an event bus with a collecting handler for tests.
  /// Returns (EventBus, Records, JoinHandle).
  fn test_event_bus() -> (Arc<EventBus>, Records) {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let handler = CollectingHandler::new(records.clone());
    let (bus, receiver) = EventBus::new(64);
    llm_core::event::spawn_event_loop(receiver, vec![Box::new(handler)]);
    (Arc::new(bus), records)
  }

  #[test]
  fn parses_openai_chat_usage() {
    let v = json!({ "usage": { "prompt_tokens": 11, "completion_tokens": 22 }});
    assert_eq!(parse_usage_any_value(&v), (Some(11), Some(22)));
  }

  #[test]
  fn parses_responses_usage_shape() {
    let v = json!({ "usage": { "input_tokens": 5, "output_tokens": 7 }});
    assert_eq!(parse_usage_any_value(&v), (Some(5), Some(7)));
  }

  #[test]
  fn parses_anthropic_message_start_nested_usage() {
    let v = json!({
        "type": "message_start",
        "message": { "usage": { "input_tokens": 9, "output_tokens": 1 }}
    });
    assert_eq!(parse_usage_any_value(&v), (Some(9), Some(1)));
  }

  #[test]
  fn parses_responses_response_completed_nested_usage() {
    let v = json!({
        "type": "response.completed",
        "response": { "usage": { "input_tokens": 3, "output_tokens": 4 }}
    });
    assert_eq!(parse_usage_any_value(&v), (Some(3), Some(4)));
  }

  #[test]
  fn detects_sse_content_type_with_charset() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::CONTENT_TYPE,
      "text/event-stream; charset=utf-8".parse().unwrap(),
    );

    assert!(is_sse_response(&headers));
  }

  #[test]
  fn chat_array_content_becomes_multiple_parts() {
    let body = json!({
      "messages": [{
        "role": "user",
        "content": [
          { "type": "text", "text": "hello" },
          { "type": "image_url", "image_url": { "url": "data:image/png;base64,abc" } }
        ]
      }]
    });
    let messages = extract_request_messages(&body, Endpoint::ChatCompletions, 1024);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].parts.len(), 2);
    assert_eq!(messages[0].parts[0].part_type, "text");
    assert_eq!(messages[0].parts[0].content.as_ref(), b"hello");
    assert_eq!(messages[0].parts[1].part_type, "image_url");
    assert!(std::str::from_utf8(messages[0].parts[1].content.as_ref())
      .unwrap()
      .contains("image_url"));
  }

  #[tokio::test]
  async fn build_call_record_generates_auto_session_and_assistant_raw_part() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let req_body = json!({ "model": "glm-4.6", "messages": [{ "role": "user", "content": "hi" }] });
    let resp_body = Bytes::from_static(br#"{"id":"r1"}"#);
    let event = CompletedEventBuilder::new(
      1024 * 1024,
      crate::db::HttpSnapshot {
        method: None,
        url: None,
        status: Some(200),
        headers: HeaderMap::new(),
        body: resp_body.clone(),
      },
      Instant::now(),
      200,
    )
    .with_ids(None, None, None)
    .with_request_body(&req_body, Some(Endpoint::ChatCompletions))
    .with_outbound_response(Some(&HeaderMap::new()), Some(&resp_body))
    .build();
    if let llm_core::event::Event::RequestCompleted { session_source, messages, .. } = &event {
      assert_eq!(*session_source, SessionSource::Auto);
      assert!(messages
        .iter()
        .flat_map(|m| &m.parts)
        .any(|p| p.part_type == "raw" && p.content.as_ref() == resp_body.as_ref()));
    } else {
      panic!("expected RequestCompleted event");
    }
  }

  #[tokio::test]
  async fn build_call_record_persists_header_session_request_and_project_ids() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let req_body = json!({ "model": "glm-4.6", "messages": [] });

    let event = CompletedEventBuilder::new(
      1024 * 1024,
      crate::db::HttpSnapshot {
        method: None,
        url: None,
        status: Some(200),
        headers: HeaderMap::new(),
        body: Bytes::new(),
      },
      Instant::now(),
      200,
    )
    .with_ids(
      Some("client-session"),
      Some("request-123"),
      Some("stream terminated before completion"),
    )
    .with_request_body(&req_body, Some(Endpoint::ChatCompletions))
    .build();

    if let llm_core::event::Event::RequestCompleted { session_source, request_id, request_error, .. } = &event {
      assert_eq!(*session_source, SessionSource::Header);
      assert_eq!(request_id, "request-123");
      assert_eq!(request_error.as_deref(), Some("stream terminated before completion"));
    } else {
      panic!("expected RequestCompleted event");
    }
  }

  #[tokio::test]
  async fn record_passthrough_call_persists_requests_row_shape() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let (events, records) = test_event_bus();
    let state = build_state(&cfg, events.clone()).unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert("x-session-id", "client-session".parse().unwrap());
    let req_body =
      Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#);

    let req_body_json: serde_json::Value = serde_json::from_slice(&req_body).unwrap();

    let ctx = ForwardContext::from_passthrough(
      &Method::POST,
      "/v1/chat/completions",
      &req_headers,
      &req_body_json,
      Instant::now(),
    );

    // Emit lifecycle events as caller would
    state.events.emit(llm_core::event::Event::RequestStarted {
      request_id: ctx.request_id.clone().unwrap_or_default(),
      ts: 0,
      endpoint: ctx.endpoint.map(|e| e.as_str()).unwrap_or("unknown").to_string(),
      initiator: None,
      session_id: ctx.session_id.clone(),
      project_id: None,
      inbound_req: crate::db::HttpSnapshot {
        method: Some("POST".to_string()),
        url: Some("https://api.openai.com/v1/chat/completions".to_string()),
        status: None,
        headers: req_headers.clone(),
        body: req_body.clone(),
      },
    });
    state.events.emit(llm_core::event::Event::RequestParsed {
      request_id: ctx.request_id.clone().unwrap_or_default(),
      account_id: "passthrough".to_string(),
      provider_id: "api.openai.com".to_string(),
      model: ctx.model.clone(),
      stream: true,
      initiator: "user".to_string(),
      outbound_req: Some(crate::db::HttpSnapshot {
        method: Some("POST".to_string()),
        url: Some("https://api.openai.com/v1/chat/completions".to_string()),
        status: None,
        headers: req_headers.clone(),
        body: req_body.clone(),
      }),
    });

    // Set up a mock upstream server
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = vec![0_u8; 8192];
      let _ = stream.read(&mut buf).await.unwrap();
      stream
        .write_all(
          b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2}}",
        )
        .await
        .unwrap();
    });

    let response = reqwest::Client::new()
      .get(format!("http://{addr}/test"))
      .send()
      .await
      .unwrap();

    let resp = passthrough_buffered_response(&state, &ctx, &req_body_json, response).await;
    let resp_body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    server.await.unwrap();
    assert!(std::str::from_utf8(&resp_body).unwrap().contains("prompt_tokens"));

    // Shut down event bus to flush
    events.shutdown().await;
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].endpoint, "chat_completions");
    assert_eq!(records[0].account_id, "passthrough");
    assert_eq!(records[0].provider_id, "api.openai.com");
    assert_eq!(records[0].model, "gpt-4.1");
    assert_eq!(records[0].session_id, "client-session");
    assert_eq!(records[0].stream, true);
    assert_eq!(records[0].prompt_tokens, Some(1));
    assert_eq!(records[0].completion_tokens, Some(2));
    assert_eq!(records[0].inbound_req.method.as_deref(), Some("POST"));
    assert_eq!(
      records[0].inbound_req.url.as_deref(),
      Some("https://api.openai.com/v1/chat/completions")
    );
    assert_eq!(
      records[0].outbound_req.as_ref().and_then(|s| s.url.as_deref()),
      Some("https://api.openai.com/v1/chat/completions")
    );
  }

  #[tokio::test]
  async fn passthrough_streaming_response_records_sse_usage() {
    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    let (events, records) = test_event_bus();
    let state = build_state(&cfg, events.clone()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = vec![0_u8; 8192];
      let _ = stream.read(&mut buf).await.unwrap();
      stream
        .write_all(
          b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":2,\"completion_tokens\":3}}\n\ndata: [DONE]\n\n",
        )
        .await
        .unwrap();
    });

    let response = reqwest::Client::new()
      .get(format!("http://{addr}/stream"))
      .send()
      .await
      .unwrap();
    assert!(is_sse_response(response.headers()));

    let req_body_bytes = Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#);
    let req_body_json: serde_json::Value = serde_json::from_slice(&req_body_bytes).unwrap();

    let ctx = ForwardContext::from_passthrough(
      &Method::POST,
      "/v1/chat/completions",
      &HeaderMap::new(),
      &req_body_json,
      Instant::now(),
    );

    let streamed = passthrough_streaming_response(
      state,
      ctx,
      &req_body_json,
      response,
    );
    let streamed_body = to_bytes(streamed.into_body(), usize::MAX).await.unwrap();
    server.await.unwrap();

    let body_text = std::str::from_utf8(&streamed_body).unwrap();
    assert!(body_text.contains("prompt_tokens"));
    // Allow background recorder task to finish processing
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // Flush the event bus to ensure the record is captured
    events.shutdown().await;
    let records = records.lock().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].prompt_tokens, Some(2));
    assert_eq!(records[0].completion_tokens, Some(3));
    assert_eq!(records[0].request_error, None);
    assert!(std::str::from_utf8(records[0].inbound_resp.body.as_ref())
      .unwrap()
      .contains("[DONE]"));
  }
}
