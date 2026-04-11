use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, CONTENT_TYPE};
use serde_json::{json, Map, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::utils;
use super::{
  join_upstream_url, ProviderAdapter, ProviderCapabilities, ProviderError, ProviderOperation, ProviderStream,
  ProviderStreamResponse, UpstreamLogContext,
};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct ClaudeAdapter {
  client: reqwest::Client,
}

impl ClaudeAdapter {
  pub fn new() -> Self {
    Self {
      client: reqwest::Client::new(),
    }
  }

  fn headers(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
  ) -> Result<HeaderMap, ProviderError> {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
      HeaderName::from_static("anthropic-version"),
      HeaderValue::from_static(ANTHROPIC_VERSION),
    );
    if let Some(token) = creds.and_then(|c| c.api_key.clone()) {
      let value = HeaderValue::from_str(&token).map_err(|e| ProviderError::internal(e.to_string()))?;
      headers.insert(HeaderName::from_static("x-api-key"), value);
    }
    utils::apply_config_headers(&mut headers, &config.headers);
    Ok(headers)
  }

  fn to_anthropic_request(route: &ModelRoute, request_body: Value, stream: bool) -> Value {
    let mut out = Map::new();
    out.insert("model".to_string(), Value::String(route.provider_model.clone()));
    out.insert("stream".to_string(), Value::Bool(stream));
    out.insert("max_tokens".to_string(), Value::Number(1024u64.into()));

    if let Some(max_tokens) = request_body.get("max_tokens").cloned() {
      out.insert("max_tokens".to_string(), max_tokens);
    } else if let Some(max_output_tokens) = request_body.get("max_output_tokens").cloned() {
      out.insert("max_tokens".to_string(), max_output_tokens);
    }
    if let Some(temperature) = request_body.get("temperature").cloned() {
      out.insert("temperature".to_string(), temperature);
    }
    if let Some(top_p) = request_body.get("top_p").cloned() {
      out.insert("top_p".to_string(), top_p);
    }
    if let Some(stop) = request_body.get("stop").cloned() {
      out.insert("stop_sequences".to_string(), stop);
    }

    let messages = if let Some(messages) = request_body.get("messages") {
      messages.clone()
    } else {
      Self::input_to_messages(request_body.get("input"))
    };
    out.insert("messages".to_string(), normalize_messages(messages));

    if let Some(system) = request_body.get("system").cloned() {
      out.insert("system".to_string(), system);
    }

    Value::Object(out)
  }

  fn input_to_messages(input: Option<&Value>) -> Value {
    match input {
      Some(Value::String(text)) => json!([{ "role": "user", "content": text }]),
      Some(Value::Array(items)) => {
        let text = items
          .iter()
          .map(|v| {
            if let Some(s) = v.as_str() {
              s.to_string()
            } else if let Some(obj) = v.as_object() {
              obj.get("text").and_then(|x| x.as_str()).unwrap_or_default().to_string()
            } else {
              String::new()
            }
          })
          .filter(|s| !s.is_empty())
          .collect::<Vec<String>>()
          .join("\n");
        json!([{ "role": "user", "content": text }])
      }
      _ => json!([{ "role": "user", "content": "" }]),
    }
  }

  async fn post_json(
    &self,
    log_ctx: UpstreamLogContext,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    body: Value,
  ) -> Result<Value, ProviderError> {
    let started = log_ctx.started(&body);
    let res = self
      .client
      .post(join_upstream_url(&config.base_url, &log_ctx.upstream_path))
      .headers(self.headers(config, creds)?)
      .json(&body)
      .send()
      .await
      .map_err(|e| {
        log_ctx.failed(started, None, Some(&e.to_string()));
        ProviderError::http(e.to_string())
      })?;

    let status = res.status();
    if status.as_u16() == 401 {
      log_ctx.failed(started, Some(401), Some("unauthorized"));
      return Err(ProviderError::Unauthorized { status_code: 401 });
    }
    if !status.is_success() {
      let details = res.text().await.unwrap_or_default();
      log_ctx.failed(started, Some(status.as_u16()), Some(&details));
      return Err(ProviderError::http_with_status(
        format!("upstream returned status {status}: {details}"),
        status.as_u16(),
      ));
    }

    let parsed = res.json::<Value>().await.map_err(|e| {
      log_ctx.failed(started, Some(status.as_u16()), Some(&e.to_string()));
      ProviderError::http_with_status(e.to_string(), status.as_u16())
    })?;
    log_ctx.completed(started, status.as_u16());
    Ok(parsed)
  }

  async fn post_stream(
    &self,
    log_ctx: UpstreamLogContext,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    body: Value,
    mode: StreamMode,
    route: ModelRoute,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let started = log_ctx.started(&body);
    let res = self
      .client
      .post(join_upstream_url(&config.base_url, &log_ctx.upstream_path))
      .headers(self.headers(config, creds)?)
      .json(&body)
      .send()
      .await
      .map_err(|e| {
        log_ctx.failed(started, None, Some(&e.to_string()));
        ProviderError::http(e.to_string())
      })?;

    let status = res.status();
    if status.as_u16() == 401 {
      log_ctx.failed(started, Some(401), Some("unauthorized"));
      return Err(ProviderError::Unauthorized { status_code: 401 });
    }
    if !status.is_success() {
      let details = res.text().await.unwrap_or_default();
      log_ctx.failed(started, Some(status.as_u16()), Some(&details));
      return Err(ProviderError::http_with_status(
        format!("upstream returned status {status}"),
        status.as_u16(),
      ));
    }

    log_ctx.completed(started, status.as_u16());
    Ok(ProviderStreamResponse {
      stream: normalize_anthropic_sse(res, mode, route),
      upstream_status: status.as_u16(),
    })
  }
}

