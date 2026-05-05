//! Format-agnostic upstream-response forwarders.
//!
//! Both buffered and streaming variants forward upstream bytes verbatim. Token
//! usage extraction is format-aware so usage logging stays uniform across chat,
//! responses, and messages endpoints.

mod buffered;
mod passthrough;
mod recording;
mod stream;
mod usage;

pub(crate) use buffered::buffered_response;
pub(crate) use passthrough::record_passthrough_call;
pub(crate) use stream::stream_response;

#[cfg(test)]
mod tests {
  use super::passthrough::record_passthrough_call;
  use super::recording::{build_call_record, extract_request_messages};
  use super::usage::parse_usage_any_value;
  use crate::config::{Account as AccountCfg, AuthType, Config};
  use crate::db::{CallRecord, SessionSource};
  use crate::provider::Endpoint;
  use crate::server::build_state;
  use crate::util::secret::Secret;
  use axum::http::{HeaderMap, Method};
  use bytes::Bytes;
  use llm_core::db::DbStore;
  use serde_json::json;
  use std::sync::{Arc, Mutex};
  use std::time::Instant;
  use uuid::Uuid;

  #[derive(Default)]
  struct FakeDb {
    records: Mutex<Vec<CallRecord>>,
  }

  impl crate::db::DbStore for FakeDb {
    fn body_max_bytes(&self) -> usize {
      1024 * 1024
    }

    fn record(&self, record: CallRecord) {
      self.records.lock().unwrap().push(record);
    }
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
    let db = Arc::new(FakeDb::default());
    let req_body = json!({ "model": "glm-4.6", "messages": [{ "role": "user", "content": "hi" }] });
    let resp_body = Bytes::from_static(br#"{"id":"r1"}"#);
    let record = build_call_record(
      db.body_max_bytes(),
      "acct",
      "zai-coding-plan",
      Endpoint::ChatCompletions,
      "glm-4.6",
      "user",
      None,
      None,
      None,
      None,
      &HeaderMap::new(),
      &req_body,
      Some(&HeaderMap::new()),
      Some(&resp_body),
      &HeaderMap::new(),
      None,
      None,
      None,
      Instant::now(),
      200,
      false,
    );
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
    let db = Arc::new(FakeDb::default());
    let req_body = json!({ "model": "glm-4.6", "messages": [] });

    let record = build_call_record(
      db.body_max_bytes(),
      "acct",
      "zai-coding-plan",
      Endpoint::ChatCompletions,
      "glm-4.6",
      "user",
      Some("client-session"),
      Some("request-123"),
      Some("stream terminated before completion"),
      Some("project-456"),
      &HeaderMap::new(),
      &req_body,
      None,
      None,
      &HeaderMap::new(),
      None,
      None,
      None,
      Instant::now(),
      200,
      false,
    );

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
    let db = Arc::new(FakeDb::default());
    let state = build_state(&cfg, Some(db.clone())).unwrap();
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

    let records = db.records.lock().unwrap();
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
}
