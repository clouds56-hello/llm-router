pub(crate) mod parse;
pub(crate) mod request;

pub(crate) use parse::{
  request_header_extract, ChatParser, MessagesParser, RequestParser, ResponsesParser,
};

use crate::api::error::ApiError;
use axum::http::header::CONTENT_TYPE;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use bytes::Bytes;
use llm_core::db::{SessionSource, Usage};
use llm_core::event::{Event, LegacyRequestEvent};

pub use request::{dry_run_request, DryRunEndpoint, DryRunOutput};
use std::time::Instant;

/// JSON error envelope content-type used by `ApiError::IntoResponse`.
fn json_envelope_headers() -> HeaderMap {
  let mut h = HeaderMap::new();
  h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
  h
}

/// Builds a `RequestResult` event using the exact `ApiError`
/// supplied so the persisted envelope (status, `type`, message) matches what
/// the client actually receives — including non-upstream kinds like
/// `not_implemented_error` and `bad_gateway`.
pub(crate) fn build_failure_result_event_from_api_err(
  request_id: String,
  attempt: u32,
  started: Instant,
  api_err: &ApiError,
  upstream_body: Option<Bytes>,
) -> Event {
  Event::LegacyRequest(LegacyRequestEvent::Result {
    request_id,
    attempt,
    session_source: SessionSource::Auto,
    latency_ms: started.elapsed().as_millis() as u64,
    inbound_status: api_err.status().as_u16(),
    usage: Usage::default(),
    request_error: Some(api_err.to_string()),
    inbound_resp_headers: (&json_envelope_headers()).into(),
    inbound_resp_body: api_err.body_bytes(),
    outbound_resp_body: upstream_body,
    messages: Vec::new(),
  })
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::http::HeaderValue;
  use llm_provider_zai::Endpoint;
  use serde_json::json;

  #[test]
  fn chat_parser_reads_request_metadata() {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", HeaderValue::from_static("session-1"));
    headers.insert("x-request-id", HeaderValue::from_static("request-1"));
    headers.insert("x-opencode-project", HeaderValue::from_static("project-1"));
    headers.insert("x-behave-as", HeaderValue::from_static("architect"));
    let body = json!({
      "model": "gpt-4.1",
      "stream": true,
      "messages": [{"role": "user", "content": "hi"}]
    });

    let parsed = ChatParser.parse(headers.clone(), body.clone());

    assert_eq!(parsed.meta.endpoint, Endpoint::ChatCompletions);
    assert_eq!(parsed.meta.upstream_endpoint, Endpoint::ChatCompletions);
    assert_eq!(parsed.meta.model, "gpt-4.1");
    assert_eq!(parsed.meta.upstream_model, "gpt-4.1");
    assert!(parsed.meta.stream);
    assert_eq!(parsed.meta.session_id.as_deref(), Some("session-1"));
    assert_eq!(parsed.meta.request_id.as_deref(), Some("request-1"));
    assert_eq!(parsed.meta.project_id.as_deref(), Some("project-1"));
    assert_eq!(parsed.meta.behave_as.as_deref(), Some("architect"));
    assert_eq!(parsed.meta.initiator, "user");
    assert_eq!(parsed.body, body);
    assert_eq!(
      parsed.meta.inbound_headers.get("x-session-id").map(|v| v.as_str()),
      headers.get("x-session-id").and_then(|v| v.to_str().ok())
    );
  }

  #[test]
  fn infers_stream_from_accept_header_when_body_omits_it() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::ACCEPT,
      HeaderValue::from_static("text/event-stream"),
    );
    let body = json!({
      "model": "gpt-5",
      "input": "hi"
    });

    let parsed = ResponsesParser.parse(headers, body);
    assert!(parsed.meta.stream);
  }

  #[test]
  fn explicit_stream_flag_overrides_accept_header() {
    let mut headers = HeaderMap::new();
    headers.insert(
      axum::http::header::ACCEPT,
      HeaderValue::from_static("text/event-stream"),
    );
    let body = json!({
      "model": "gpt-5",
      "stream": false,
      "input": "hi"
    });

    let parsed = ResponsesParser.parse(headers, body);
    assert!(!parsed.meta.stream);
  }

  #[test]
  fn build_failure_result_event_from_api_err_preserves_not_implemented_envelope() {
    let api_err = ApiError::not_implemented("messages", "claude-sonnet-4-6");
    let event = build_failure_result_event_from_api_err("req-3".into(), 0, Instant::now(), &api_err, None);
    let Event::LegacyRequest(LegacyRequestEvent::Result {
      inbound_status,
      inbound_resp_body,
      request_error,
      ..
    }) = event
    else {
      panic!("wrong variant");
    };
    assert_eq!(inbound_status, 501);
    let err_msg = request_error.expect("request_error populated");
    assert!(err_msg.contains("messages"));
    assert!(err_msg.contains("claude-sonnet-4-6"));
    let envelope: serde_json::Value = serde_json::from_slice(&inbound_resp_body).unwrap();
    assert_eq!(envelope["error"]["code"], 501);
    assert_eq!(envelope["error"]["type"], "not_implemented_error");
    assert!(envelope["error"]["message"]
      .as_str()
      .unwrap()
      .contains("claude-sonnet-4-6"));
  }

}