#[async_trait]
impl ProviderAdapter for ClaudeAdapter {
  fn name(&self) -> &'static str {
    "claude"
  }

  fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
    ProviderCapabilities::all()
  }

  fn upstream_request_body(
    &self,
    _operation: ProviderOperation,
    stream: bool,
    route: &ModelRoute,
    _provider: &ProviderDefinition,
    request_body: &Value,
  ) -> Value {
    Self::to_anthropic_request(route, request_body.clone(), stream)
  }

  fn upstream_path(
    &self,
    _operation: ProviderOperation,
    _stream: bool,
    _route: &ModelRoute,
    _provider: &ProviderDefinition,
  ) -> String {
    "/v1/messages".to_string()
  }

  async fn chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let anthropic_req = Self::to_anthropic_request(route, request_body, false);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/messages".to_string(),
      method: "POST",
      model: anthropic_req.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    let upstream = self.post_json(ctx, config, creds, anthropic_req).await?;
    Ok(anthropic_to_chat_completion(upstream, route))
  }

  async fn responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let anthropic_req = Self::to_anthropic_request(route, request_body, false);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/messages".to_string(),
      method: "POST",
      model: anthropic_req.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    let upstream = self.post_json(ctx, config, creds, anthropic_req).await?;
    Ok(anthropic_to_response(upstream, route))
  }

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let anthropic_req = Self::to_anthropic_request(route, request_body, true);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/messages".to_string(),
      method: "POST",
      model: anthropic_req.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    self
      .post_stream(ctx, config, creds, anthropic_req, StreamMode::Chat, route.clone())
      .await
  }

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let anthropic_req = Self::to_anthropic_request(route, request_body, true);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/messages".to_string(),
      method: "POST",
      model: anthropic_req.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    self
      .post_stream(ctx, config, creds, anthropic_req, StreamMode::Responses, route.clone())
      .await
  }
}

#[derive(Clone, Copy)]
enum StreamMode {
  Chat,
  Responses,
}

fn normalize_messages(messages: Value) -> Value {
  match messages {
    Value::Array(items) => Value::Array(
      items
        .into_iter()
        .map(|item| {
          let Some(obj) = item.as_object() else {
            return item;
          };
          let role = obj.get("role").and_then(|v| v.as_str()).unwrap_or("user");
          let content = obj
            .get("content")
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
          let normalized_content = match content {
            Value::String(text) => json!([{ "type": "text", "text": text }]),
            Value::Array(arr) => Value::Array(arr),
            other => json!([{ "type": "text", "text": other.to_string() }]),
          };
          json!({ "role": role, "content": normalized_content })
        })
        .collect(),
    ),
    _ => json!([{ "role": "user", "content": [{ "type":"text", "text": "" }] }]),
  }
}

