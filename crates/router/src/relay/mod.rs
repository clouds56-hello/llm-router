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

pub(crate) use buffered::buffered_response;
pub(crate) use context::ForwardContext;
pub(crate) use passthrough::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response};
pub(crate) use stream::stream_response;

#[cfg(test)]
mod tests {
  use super::buffered_response;
  use super::context::ForwardContext;
  use super::passthrough::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response};
  use super::recording::{extract_request_messages, CompletedEventBuilder};
  use crate::api::build_state;
  use crate::config::{Account as AccountCfg, AuthType, Config};
  use crate::db::{CallRecord, SessionSource, Usage};
  use crate::pipeline::{BodyExtract, HeaderExtract};
  use crate::provider::Endpoint;
  use crate::util::secret::Secret;
  use axum::body::to_bytes;
  use axum::http::{HeaderMap, Method};
  use bytes::Bytes;
  use llm_core::event::{Event, EventBus, EventHandler, LegacyRequestEvent};
  use reqwest::ResponseBuilderExt;
  use serde_json::json;
  use std::sync::{Arc, Mutex};
  use std::time::Instant;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};

  /// Shared record collector for tests.
  type Records = Arc<Mutex<Vec<CallRecord>>>;

  /// Event handler that accumulates lifecycle events and produces CallRecords on completion.
  struct CollectingHandler {
    records: Records,
    pending: std::collections::HashMap<(String, u32), CallRecord>,
  }

  impl CollectingHandler {
    fn new(records: Records) -> Self {
      Self {
        records,
        pending: std::collections::HashMap::new(),
      }
    }
  }

  impl EventHandler for CollectingHandler {
    fn handle(&mut self, event: &Event) {
      match event {
        Event::LegacyRequest(LegacyRequestEvent::Started {
          request_id,
          ts,
          endpoint,
          session_id,
          method,
          url,
          ..
        }) => {
          self.pending.insert(
            (request_id.clone(), 0),
            CallRecord {
              ts: *ts,
              session_id: session_id.clone().unwrap_or_default(),
              session_source: SessionSource::Auto,
              user: None,
              local_addr: None,
              mode: None,
              behave_as: None,
              peer_addr: None,
              method: Some(method.clone()),
              request_id: request_id.clone(),
              request_error: None,
              project_id: None,
              endpoint: endpoint.clone(),
              account_id: String::new(),
              provider_id: String::new(),
              model: String::new(),
              initiator: String::new(),
              status: 0,
              stream: false,
              latency_ms: None,
              latency_header_ms: None,
              usage: Usage::default(),
              inbound: crate::db::HttpSnapshot {
                method: Some(method.clone()),
                url: url.clone(),
                ..Default::default()
              },
              outbound: None,
              messages: Vec::new(),
            },
          );
        }
        Event::LegacyRequest(LegacyRequestEvent::Parsed {
          request_id,
          attempt,
          account_id,
          provider_id,
          model,
          stream,
          initiator,
          behave_as,
          inbound_body,
        }) => {
          // Retry attempts: clone from base entry (attempt 0)
          let key = (request_id.clone(), *attempt);
          if *attempt > 0 && !self.pending.contains_key(&key) {
            if let Some(base) = self.pending.get(&(request_id.clone(), 0)).cloned() {
              let mut retry = base;
              retry.latency_header_ms = None;
              self.pending.insert(key.clone(), retry);
            }
          }
          if let Some(r) = self.pending.get_mut(&key) {
            r.account_id = account_id.clone();
            r.provider_id = provider_id.clone();
            r.model = model.clone();
            r.stream = *stream;
            r.initiator = initiator.clone();
            r.behave_as = behave_as.clone();
            r.inbound.req_body = inbound_body.clone();
            // Stamp composite request_id on the row for retries
            if *attempt > 0 {
              r.request_id = format!("{request_id}:{attempt}");
            }
          }
        }
        Event::LegacyRequest(LegacyRequestEvent::Result {
          request_id,
          attempt,
          session_source,
          latency_ms,
          inbound_status,
          usage,
          request_error,
          inbound_resp_headers,
          inbound_resp_body,
          outbound_resp_body,
          messages,
        }) => {
          let key = (request_id.clone(), *attempt);
          let composite_id = if *attempt == 0 {
            request_id.clone()
          } else {
            format!("{request_id}:{attempt}")
          };
          let mut r = self.pending.remove(&key).unwrap_or_else(|| CallRecord {
            ts: 0,
            session_id: String::new(),
            session_source: SessionSource::Auto,
            user: None,
            local_addr: None,
            mode: None,
            behave_as: None,
            peer_addr: None,
            method: None,
            request_id: composite_id.clone(),
            request_error: None,
            project_id: None,
            endpoint: String::new(),
            account_id: String::new(),
            provider_id: String::new(),
            model: String::new(),
            initiator: String::new(),
            status: 0,
            stream: false,
            latency_ms: None,
            latency_header_ms: None,
            usage: Usage::default(),
            inbound: Default::default(),
            outbound: None,
            messages: Vec::new(),
          });
          r.session_source = *session_source;
          r.latency_ms = Some(*latency_ms);
          r.status = *inbound_status;
          r.usage = usage.clone();
          r.request_error = request_error.clone();
          r.inbound.status = Some(*inbound_status);
          r.inbound.resp_headers = inbound_resp_headers.clone();
          r.inbound.resp_body = inbound_resp_body.clone();
          if let Some(b) = outbound_resp_body.as_ref() {
            if let Some(o) = r.outbound.as_mut() {
              o.resp_body = b.clone();
            } else {
              r.outbound = Some(crate::db::HttpSnapshot {
                resp_body: b.clone(),
                ..Default::default()
              });
            }
          }
          r.messages = messages.clone();
          self.records.lock().unwrap().push(r);
        }
        Event::LegacyRequest(LegacyRequestEvent::Responded {
          request_id,
          attempt,
          latency_ms,
          outbound_status,
          outbound_resp_headers,
          outbound_req_method,
          outbound_req_url,
          outbound_req_headers,
          outbound_req_body,
        }) => {
          let key = (request_id.clone(), *attempt);
          if let Some(r) = self.pending.get_mut(&key) {
            r.latency_header_ms = Some(*latency_ms);
            let outbound = r.outbound.get_or_insert_with(crate::db::HttpSnapshot::default);
            outbound.status = Some(*outbound_status);
            outbound.resp_headers = outbound_resp_headers.clone();
            if outbound_req_method.is_some() {
              outbound.method = outbound_req_method.clone();
            }
            if outbound_req_url.is_some() {
              outbound.url = outbound_req_url.clone();
            }
            if let Some(h) = outbound_req_headers.as_ref() {
              outbound.req_headers = h.clone();
            }
            if let Some(b) = outbound_req_body.as_ref() {
              outbound.req_body = b.clone();
            }
          }
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
    let (bus, receiver) = {
      let bus = EventBus::new(64);
      let rx = bus.subscribe();
      (bus, rx)
    };
    llm_core::event::spawn_event_loop(receiver, vec![Box::new(handler)]);
    (Arc::new(bus), records)
  }

  #[test]
  fn detects_sse_content_type_with_charset() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::CONTENT_TYPE,
      "text/event-stream; charset=utf-8".parse().unwrap(),
    );

    assert!(is_sse_response(&headers, false));
  }

  #[test]
  fn falls_back_to_stream_when_content_type_missing() {
    let headers = HeaderMap::new();
    assert!(is_sse_response(&headers, true));
    assert!(!is_sse_response(&headers, false));
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
    let req_body = json!({ "model": "glm-4.6", "messages": [{ "role": "user", "content": "hi" }] });
    let resp_body = Bytes::from_static(br#"{"id":"r1"}"#);
    let event = CompletedEventBuilder::new(
      1024 * 1024,
      "test-req".to_string(),
      HeaderMap::new(),
      resp_body.clone(),
      Instant::now(),
      200,
    )
    .with_ids(None, None)
    .with_request_body(&req_body, Some(Endpoint::ChatCompletions))
    .with_outbound_response_body(Some(&resp_body))
    .build();
    if let llm_core::event::Event::LegacyRequest(llm_core::event::LegacyRequestEvent::Result {
      session_source,
      messages,
      ..
    }) = &event
    {
      assert_eq!(*session_source, SessionSource::Auto);
      assert!(messages
        .iter()
        .flat_map(|m| &m.parts)
        .any(|p| p.part_type == "raw" && p.content.as_ref() == resp_body.as_ref()));
    } else {
      panic!("expected RequestResult event");
    }
  }

  #[tokio::test]
  async fn build_call_record_persists_header_session_request_and_project_ids() {
    let req_body = json!({ "model": "glm-4.6", "messages": [] });

    let event = CompletedEventBuilder::new(
      1024 * 1024,
      "request-123".to_string(),
      HeaderMap::new(),
      Bytes::new(),
      Instant::now(),
      200,
    )
    .with_ids(Some("client-session"), Some("stream terminated before completion"))
    .with_request_body(&req_body, Some(Endpoint::ChatCompletions))
    .build();

    if let llm_core::event::Event::LegacyRequest(llm_core::event::LegacyRequestEvent::Result {
      session_source,
      request_id,
      request_error,
      ..
    }) = &event
    {
      assert_eq!(*session_source, SessionSource::Header);
      assert_eq!(request_id, "request-123");
      assert_eq!(request_error.as_deref(), Some("stream terminated before completion"));
    } else {
      panic!("expected RequestResult event");
    }
  }

  #[tokio::test]
  async fn record_passthrough_call_persists_requests_row_shape() {
    let cfg = Config::default();
    let accounts = vec![AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tier: llm_core::account::AccountTier::Active,
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
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }];
    let (events, records) = test_event_bus();
    let state = build_state(&cfg, &accounts, events.clone()).unwrap();
    let mut req_headers = HeaderMap::new();
    req_headers.insert("x-session-id", "client-session".parse().unwrap());
    let req_body =
      Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#);

    let req_body_json: serde_json::Value = serde_json::from_slice(&req_body).unwrap();
    let request_id = "request-123".to_string();
    let header_extract = HeaderExtract {
      request_id: request_id.clone(),
      session_id: Some("client-session".to_string()),
      project_id: None,
      header_initiator: None,
      route_mode_hint: None,
    };
    let body_extract = BodyExtract {
      model: "gpt-4.1".to_string(),
      stream: true,
      initiator: "user".to_string(),
      header_initiator: None,
    };

    let ctx = ForwardContext::from_passthrough(
      &Method::POST,
      "/v1/chat/completions",
      &header_extract,
      &body_extract,
      req_headers.clone(),
      Instant::now(),
    );
    assert_eq!(ctx.request_id, request_id);

    // Emit lifecycle events as caller would
    state.events.emit(llm_core::event::Event::LegacyRequest(
      llm_core::event::LegacyRequestEvent::Started {
        request_id: ctx.request_id.clone(),
        ts: 0,
        endpoint: ctx.endpoint.map(|e| e.as_str()).unwrap_or("unknown").to_string(),
        session_id: ctx.session_id.clone(),
        peer_addr: Some("127.0.0.1:4142".into()),
        local_addr: Some("127.0.0.1:4141".into()),
        method: "POST".into(),
        inbound_method: "POST".into(),
        url: Some("https://api.openai.com/v1/chat/completions".into()),
      },
    ));
    state.events.emit(llm_core::event::Event::LegacyRequest(
      llm_core::event::LegacyRequestEvent::Parsed {
        request_id: ctx.request_id.clone(),
        attempt: 0,
        account_id: "passthrough".to_string(),
        provider_id: "api.openai.com".to_string(),
        model: ctx.model.clone(),
        stream: true,
        initiator: "user".to_string(),
        behave_as: None,
        inbound_body: req_body.clone(),
      },
    ));
    state.events.emit(llm_core::event::Event::LegacyRequest(
      llm_core::event::LegacyRequestEvent::Responded {
        request_id: ctx.request_id.clone(),
        attempt: 0,
        outbound_status: 200,
        latency_ms: 1,
        outbound_resp_headers: llm_headers::HeaderMap::new(),
        outbound_req_method: Some("POST".to_string()),
        outbound_req_url: Some("https://api.openai.com/v1/chat/completions".to_string()),
        outbound_req_headers: Some((&req_headers).into()),
        outbound_req_body: Some(req_body.clone()),
      },
    ));

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
    assert_eq!(records[0].usage.input_tokens, Some(1));
    assert_eq!(records[0].usage.output_tokens, Some(2));
    assert_eq!(records[0].inbound.method.as_deref(), Some("POST"));
    assert_eq!(
      records[0].inbound.url.as_deref(),
      Some("https://api.openai.com/v1/chat/completions")
    );
    assert_eq!(
      records[0].outbound.as_ref().and_then(|s| s.url.as_deref()),
      Some("https://api.openai.com/v1/chat/completions")
    );
  }

  #[tokio::test]
  async fn buffered_response_synthesizes_json_error_for_blank_upstream_error() {
    let cfg = Config::default();
    let accounts = vec![AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tier: llm_core::account::AccountTier::Active,
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
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }];
    let state = build_state(&cfg, &accounts, Arc::new(EventBus::noop())).unwrap();
    let response = reqwest::Response::from(
      http::Response::builder()
        .status(501)
        .url(reqwest::Url::parse("https://api.openai.com/v1/responses").unwrap())
        .body("")
        .unwrap(),
    );
    let ctx = ForwardContext::from_pipeline(
      Endpoint::Responses,
      Endpoint::Responses,
      "unknown".into(),
      None,
      "request-blank-501".into(),
      0,
      Instant::now(),
    );

    let resp = buffered_response(state, response, ctx, &json!({ "model": "unknown", "input": "hi" })).await;
    assert_eq!(resp.status(), axum::http::StatusCode::NOT_IMPLEMENTED);

    let resp_body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
    assert_eq!(json["error"]["type"], "upstream_error");
    assert_eq!(json["error"]["code"], 501);
    assert_eq!(
      json["error"]["message"],
      serde_json::Value::String("upstream returned 501 with an empty response body".into())
    );
  }

  #[tokio::test]
  async fn passthrough_streaming_response_records_sse_usage() {
    let cfg = Config::default();
    let accounts = vec![AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tier: llm_core::account::AccountTier::Active,
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
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }];
    let (events, records) = test_event_bus();
    let state = build_state(&cfg, &accounts, events.clone()).unwrap();

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
    assert!(is_sse_response(response.headers(), true));

    let req_body_bytes =
      Bytes::from_static(br#"{"model":"gpt-4.1","messages":[{"role":"user","content":"hi"}],"stream":true}"#);
    let req_body_json: serde_json::Value = serde_json::from_slice(&req_body_bytes).unwrap();
    let request_id = "request-456".to_string();
    let header_extract = HeaderExtract {
      request_id: request_id.clone(),
      session_id: None,
      project_id: None,
      header_initiator: None,
      route_mode_hint: None,
    };
    let body_extract = BodyExtract {
      model: "gpt-4.1".to_string(),
      stream: true,
      initiator: "user".to_string(),
      header_initiator: None,
    };

    let ctx = ForwardContext::from_passthrough(
      &Method::POST,
      "/v1/chat/completions",
      &header_extract,
      &body_extract,
      HeaderMap::new(),
      Instant::now(),
    );
    assert_eq!(ctx.request_id, request_id);

    let streamed = passthrough_streaming_response(state, ctx, &req_body_json, response);
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
    assert_eq!(records[0].usage.input_tokens, Some(2));
    assert_eq!(records[0].usage.output_tokens, Some(3));
    assert_eq!(records[0].request_error, None);
    assert!(std::str::from_utf8(records[0].inbound.resp_body.as_ref())
      .unwrap()
      .contains("[DONE]"));
  }
}
