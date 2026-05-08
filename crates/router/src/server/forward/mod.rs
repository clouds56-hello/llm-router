//! Format-agnostic upstream-response forwarders.
//!
//! Both buffered and streaming variants forward upstream bytes verbatim. Token
//! usage extraction is format-aware so usage logging stays uniform across chat,
//! responses, and messages endpoints.

mod buffered;
mod observers;
mod passthrough;
mod recording;
mod stream;
mod usage;

pub(crate) use buffered::buffered_response;
pub(crate) use passthrough::{is_sse_response, passthrough_streaming_response, record_passthrough_call};
pub(crate) use stream::stream_response;

#[cfg(test)]
mod tests {
  use super::passthrough::{is_sse_response, passthrough_streaming_response, record_passthrough_call};
  use super::recording::{extract_request_messages, CallRecordBuilder};
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
  use uuid::Uuid;

  /// Shared record collector for tests.
  type Records = Arc<Mutex<Vec<CallRecord>>>;

  /// Event handler that collects CallRecords for test assertions.
  struct CollectingHandler {
    records: Records,
  }

  impl EventHandler for CollectingHandler {
    fn handle(&mut self, event: &Event) {
      if let Event::RequestCompleted { record } = event {
        self.records.lock().unwrap().push(record.clone());
      }
    }
  }

  /// Create an event bus with a collecting handler for tests.
  /// Returns (EventBus, Records, JoinHandle).
  fn test_event_bus() -> (Arc<EventBus>, Records) {
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let handler = CollectingHandler { records: records.clone() };
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
    let record = CallRecordBuilder::for_endpoint(
      1024 * 1024, // body_max_bytes
      "acct",
      "zai-coding-plan",
      Endpoint::ChatCompletions,
      "glm-4.6",
      "user",
      crate::db::HttpSnapshot {
        method: None,
        url: None,
        status: Some(200),
        headers: HeaderMap::new(),
        body: resp_body.clone(),
      },
      Instant::now(),
      200,
      false,
    )
    .with_ids(None, None, None, None)
    .with_request_json(&HeaderMap::new(), &req_body)
    .with_outbound_response(Some(&HeaderMap::new()), Some(&resp_body))
    .build();
    assert_eq!(record.session_source, SessionSource::Auto);
    Uuid::parse_str(&record.session_id).unwrap();
    assert!(record
      .messages
      .iter()
      .flat_map(|m| &m.parts)
      .any(|p| p.part_type == "raw" && p.content.as_ref() == resp_body.as_ref()));
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

    let record = CallRecordBuilder::for_endpoint(
      1024 * 1024, // body_max_bytes
      "acct",
      "zai-coding-plan",
      Endpoint::ChatCompletions,
      "glm-4.6",
      "user",
      crate::db::HttpSnapshot {
        method: None,
        url: None,
        status: Some(200),
        headers: HeaderMap::new(),
        body: Bytes::new(),
      },
      Instant::now(),
      200,
      false,
    )
    .with_ids(
      Some("client-session"),
      Some("request-123"),
      Some("stream terminated before completion"),
      Some("project-456"),
    )
    .with_request_json(&HeaderMap::new(), &req_body)
    .build();

    assert_eq!(record.session_id, "client-session");
    assert_eq!(record.session_source, SessionSource::Header);
    assert_eq!(record.request_id.as_deref(), Some("request-123"));
    assert_eq!(
      record.request_error.as_deref(),
      Some("stream terminated before completion")
    );
    assert_eq!(record.project_id.as_deref(), Some("project-456"));
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
    let mut outbound_req_headers = HeaderMap::new();
    outbound_req_headers.insert(axum::http::header::HOST, "api.openai.com".parse().unwrap());
    let req_body =
      Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#);
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(axum::http::header::CONTENT_TYPE, "application/json".parse().unwrap());
    let resp_body = Bytes::from_static(br#"{"usage":{"prompt_tokens":1,"completion_tokens":2}}"#);

    record_passthrough_call(
      &state,
      "api.openai.com",
      &Method::POST,
      "/v1/chat/completions",
      &req_headers,
      &req_body,
      &outbound_req_headers,
      &resp_headers,
      &resp_body,
      200,
      Instant::now(),
    );

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

    let streamed = passthrough_streaming_response(
      state,
      "api.openai.com".to_string(),
      Method::POST,
      "/v1/chat/completions".to_string(),
      HeaderMap::new(),
      Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#),
      HeaderMap::new(),
      response,
      Instant::now(),
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