fn anthropic_to_chat_completion(upstream: Value, route: &ModelRoute) -> Value {
  let text = extract_anthropic_text(&upstream);
  let usage = usage_from_anthropic(&upstream);
  let created = now_unix();
  json!({
    "id": upstream.get("id").cloned().unwrap_or_else(|| Value::String("chatcmpl-anthropic".to_string())),
    "object": "chat.completion",
    "created": created,
    "model": route.openai_name,
    "choices": [{
      "index": 0,
      "message": { "role": "assistant", "content": text },
      "finish_reason": "stop"
    }],
    "usage": usage
  })
}

fn anthropic_to_response(upstream: Value, route: &ModelRoute) -> Value {
  let text = extract_anthropic_text(&upstream);
  let usage = usage_from_anthropic(&upstream);
  json!({
    "id": upstream.get("id").cloned().unwrap_or_else(|| Value::String("resp-anthropic".to_string())),
    "object": "response",
    "status": "completed",
    "model": route.openai_name,
    "output": [{
      "type": "message",
      "role": "assistant",
      "content": [{ "type": "output_text", "text": text }]
    }],
    "output_text": text,
    "usage": usage
  })
}

fn extract_anthropic_text(upstream: &Value) -> String {
  upstream
    .get("content")
    .and_then(|v| v.as_array())
    .map(|items| {
      items
        .iter()
        .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
        .collect::<Vec<&str>>()
        .join("")
    })
    .unwrap_or_default()
}

fn usage_from_anthropic(upstream: &Value) -> Value {
  let prompt_tokens = upstream
    .get("usage")
    .and_then(|u| u.get("input_tokens"))
    .and_then(|v| v.as_u64())
    .unwrap_or(0);
  let completion_tokens = upstream
    .get("usage")
    .and_then(|u| u.get("output_tokens"))
    .and_then(|v| v.as_u64())
    .unwrap_or(0);
  json!({
    "prompt_tokens": prompt_tokens,
    "completion_tokens": completion_tokens,
    "total_tokens": prompt_tokens + completion_tokens
  })
}

fn now_unix() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .map(|d| d.as_secs())
    .unwrap_or(0)
}

fn normalize_anthropic_sse(res: reqwest::Response, mode: StreamMode, route: ModelRoute) -> ProviderStream {
  let (tx, rx) = mpsc::channel::<Result<String, ProviderError>>(32);
  tokio::spawn(async move {
    let mut upstream = res.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = upstream.next().await {
      let bytes = match chunk {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::http(err.to_string()))).await;
          break;
        }
      };
      let part = match String::from_utf8(bytes.to_vec()) {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::internal(err.to_string()))).await;
          break;
        }
      };
      buffer.push_str(&part);
      while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer = buffer[idx + 2..].to_string();
        let converted = match convert_anthropic_frame(&frame, mode, &route) {
          Ok(v) => v,
          Err(err) => {
            let _ = tx.send(Err(err)).await;
            return;
          }
        };
        for payload in converted {
          if tx.send(Ok(payload)).await.is_err() {
            return;
          }
        }
      }
    }
  });
  Box::pin(ReceiverStream::new(rx))
}

