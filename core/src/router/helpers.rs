use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::Json;
use chrono::Utc;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::app_state::AppState;
use crate::db::{
  ChatHistoryRecord, ChatMessageRecord, RequestRecordCompleted, RequestRecordFailed, RequestRecordStart, TokenUsage,
  UsageRecord,
};
use crate::providers::{ProviderError, ProviderStream};

use super::{StreamPersistence, StreamResponseKind};

pub(super) fn account_override_from_headers(headers: &HeaderMap) -> Option<String> {
  headers
    .get("x-llm-router-account-id")
    .and_then(|v| v.to_str().ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

pub(super) fn join_url(base: &str, path: &str) -> String {
  let left = base.trim_end_matches('/');
  let right = path.trim_start_matches('/');
  format!("{left}/{right}")
}

pub(super) fn persist_request_started(
  state: &Arc<AppState>,
  request_id: &str,
  endpoint: &str,
  route: &crate::config::ModelRoute,
  adapter_name: &str,
  account_id: Option<&str>,
  is_stream: bool,
  request_body: &Value,
  upstream_request_body: &Value,
) {
  let result = state.requests().record_request_started(RequestRecordStart {
    request_id: request_id.to_string(),
    created_at: Utc::now(),
    endpoint: endpoint.to_string(),
    provider: route.provider.clone(),
    adapter: adapter_name.to_string(),
    model: route.openai_name.clone(),
    account_id: account_id.map(ToString::to_string),
    is_stream,
    request_body_json: request_body.to_string(),
    upstream_request_body_json: upstream_request_body.to_string(),
  });
  if let Err(err) = result {
    tracing::warn!(target: "persistence", request_id = %request_id, error = %err, "failed to persist request start");
  }
}

pub(super) fn persist_request_completed(
  state: &Arc<AppState>,
  request_id: &str,
  http_status: Option<u16>,
  response_body: Option<&Value>,
  response_sse_text: Option<String>,
  usage: TokenUsage,
  started_at: Instant,
) {
  let result = state.requests().record_request_completed(RequestRecordCompleted {
    request_id: request_id.to_string(),
    completed_at: Utc::now(),
    response_body_json: response_body.map(Value::to_string),
    response_sse_text,
    http_status,
    usage,
    latency_ms: started_at.elapsed().as_millis() as i64,
  });
  if let Err(err) = result {
    tracing::warn!(
      target: "persistence",
      request_id = %request_id,
      error = %err,
      "failed to persist request completion"
    );
  }
}

pub(super) fn persist_request_failed(
  state: &Arc<AppState>,
  request_id: &str,
  http_status: Option<u16>,
  error_text: &str,
  response_sse_text: Option<String>,
  started_at: Instant,
) {
  let result = state.requests().record_request_failed(RequestRecordFailed {
    request_id: request_id.to_string(),
    completed_at: Utc::now(),
    http_status,
    error_text: error_text.to_string(),
    response_sse_text,
    latency_ms: started_at.elapsed().as_millis() as i64,
  });
  if let Err(err) = result {
    tracing::warn!(target: "persistence", request_id = %request_id, error = %err, "failed to persist request failure");
  }
}

pub(super) fn persist_provider_error(
  state: &Arc<AppState>,
  request_id: &str,
  err: &ProviderError,
  response_sse_text: Option<String>,
  started_at: Instant,
) {
  persist_request_failed(
    state,
    request_id,
    provider_error_status(err),
    &err.to_string(),
    response_sse_text,
    started_at,
  );
}

pub(super) fn persist_chat_history(
  state: &Arc<AppState>,
  request_id: &str,
  route: &crate::config::ModelRoute,
  account_id: Option<&str>,
  request_body: &Value,
  endpoint: &str,
) {
  let messages = extract_chat_messages(request_body, endpoint);
  if messages.is_empty() {
    return;
  }
  let result = state.requests().record_chat_history(ChatHistoryRecord {
    conversation_id: request_id.to_string(),
    created_at: Utc::now(),
    provider: route.provider.clone(),
    account_id: account_id.map(ToString::to_string),
    model: route.openai_name.clone(),
    latest_request_id: request_id.to_string(),
    messages,
  });
  if let Err(err) = result {
    tracing::warn!(target: "persistence", request_id = %request_id, error = %err, "failed to persist chat history");
  }
}

pub(super) fn persist_assistant_message_from_chat_completion(
  state: &Arc<AppState>,
  conversation_id: &str,
  payload: &Value,
) {
  let Some((text, raw_json)) = assistant_message_from_chat_completion(payload) else {
    return;
  };
  persist_assistant_message(state, conversation_id, &text, &raw_json);
}

pub(super) fn persist_assistant_message_from_response_payload(
  state: &Arc<AppState>,
  conversation_id: &str,
  payload: &Value,
) {
  let Some((text, raw_json)) = assistant_message_from_response_payload(payload) else {
    return;
  };
  persist_assistant_message(state, conversation_id, &text, &raw_json);
}

pub(super) fn apply_usage(
  state: &Arc<AppState>,
  provider: &str,
  account_id: Option<&str>,
  model: &str,
  usage: TokenUsage,
) {
  let result = state.requests().apply_usage(UsageRecord {
    used_at: Utc::now(),
    provider: provider.to_string(),
    account_id: account_id.map(ToString::to_string),
    model: model.to_string(),
    usage,
  });
  if let Err(err) = result {
    tracing::warn!(target: "persistence", provider = provider, model = model, error = %err, "failed to persist usage");
  }
}

pub(super) fn extract_usage(value: &Value) -> TokenUsage {
  let usage = value.get("usage").unwrap_or(value);
  let prompt = usage
    .get("prompt_tokens")
    .or_else(|| usage.get("input_tokens"))
    .and_then(Value::as_i64)
    .unwrap_or(0);
  let completion = usage
    .get("completion_tokens")
    .or_else(|| usage.get("output_tokens"))
    .and_then(Value::as_i64)
    .unwrap_or(0);
  let total = usage
    .get("total_tokens")
    .and_then(Value::as_i64)
    .unwrap_or(prompt + completion);
  TokenUsage {
    prompt_tokens: prompt.max(0),
    completion_tokens: completion.max(0),
    total_tokens: total.max(0),
  }
}

pub(super) fn sse_response(provider_stream: ProviderStream, persistence: Option<StreamPersistence>) -> Response {
  let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
  tokio::spawn(async move {
    futures::pin_mut!(provider_stream);
    let mut sse_capture = String::new();
    let mut assistant_text = String::new();
    let mut stream_id = None::<String>;
    let mut usage = TokenUsage::default();
    let mut saw_stream_error = None::<String>;
    let mut client_disconnected = false;

    while let Some(item) = provider_stream.next().await {
      let event = match item {
        Ok(chunk) => {
          if let Some(id) = extract_stream_id(&chunk) {
            stream_id = Some(id);
          }
          if let Some(next) = extract_usage_from_chunk(&chunk) {
            usage = next;
          }
          if let Some(delta) = extract_stream_text_delta(&chunk) {
            assistant_text.push_str(&delta);
          }
          sse_capture.push_str("data: ");
          sse_capture.push_str(&chunk);
          sse_capture.push_str("\n\n");
          Event::default().data(chunk)
        }
        Err(err) => {
          let payload = json!({"error": err.to_string()}).to_string();
          sse_capture.push_str("event: error\n");
          sse_capture.push_str("data: ");
          sse_capture.push_str(&payload);
          sse_capture.push_str("\n\n");
          saw_stream_error = Some(err.to_string());
          Event::default().event("error").data(payload)
        }
      };
      if tx.send(Ok(event)).await.is_err() {
        client_disconnected = true;
        break;
      }
      if saw_stream_error.is_some() {
        break;
      }
    }

    if let Some(p) = persistence {
      if let Some(err) = saw_stream_error {
        persist_request_failed(
          &p.state,
          &p.request_id,
          p.upstream_status,
          &err,
          Some(sse_capture),
          p.started_at,
        );
      } else if client_disconnected {
        persist_request_failed(
          &p.state,
          &p.request_id,
          None,
          "client disconnected before stream completed",
          Some(sse_capture),
          p.started_at,
        );
      } else {
        let final_payload = synthesize_stream_result(
          p.response_kind,
          p.model.as_str(),
          stream_id.as_deref(),
          assistant_text.as_str(),
          &usage,
        );
        persist_request_completed(
          &p.state,
          &p.request_id,
          p.upstream_status.or(Some(StatusCode::OK.as_u16())),
          Some(&final_payload),
          Some(sse_capture),
          usage.clone(),
          p.started_at,
        );
        apply_usage(
          &p.state,
          p.provider.as_str(),
          p.account_id.as_deref(),
          p.model.as_str(),
          usage,
        );
        if !assistant_text.trim().is_empty() {
          let raw_json = json!({"role":"assistant","content": assistant_text}).to_string();
          persist_assistant_message(&p.state, &p.request_id, &assistant_text, &raw_json);
        }
      }
    }
  });

  Sse::new(ReceiverStream::new(rx))
    .keep_alive(KeepAlive::default())
    .into_response()
}

pub(super) fn provider_error_response(err: ProviderError) -> Response {
  match err {
    ProviderError::Unauthorized { .. } => json_error(StatusCode::UNAUTHORIZED, "unauthorized"),
    ProviderError::Unsupported(msg) => json_error(StatusCode::NOT_IMPLEMENTED, &msg),
    ProviderError::Http { message, .. } | ProviderError::Internal { message, .. } => {
      json_error(StatusCode::BAD_GATEWAY, &message)
    }
  }
}

pub(super) fn json_error(status: StatusCode, message: &str) -> Response {
  (status, Json(json!({ "error": message }))).into_response()
}

fn persist_assistant_message(state: &Arc<AppState>, conversation_id: &str, text: &str, raw_json: &str) {
  if text.trim().is_empty() {
    return;
  }
  let result = state
    .requests()
    .append_chat_message(conversation_id, Utc::now(), "assistant", text, raw_json);
  if let Err(err) = result {
    tracing::warn!(
      target: "persistence",
      conversation_id = %conversation_id,
      error = %err,
      "failed to append assistant message"
    );
  }
}

fn extract_chat_messages(request_body: &Value, endpoint: &str) -> Vec<ChatMessageRecord> {
  if endpoint == "/v1/chat/completions" {
    return json_messages_to_records(request_body.get("messages"));
  }

  if let Some(records) = request_body
    .get("messages")
    .map(Some)
    .map(json_messages_to_records)
    .filter(|v| !v.is_empty())
  {
    return records;
  }

  let text = input_to_text(request_body.get("input"));
  if text.trim().is_empty() {
    return Vec::new();
  }
  vec![ChatMessageRecord {
    role: "user".to_string(),
    content_text: text.clone(),
    raw_json: json!({"role":"user","content": text}).to_string(),
  }]
}

fn json_messages_to_records(messages: Option<&Value>) -> Vec<ChatMessageRecord> {
  let Some(Value::Array(items)) = messages else {
    return Vec::new();
  };
  let mut out = Vec::with_capacity(items.len());
  for item in items {
    let role = item.get("role").and_then(Value::as_str).unwrap_or("user").to_string();
    let content = item.get("content").map(content_to_text).unwrap_or_default();
    out.push(ChatMessageRecord {
      role,
      content_text: content,
      raw_json: item.to_string(),
    });
  }
  out
}

fn extract_usage_from_chunk(chunk: &str) -> Option<TokenUsage> {
  let parsed: Value = serde_json::from_str(chunk).ok()?;
  let usage = extract_usage(&parsed);
  if usage.prompt_tokens == 0 && usage.completion_tokens == 0 && usage.total_tokens == 0 {
    None
  } else {
    Some(usage)
  }
}

fn extract_stream_text_delta(chunk: &str) -> Option<String> {
  let parsed: Value = serde_json::from_str(chunk).ok()?;
  if let Some(delta) = parsed
    .get("choices")
    .and_then(Value::as_array)
    .and_then(|items| items.first())
    .and_then(|choice| choice.get("delta"))
    .and_then(|delta| delta.get("content"))
    .and_then(Value::as_str)
  {
    return Some(delta.to_string());
  }
  if parsed.get("type").and_then(Value::as_str) == Some("response.output_text.delta") {
    return parsed.get("delta").and_then(Value::as_str).map(ToString::to_string);
  }
  None
}

fn assistant_message_from_chat_completion(payload: &Value) -> Option<(String, String)> {
  let message = payload
    .get("choices")
    .and_then(Value::as_array)
    .and_then(|items| items.first())
    .and_then(|choice| choice.get("message"))?;
  let text = message
    .get("content")
    .map(content_to_text)
    .unwrap_or_default()
    .trim()
    .to_string();
  if text.is_empty() {
    return None;
  }
  Some((text, message.to_string()))
}

fn assistant_message_from_response_payload(payload: &Value) -> Option<(String, String)> {
  if let Some(message) = payload
    .get("output")
    .and_then(Value::as_array)
    .and_then(|items| items.first())
  {
    let text = response_to_text(payload).trim().to_string();
    if !text.is_empty() {
      return Some((text, message.to_string()));
    }
  }
  let text = response_to_text(payload).trim().to_string();
  if text.is_empty() {
    None
  } else {
    Some((text.clone(), json!({"role":"assistant","content": text}).to_string()))
  }
}

fn provider_error_status(err: &ProviderError) -> Option<u16> {
  match err {
    ProviderError::Unsupported(_) => Some(StatusCode::NOT_IMPLEMENTED.as_u16()),
    _ => err.status_code(),
  }
}

fn extract_stream_id(chunk: &str) -> Option<String> {
  let parsed: Value = serde_json::from_str(chunk).ok()?;
  parsed
    .get("id")
    .or_else(|| parsed.get("response").and_then(|v| v.get("id")))
    .and_then(Value::as_str)
    .map(ToString::to_string)
}

fn synthesize_stream_result(
  response_kind: StreamResponseKind,
  model: &str,
  stream_id: Option<&str>,
  assistant_text: &str,
  usage: &TokenUsage,
) -> Value {
  let usage_chat = json!({
    "prompt_tokens": usage.prompt_tokens,
    "completion_tokens": usage.completion_tokens,
    "total_tokens": usage.total_tokens
  });
  match response_kind {
    StreamResponseKind::ChatCompletions => json!({
      "id": stream_id.unwrap_or("chatcmpl-stream"),
      "object": "chat.completion",
      "created": 0,
      "model": model,
      "choices": [{
        "index": 0,
        "message": {"role":"assistant","content": assistant_text},
        "finish_reason": "stop"
      }],
      "usage": usage_chat
    }),
    StreamResponseKind::Responses => json!({
      "id": stream_id.unwrap_or("resp-stream"),
      "object": "response",
      "status": "completed",
      "model": model,
      "output": [{
        "type": "message",
        "role": "assistant",
        "content": [{"type":"output_text","text": assistant_text}]
      }],
      "output_text": assistant_text,
      "usage": {
        "input_tokens": usage.prompt_tokens,
        "output_tokens": usage.completion_tokens,
        "total_tokens": usage.total_tokens
      }
    }),
  }
}

fn response_to_text(response: &Value) -> String {
  if let Some(text) = response.get("output_text").and_then(|v| v.as_str()) {
    return text.to_string();
  }
  response
    .get("output")
    .and_then(|v| v.as_array())
    .and_then(|items| items.first())
    .and_then(|item| item.get("content"))
    .and_then(|v| v.as_array())
    .and_then(|items| items.first())
    .and_then(|item| item.get("text"))
    .and_then(|v| v.as_str())
    .unwrap_or_default()
    .to_string()
}

fn input_to_text(input: Option<&Value>) -> String {
  match input {
    Some(Value::String(s)) => s.clone(),
    Some(Value::Array(items)) => items.iter().map(content_to_text).collect::<Vec<String>>().join("\n"),
    Some(other) => content_to_text(other),
    None => String::new(),
  }
}

fn content_to_text(content: &Value) -> String {
  if let Some(s) = content.as_str() {
    return s.to_string();
  }
  if let Some(obj) = content.as_object() {
    if let Some(text) = obj.get("text").and_then(|v| v.as_str()) {
      return text.to_string();
    }
  }
  if let Some(arr) = content.as_array() {
    return arr
      .iter()
      .map(|item| {
        if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
          text.to_string()
        } else if let Some(text) = item.get("content").and_then(|v| v.as_str()) {
          text.to_string()
        } else if let Some(text) = item.as_str() {
          text.to_string()
        } else {
          String::new()
        }
      })
      .filter(|s| !s.is_empty())
      .collect::<Vec<String>>()
      .join("\n");
  }
  String::new()
}