fn convert_anthropic_frame(frame: &str, mode: StreamMode, route: &ModelRoute) -> Result<Vec<String>, ProviderError> {
  let mut event_name: Option<String> = None;
  let mut data_lines: Vec<String> = Vec::new();
  for raw in frame.lines() {
    let line = raw.trim_end_matches('\r');
    if let Some(e) = line.strip_prefix("event:") {
      event_name = Some(e.trim().to_string());
    } else if let Some(d) = line.strip_prefix("data:") {
      data_lines.push(d.trim_start().to_string());
    }
  }
  if data_lines.is_empty() {
    return Ok(Vec::new());
  }
  let payload = data_lines.join("\n");
  if payload == "[DONE]" {
    return Ok(vec!["[DONE]".to_string()]);
  }
  let value: Value = serde_json::from_str(&payload).map_err(|e| ProviderError::internal(e.to_string()))?;
  let event = event_name
    .or_else(|| value.get("type").and_then(|v| v.as_str()).map(|s| s.to_string()))
    .unwrap_or_default();
  let id = value
    .get("message")
    .and_then(|m| m.get("id"))
    .or_else(|| value.get("id"))
    .and_then(|v| v.as_str())
    .unwrap_or("anthropic-stream");

  match mode {
    StreamMode::Chat => anthropic_event_to_chat_chunks(&event, &value, route, id),
    StreamMode::Responses => anthropic_event_to_response_chunks(&event, &value),
  }
}

fn anthropic_event_to_chat_chunks(
  event: &str,
  value: &Value,
  route: &ModelRoute,
  id: &str,
) -> Result<Vec<String>, ProviderError> {
  match event {
    "message_start" => Ok(vec![json!({
      "id": id,
      "object": "chat.completion.chunk",
      "created": now_unix(),
      "model": route.openai_name,
      "choices": [{"index":0, "delta":{"role":"assistant"}, "finish_reason": Value::Null}]
    })
    .to_string()]),
    "content_block_delta" => {
      let text = value
        .get("delta")
        .and_then(|d| d.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();
      if text.is_empty() {
        return Ok(Vec::new());
      }
      Ok(vec![json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_unix(),
        "model": route.openai_name,
        "choices": [{"index":0, "delta":{"content": text}, "finish_reason": Value::Null}]
      })
      .to_string()])
    }
    "message_delta" => {
      let reason = value
        .get("delta")
        .and_then(|d| d.get("stop_reason"))
        .cloned()
        .unwrap_or_else(|| Value::String("stop".to_string()));
      Ok(vec![json!({
        "id": id,
        "object": "chat.completion.chunk",
        "created": now_unix(),
        "model": route.openai_name,
        "choices": [{"index":0, "delta":{}, "finish_reason": reason}]
      })
      .to_string()])
    }
    "message_stop" => Ok(vec!["[DONE]".to_string()]),
    _ => Ok(Vec::new()),
  }
}

fn anthropic_event_to_response_chunks(event: &str, value: &Value) -> Result<Vec<String>, ProviderError> {
  match event {
    "message_start" => Ok(vec![
      json!({"type":"response.created","response":{"status":"in_progress"}}).to_string(),
    ]),
    "content_block_delta" => {
      let text = value
        .get("delta")
        .and_then(|d| d.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or_default();
      if text.is_empty() {
        return Ok(Vec::new());
      }
      Ok(vec![
        json!({"type":"response.output_text.delta","delta":text}).to_string()
      ])
    }
    "message_stop" => Ok(vec![
      json!({"type":"response.completed"}).to_string(),
      "[DONE]".to_string(),
    ]),
    _ => Ok(Vec::new()),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::collections::HashMap;
  use std::net::SocketAddr;

  use axum::extract::State;
  use axum::http::StatusCode;
  use axum::routing::post;
  use axum::Router;
  use tokio::sync::oneshot;
  use tracing::Instrument;
  use tracing_subscriber::layer::SubscriberExt;

  use crate::db::logging::{LogCaptureLayer, LogQuery, LogStore};

  #[derive(Clone)]
  struct UpstreamStub {
    status: StatusCode,
    body: String,
  }

  async fn stub_handler(State(stub): State<UpstreamStub>) -> (StatusCode, [(String, String); 1], String) {
    (
      stub.status,
      [("content-type".to_string(), "application/json".to_string())],
      stub.body,
    )
  }

  async fn start_stub_server(stub: UpstreamStub) -> (SocketAddr, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/v1/messages", post(stub_handler)).with_state(stub);
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
      let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async {
          let _ = rx.await;
        })
        .await;
    });
    (addr, tx)
  }

  fn route() -> ModelRoute {
    ModelRoute {
      openai_name: "claude-3-7-sonnet".to_string(),
      provider: "claude".to_string(),
      provider_model: "claude-3-7-sonnet-20250219".to_string(),
      is_default: true,
    }
  }

  fn provider_def(base_url: &str) -> ProviderDefinition {
    ProviderDefinition {
      provider_type: "claude".to_string(),
      base_url: base_url.to_string(),
      enabled: true,
      headers: HashMap::new(),
      metadata: HashMap::new(),
    }
  }

  #[test]
  fn builds_anthropic_request_from_chat_messages() {
    let route = ModelRoute {
      openai_name: "claude-3-7-sonnet".to_string(),
      provider: "claude".to_string(),
      provider_model: "claude-3-7-sonnet-20250219".to_string(),
      is_default: true,
    };
    let req = ClaudeAdapter::to_anthropic_request(
      &route,
      json!({
        "messages": [{"role":"user","content":"hello"}],
        "temperature": 0.2
      }),
      false,
    );

    assert_eq!(
      req.get("model").and_then(|v| v.as_str()),
      Some("claude-3-7-sonnet-20250219")
    );
    assert_eq!(req.get("stream").and_then(|v| v.as_bool()), Some(false));
    assert_eq!(req.get("temperature").and_then(|v| v.as_f64()), Some(0.2));
    assert!(req.get("messages").is_some());
  }

  #[test]
  fn maps_anthropic_sync_to_openai_shapes() {
    let route = ModelRoute {
      openai_name: "claude-3-7-sonnet".to_string(),
      provider: "claude".to_string(),
      provider_model: "claude-3-7-sonnet-20250219".to_string(),
      is_default: true,
    };
    let upstream = json!({
      "id": "msg_1",
      "content": [{"type":"text","text":"hello world"}],
      "usage": {"input_tokens": 3, "output_tokens": 5}
    });
    let chat = anthropic_to_chat_completion(upstream.clone(), &route);
    let resp = anthropic_to_response(upstream, &route);
    assert_eq!(chat.get("object").and_then(|v| v.as_str()), Some("chat.completion"));
    assert_eq!(resp.get("object").and_then(|v| v.as_str()), Some("response"));
    assert_eq!(resp.get("output_text").and_then(|v| v.as_str()), Some("hello world"));
  }

  #[test]
  fn converts_anthropic_stream_frame_to_chat_chunk() {
    let route = ModelRoute {
      openai_name: "claude-3-7-sonnet".to_string(),
      provider: "claude".to_string(),
      provider_model: "claude-3-7-sonnet-20250219".to_string(),
      is_default: true,
    };
    let frame = "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n";
    let chunks = convert_anthropic_frame(frame, StreamMode::Chat, &route).expect("convert");
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].contains("\"chat.completion.chunk\""));
    assert!(chunks[0].contains("\"content\":\"hi\""));
  }

  #[tokio::test]
  async fn logs_upstream_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 1_000).expect("store");
    let subscriber = tracing_subscriber::registry().with(LogCaptureLayer::new(store.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);

    let adapter = ClaudeAdapter::new();
    let (addr, shutdown) = start_stub_server(UpstreamStub {
      status: StatusCode::NOT_FOUND,
      body: r#"{"error":"no such model"}"#.to_string(),
    })
    .await;
    let config = provider_def(&format!("http://{addr}"));
    let span = tracing::info_span!("http.request", request_id = "req-claude-err");
    async {
      let err = adapter
        .chat_completion(
          &config,
          None,
          &route(),
          json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .expect_err("expected failure");
      assert!(err.to_string().contains("upstream returned status 404"));
    }
    .instrument(span)
    .await;
    let _ = shutdown.send(());

    let logs = store
      .query(LogQuery {
        limit: Some(200),
        level: None,
        request_id: Some("req-claude-err".to_string()),
      })
      .expect("query");
    let failed = logs
      .iter()
      .find(|l| l.message == "upstream request failed")
      .expect("failed log");
    assert_eq!(failed.metadata.get("provider").map(String::as_str), Some("claude"));
    assert_eq!(failed.metadata.get("status").map(String::as_str), Some("404"));
  }
}
