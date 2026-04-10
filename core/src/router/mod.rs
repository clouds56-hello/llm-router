use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Extension, Query, State};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{middleware, middleware::Next};
use axum::{Json, Router};
use chrono::Utc;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::Instrument;
use uuid::Uuid;

use crate::app_state::AppState;
use crate::logging::LogQuery;
use crate::persistence::{
  ChatHistoryRecord, ChatMessageRecord, RequestRecordCompleted, RequestRecordFailed, RequestRecordStart, TokenUsage,
  UsageRecord,
};
use crate::providers::{ProviderError, ProviderStream};

#[derive(Clone)]
struct RequestContext {
  request_id: String,
  started_at: Instant,
}

#[derive(Clone)]
struct StreamPersistence {
  state: Arc<AppState>,
  request_id: String,
  provider: String,
  account_id: Option<String>,
  model: String,
  started_at: Instant,
}

pub fn build_router(state: Arc<AppState>) -> Router {
  Router::new()
    .route("/health", get(health))
    .route("/v1/chat/completions", post(chat_completions))
    .route("/v1/responses", post(responses))
    .route("/api/providers/status", get(provider_status))
    .route("/api/models", get(model_list))
    .route("/api/config", get(active_config))
    .route("/api/logs", get(request_logs))
    .with_state(state)
    .layer(middleware::from_fn(with_request_span))
    .layer(CorsLayer::permissive())
    .layer(TraceLayer::new_for_http())
}

async fn health() -> Json<Value> {
  Json(json!({ "ok": true }))
}

async fn provider_status(State(state): State<Arc<AppState>>) -> Json<Value> {
  let loaded = state.config().get();
  Json(json!({ "providers": state.providers().provider_status(&loaded) }))
}

async fn model_list(State(state): State<Arc<AppState>>) -> Json<Value> {
  let loaded = state.config().get();
  let models: Vec<Value> = loaded
    .models
    .models
    .iter()
    .map(|m| {
      json!({
          "name": m.openai_name,
          "provider": m.provider,
          "provider_model": m.provider_model,
          "is_default": m.is_default,
          "enabled": loaded.is_model_enabled(&m.openai_name),
      })
    })
    .collect();

  Json(json!({ "models": models }))
}

async fn active_config(State(state): State<Arc<AppState>>) -> Json<Value> {
  let loaded = state.config().get();
  Json(json!({
      "providers": loaded.providers,
      "models": loaded.models,
      "credentials": loaded.credentials,
      "last_error": state.config().last_error(),
  }))
}

#[derive(Debug, Deserialize)]
struct RequestLogsQuery {
  limit: Option<usize>,
  level: Option<String>,
  request_id: Option<String>,
}

async fn request_logs(State(state): State<Arc<AppState>>, Query(query): Query<RequestLogsQuery>) -> Response {
  let filter = LogQuery {
    limit: query.limit,
    level: query.level,
    request_id: query.request_id,
  };
  match state.logs().query(filter) {
    Ok(logs) => Json(json!({ "logs": logs })).into_response(),
    Err(err) => json_error(
      StatusCode::INTERNAL_SERVER_ERROR,
      &format!("failed to query logs: {err}"),
    ),
  }
}

async fn chat_completions(
  State(state): State<Arc<AppState>>,
  Extension(ctx): Extension<RequestContext>,
  headers: HeaderMap,
  Json(mut body): Json<Value>,
) -> Response {
  let loaded = state.config().get();
  let model_name = body.get("model").and_then(|v| v.as_str()).unwrap_or_default();

  let Some(route) = loaded.resolve_model(model_name) else {
    return json_error(StatusCode::BAD_REQUEST, "model routing config is empty");
  };

  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".to_string(), Value::String(route.openai_name.clone()));
  }

  let stream_requested = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  let request_body_for_storage = body.clone();

  let account_override = account_override_from_headers(&headers);
  let resolved = match state
    .providers()
    .adapter_for_provider(&loaded, route, account_override.as_deref())
  {
    Ok(v) => v,
    Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
  };
  let adapter = resolved.adapter;
  let provider_cfg = resolved.provider_cfg;
  let creds = resolved.creds;
  let effective_account_id = resolved.effective_account_id;

  tracing::info!(
    target: "router",
    model = route.openai_name,
    provider = route.provider,
    adapter = adapter.name(),
    "chat request"
  );

  let caps = adapter.capabilities(route);
  let upstream_path = if stream_requested {
    if caps.stream_chat_completion {
      "/v1/chat/completions"
    } else if caps.stream_responses {
      "/v1/responses"
    } else {
      "/v1/chat/completions"
    }
  } else if caps.chat_completion {
    "/v1/chat/completions"
  } else if caps.responses {
    "/v1/responses"
  } else {
    "/v1/chat/completions"
  };
  let upstream_endpoint = join_url(&provider_cfg.base_url, upstream_path);
  persist_request_started(
    &state,
    &ctx.request_id,
    &upstream_endpoint,
    route,
    adapter.name(),
    effective_account_id.as_deref(),
    stream_requested,
    &request_body_for_storage,
  );
  persist_chat_history(
    &state,
    &ctx.request_id,
    route,
    effective_account_id.as_deref(),
    &request_body_for_storage,
    "/v1/chat/completions",
  );

  if stream_requested {
    if caps.stream_chat_completion {
      return match adapter
        .stream_chat_completion(&provider_cfg, creds.as_ref(), route, body)
        .await
      {
        Ok(provider_stream) => sse_response(
          provider_stream,
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            started_at: ctx.started_at,
          }),
        ),
        Err(err) => {
          persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
          provider_error_response(err)
        }
      };
    }
    if caps.stream_responses {
      let converted = chat_request_to_response_request(body);
      return match adapter
        .stream_responses(&provider_cfg, creds.as_ref(), route, converted)
        .await
      {
        Ok(provider_stream) => sse_response(
          convert_response_stream_to_chat_stream(provider_stream, route),
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            started_at: ctx.started_at,
          }),
        ),
        Err(err) => {
          persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
          provider_error_response(err)
        }
      };
    }
    persist_request_failed(
      &state,
      &ctx.request_id,
      Some(StatusCode::NOT_IMPLEMENTED.as_u16()),
      &format!(
        "provider '{}' does not support streaming chat completions or streaming responses",
        adapter.name()
      ),
      None,
      ctx.started_at,
    );
    return json_error(
      StatusCode::NOT_IMPLEMENTED,
      &format!(
        "provider '{}' does not support streaming chat completions or streaming responses",
        adapter.name()
      ),
    );
  }

  if caps.chat_completion {
    return match adapter
      .chat_completion(&provider_cfg, creds.as_ref(), route, body)
      .await
    {
      Ok(data) => {
        let usage = extract_usage(&data);
        persist_request_completed(
          &state,
          &ctx.request_id,
          Some(StatusCode::OK.as_u16()),
          Some(&data),
          None,
          usage.clone(),
          ctx.started_at,
        );
        apply_usage(
          &state,
          route.provider.as_str(),
          effective_account_id.as_deref(),
          route.openai_name.as_str(),
          usage,
        );
        persist_assistant_message_from_chat_completion(&state, &ctx.request_id, &data);
        Json(data).into_response()
      }
      Err(err) => {
        persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
        provider_error_response(err)
      }
    };
  }

  if caps.responses {
    let converted = chat_request_to_response_request(body);
    return match adapter.responses(&provider_cfg, creds.as_ref(), route, converted).await {
      Ok(data) => {
        let chat = response_to_chat_completion(data.clone(), route);
        let usage = extract_usage(&data);
        persist_request_completed(
          &state,
          &ctx.request_id,
          Some(StatusCode::OK.as_u16()),
          Some(&chat),
          None,
          usage.clone(),
          ctx.started_at,
        );
        apply_usage(
          &state,
          route.provider.as_str(),
          effective_account_id.as_deref(),
          route.openai_name.as_str(),
          usage,
        );
        persist_assistant_message_from_chat_completion(&state, &ctx.request_id, &chat);
        Json(chat).into_response()
      }
      Err(err) => {
        persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
        provider_error_response(err)
      }
    };
  }

  persist_request_failed(
    &state,
    &ctx.request_id,
    Some(StatusCode::NOT_IMPLEMENTED.as_u16()),
    &format!(
      "provider '{}' does not support chat completions or responses",
      adapter.name()
    ),
    None,
    ctx.started_at,
  );
  json_error(
    StatusCode::NOT_IMPLEMENTED,
    &format!(
      "provider '{}' does not support chat completions or responses",
      adapter.name()
    ),
  )
}

async fn responses(
  State(state): State<Arc<AppState>>,
  Extension(ctx): Extension<RequestContext>,
  headers: HeaderMap,
  Json(mut body): Json<Value>,
) -> Response {
  let loaded = state.config().get();
  let model_name = body.get("model").and_then(|v| v.as_str()).unwrap_or_default();

  let Some(route) = loaded.resolve_model(model_name) else {
    return json_error(StatusCode::BAD_REQUEST, "model routing config is empty");
  };

  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".to_string(), Value::String(route.openai_name.clone()));
  }

  let stream_requested = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  let request_body_for_storage = body.clone();

  let account_override = account_override_from_headers(&headers);
  let resolved = match state
    .providers()
    .adapter_for_provider(&loaded, route, account_override.as_deref())
  {
    Ok(v) => v,
    Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
  };
  let adapter = resolved.adapter;
  let provider_cfg = resolved.provider_cfg;
  let creds = resolved.creds;
  let effective_account_id = resolved.effective_account_id;

  tracing::info!(
    target: "router",
    model = route.openai_name,
    provider = route.provider,
    adapter = adapter.name(),
    "responses request"
  );

  let caps = adapter.capabilities(route);
  let upstream_path = if stream_requested {
    if caps.stream_responses {
      "/v1/responses"
    } else if caps.stream_chat_completion {
      "/v1/chat/completions"
    } else {
      "/v1/responses"
    }
  } else if caps.responses {
    "/v1/responses"
  } else if caps.chat_completion {
    "/v1/chat/completions"
  } else {
    "/v1/responses"
  };
  let upstream_endpoint = join_url(&provider_cfg.base_url, upstream_path);
  persist_request_started(
    &state,
    &ctx.request_id,
    &upstream_endpoint,
    route,
    adapter.name(),
    effective_account_id.as_deref(),
    stream_requested,
    &request_body_for_storage,
  );
  persist_chat_history(
    &state,
    &ctx.request_id,
    route,
    effective_account_id.as_deref(),
    &request_body_for_storage,
    "/v1/responses",
  );

  if stream_requested {
    if caps.stream_responses {
      return match adapter
        .stream_responses(&provider_cfg, creds.as_ref(), route, body)
        .await
      {
        Ok(provider_stream) => sse_response(
          provider_stream,
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            started_at: ctx.started_at,
          }),
        ),
        Err(err) => {
          persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
          provider_error_response(err)
        }
      };
    }
    if caps.stream_chat_completion {
      let converted = response_request_to_chat_request(body);
      return match adapter
        .stream_chat_completion(&provider_cfg, creds.as_ref(), route, converted)
        .await
      {
        Ok(provider_stream) => sse_response(
          convert_chat_stream_to_response_stream(provider_stream),
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            started_at: ctx.started_at,
          }),
        ),
        Err(err) => {
          persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
          provider_error_response(err)
        }
      };
    }
    persist_request_failed(
      &state,
      &ctx.request_id,
      Some(StatusCode::NOT_IMPLEMENTED.as_u16()),
      &format!(
        "provider '{}' does not support streaming responses or streaming chat completions",
        adapter.name()
      ),
      None,
      ctx.started_at,
    );
    return json_error(
      StatusCode::NOT_IMPLEMENTED,
      &format!(
        "provider '{}' does not support streaming responses or streaming chat completions",
        adapter.name()
      ),
    );
  }

  if caps.responses {
    return match adapter.responses(&provider_cfg, creds.as_ref(), route, body).await {
      Ok(data) => {
        let usage = extract_usage(&data);
        persist_request_completed(
          &state,
          &ctx.request_id,
          Some(StatusCode::OK.as_u16()),
          Some(&data),
          None,
          usage.clone(),
          ctx.started_at,
        );
        apply_usage(
          &state,
          route.provider.as_str(),
          effective_account_id.as_deref(),
          route.openai_name.as_str(),
          usage,
        );
        persist_assistant_message_from_response_payload(&state, &ctx.request_id, &data);
        Json(data).into_response()
      }
      Err(err) => {
        persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
        provider_error_response(err)
      }
    };
  }

  if caps.chat_completion {
    let converted = response_request_to_chat_request(body);
    return match adapter
      .chat_completion(&provider_cfg, creds.as_ref(), route, converted)
      .await
    {
      Ok(data) => {
        let response = chat_completion_to_response(data.clone(), route);
        let usage = extract_usage(&data);
        persist_request_completed(
          &state,
          &ctx.request_id,
          Some(StatusCode::OK.as_u16()),
          Some(&response),
          None,
          usage.clone(),
          ctx.started_at,
        );
        apply_usage(
          &state,
          route.provider.as_str(),
          effective_account_id.as_deref(),
          route.openai_name.as_str(),
          usage,
        );
        persist_assistant_message_from_response_payload(&state, &ctx.request_id, &response);
        Json(response).into_response()
      }
      Err(err) => {
        persist_provider_error(&state, &ctx.request_id, &err, None, ctx.started_at);
        provider_error_response(err)
      }
    };
  }

  persist_request_failed(
    &state,
    &ctx.request_id,
    Some(StatusCode::NOT_IMPLEMENTED.as_u16()),
    &format!(
      "provider '{}' does not support responses or chat completions",
      adapter.name()
    ),
    None,
    ctx.started_at,
  );
  json_error(
    StatusCode::NOT_IMPLEMENTED,
    &format!(
      "provider '{}' does not support responses or chat completions",
      adapter.name()
    ),
  )
}

fn response_request_to_chat_request(body: Value) -> Value {
  let stream = body.get("stream").cloned();
  let model = body.get("model").cloned();
  let input = body.get("input").cloned();
  let mut out = if let Some(messages) = body.get("messages").cloned() {
    json!({ "messages": messages })
  } else {
    let text = input_to_text(input.as_ref());
    json!({
      "messages": [{"role":"user","content": text}]
    })
  };
  if let Some(obj) = out.as_object_mut() {
    if let Some(v) = stream {
      obj.insert("stream".to_string(), v);
    }
    if let Some(v) = model {
      obj.insert("model".to_string(), v);
    }
    if let Some(v) = body.get("temperature").cloned() {
      obj.insert("temperature".to_string(), v);
    }
    if let Some(v) = body.get("max_output_tokens").cloned() {
      obj.insert("max_tokens".to_string(), v);
    } else if let Some(v) = body.get("max_tokens").cloned() {
      obj.insert("max_tokens".to_string(), v);
    }
  }
  out
}

fn account_override_from_headers(headers: &HeaderMap) -> Option<String> {
  headers
    .get("x-llm-router-account-id")
    .and_then(|v| v.to_str().ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

fn chat_request_to_response_request(body: Value) -> Value {
  let model = body.get("model").cloned();
  let stream = body.get("stream").cloned();
  let messages = body.get("messages").cloned();
  let input = if let Some(messages) = messages {
    Value::String(messages_to_text(&messages))
  } else if let Some(input) = body.get("input").cloned() {
    input
  } else {
    Value::String(String::new())
  };
  let mut out = json!({ "input": input });
  if let Some(obj) = out.as_object_mut() {
    if let Some(v) = model {
      obj.insert("model".to_string(), v);
    }
    if let Some(v) = stream {
      obj.insert("stream".to_string(), v);
    }
    if let Some(v) = body.get("temperature").cloned() {
      obj.insert("temperature".to_string(), v);
    }
    if let Some(v) = body.get("max_tokens").cloned() {
      obj.insert("max_output_tokens".to_string(), v);
    } else if let Some(v) = body.get("max_output_tokens").cloned() {
      obj.insert("max_output_tokens".to_string(), v);
    }
  }
  out
}

fn chat_completion_to_response(chat: Value, route: &crate::config::ModelRoute) -> Value {
  let text = chat
    .get("choices")
    .and_then(|c| c.as_array())
    .and_then(|choices| choices.first())
    .and_then(|choice| choice.get("message"))
    .and_then(|m| m.get("content"))
    .and_then(|v| v.as_str())
    .unwrap_or_default()
    .to_string();
  json!({
    "id": chat.get("id").cloned().unwrap_or_else(|| Value::String("resp-converted".to_string())),
    "object": "response",
    "status": "completed",
    "model": route.openai_name,
    "output": [{
      "type": "message",
      "role": "assistant",
      "content": [{"type":"output_text","text": text}]
    }],
    "output_text": text,
  })
}

fn response_to_chat_completion(response: Value, route: &crate::config::ModelRoute) -> Value {
  let text = response_to_text(&response);
  json!({
    "id": response.get("id").cloned().unwrap_or_else(|| Value::String("chatcmpl-converted".to_string())),
    "object": "chat.completion",
    "created": 0,
    "model": route.openai_name,
    "choices": [{
      "index": 0,
      "message": {"role":"assistant","content": text},
      "finish_reason": "stop"
    }]
  })
}

fn convert_chat_stream_to_response_stream(provider_stream: ProviderStream) -> ProviderStream {
  let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, ProviderError>>(32);
  tokio::spawn(async move {
    futures::pin_mut!(provider_stream);
    while let Some(item) = provider_stream.next().await {
      let payload = match item {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(err)).await;
          return;
        }
      };
      if payload.trim() == "[DONE]" {
        if tx
          .send(Ok(json!({"type":"response.completed"}).to_string()))
          .await
          .is_err()
        {
          return;
        }
        let _ = tx.send(Ok("[DONE]".to_string())).await;
        return;
      }
      if let Ok(value) = serde_json::from_str::<Value>(&payload) {
        let delta = value
          .get("choices")
          .and_then(|c| c.as_array())
          .and_then(|choices| choices.first())
          .and_then(|choice| choice.get("delta"))
          .and_then(|d| d.get("content"))
          .and_then(|v| v.as_str())
          .unwrap_or_default();
        if !delta.is_empty() {
          if tx
            .send(Ok(
              json!({"type":"response.output_text.delta","delta":delta}).to_string(),
            ))
            .await
            .is_err()
          {
            return;
          }
        }
      }
    }
    let _ = tx.send(Ok(json!({"type":"response.completed"}).to_string())).await;
    let _ = tx.send(Ok("[DONE]".to_string())).await;
  });
  Box::pin(ReceiverStream::new(rx))
}

fn convert_response_stream_to_chat_stream(
  provider_stream: ProviderStream,
  route: &crate::config::ModelRoute,
) -> ProviderStream {
  let (tx, rx) = tokio::sync::mpsc::channel::<Result<String, ProviderError>>(32);
  let model = route.openai_name.clone();
  tokio::spawn(async move {
    futures::pin_mut!(provider_stream);
    let mut sent_role = false;
    while let Some(item) = provider_stream.next().await {
      let payload = match item {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(err)).await;
          return;
        }
      };
      if payload.trim() == "[DONE]" {
        if tx.send(Ok("[DONE]".to_string())).await.is_err() {
          return;
        }
        return;
      }
      let Ok(value) = serde_json::from_str::<Value>(&payload) else {
        continue;
      };
      if let Some(kind) = value.get("type").and_then(|v| v.as_str()) {
        if kind == "response.output_text.delta" {
          let delta = value.get("delta").and_then(|v| v.as_str()).unwrap_or_default();
          if !sent_role {
            let _ = tx
              .send(Ok(
                json!({
                  "id":"chatcmpl-converted",
                  "object":"chat.completion.chunk",
                  "created":0,
                  "model":model,
                  "choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":Value::Null}]
                })
                .to_string(),
              ))
              .await;
            sent_role = true;
          }
          if tx
            .send(Ok(
              json!({
                "id":"chatcmpl-converted",
                "object":"chat.completion.chunk",
                "created":0,
                "model":model,
                "choices":[{"index":0,"delta":{"content":delta},"finish_reason":Value::Null}]
              })
              .to_string(),
            ))
            .await
            .is_err()
          {
            return;
          }
        } else if kind == "response.completed" {
          let _ = tx
            .send(Ok(
              json!({
                "id":"chatcmpl-converted",
                "object":"chat.completion.chunk",
                "created":0,
                "model":model,
                "choices":[{"index":0,"delta":{},"finish_reason":"stop"}]
              })
              .to_string(),
            ))
            .await;
        }
      }
    }
  });
  Box::pin(ReceiverStream::new(rx))
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

fn messages_to_text(messages: &Value) -> String {
  messages
    .as_array()
    .map(|items| {
      items
        .iter()
        .filter_map(|item| item.get("content"))
        .map(content_to_text)
        .collect::<Vec<String>>()
        .join("\n")
    })
    .unwrap_or_default()
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

fn persist_request_started(
  state: &Arc<AppState>,
  request_id: &str,
  endpoint: &str,
  route: &crate::config::ModelRoute,
  adapter_name: &str,
  account_id: Option<&str>,
  is_stream: bool,
  request_body: &Value,
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
  });
  if let Err(err) = result {
    tracing::warn!(target: "persistence", request_id = %request_id, error = %err, "failed to persist request start");
  }
}

fn persist_request_completed(
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

fn persist_request_failed(
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

fn persist_provider_error(
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

fn persist_chat_history(
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

fn persist_assistant_message_from_chat_completion(state: &Arc<AppState>, conversation_id: &str, payload: &Value) {
  let Some((text, raw_json)) = assistant_message_from_chat_completion(payload) else {
    return;
  };
  persist_assistant_message(state, conversation_id, &text, &raw_json);
}

fn persist_assistant_message_from_response_payload(state: &Arc<AppState>, conversation_id: &str, payload: &Value) {
  let Some((text, raw_json)) = assistant_message_from_response_payload(payload) else {
    return;
  };
  persist_assistant_message(state, conversation_id, &text, &raw_json);
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

fn apply_usage(state: &Arc<AppState>, provider: &str, account_id: Option<&str>, model: &str, usage: TokenUsage) {
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

fn extract_usage(value: &Value) -> TokenUsage {
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
    ProviderError::Unauthorized => Some(StatusCode::UNAUTHORIZED.as_u16()),
    ProviderError::Unsupported(_) => Some(StatusCode::NOT_IMPLEMENTED.as_u16()),
    ProviderError::Http(msg) | ProviderError::Internal(msg) => infer_status_from_text(msg),
  }
}

fn infer_status_from_text(text: &str) -> Option<u16> {
  for token in text.split(|c: char| !c.is_ascii_digit()) {
    if token.len() == 3 {
      if let Ok(code) = token.parse::<u16>() {
        if (100..=599).contains(&code) {
          return Some(code);
        }
      }
    }
  }
  None
}

fn join_url(base: &str, path: &str) -> String {
  let left = base.trim_end_matches('/');
  let right = path.trim_start_matches('/');
  format!("{left}/{right}")
}

fn sse_response(provider_stream: ProviderStream, persistence: Option<StreamPersistence>) -> Response {
  let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
  tokio::spawn(async move {
    futures::pin_mut!(provider_stream);
    let mut sse_capture = String::new();
    let mut assistant_text = String::new();
    let mut usage = TokenUsage::default();
    let mut saw_stream_error = None::<String>;
    let mut client_disconnected = false;

    while let Some(item) = provider_stream.next().await {
      let event = match item {
        Ok(chunk) => {
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
          infer_status_from_text(&err),
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
        persist_request_completed(
          &p.state,
          &p.request_id,
          Some(StatusCode::OK.as_u16()),
          None,
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

fn provider_error_response(err: ProviderError) -> Response {
  match err {
    ProviderError::Unauthorized => json_error(StatusCode::UNAUTHORIZED, "unauthorized"),
    ProviderError::Unsupported(msg) => json_error(StatusCode::NOT_IMPLEMENTED, &msg),
    ProviderError::Http(msg) | ProviderError::Internal(msg) => json_error(StatusCode::BAD_GATEWAY, &msg),
  }
}

async fn with_request_span(request: Request<axum::body::Body>, next: Next) -> Response {
  let request_id = Uuid::new_v4().to_string();
  let method = request.method().to_string();
  let path = request.uri().path().to_string();
  let started_at = Instant::now();
  let mut request = request;
  request.extensions_mut().insert(RequestContext {
    request_id: request_id.clone(),
    started_at,
  });

  let span = tracing::info_span!(
    "http.request",
    request_id = %request_id,
    method = %method,
    path = %path
  );

  async move {
    tracing::info!(target: "router", "request started");
    let response = next.run(request).await;
    tracing::info!(target: "router", status = response.status().as_u16(), "request completed");
    response
  }
  .instrument(span)
  .await
}

fn json_error(status: StatusCode, message: &str) -> Response {
  (status, Json(json!({ "error": message }))).into_response()
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::collections::HashMap;
  use std::path::Path;
  use std::sync::Arc;

  use async_trait::async_trait;
  use futures::stream;
  use http_body_util::BodyExt;
  use tower::ServiceExt;

  use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};
  use crate::providers::{ProviderAdapter, ProviderCapabilities, ProviderRegistry, ProviderStream};

  struct MockAdapter;
  struct ChatOnlyAdapter;

  #[async_trait]
  impl ProviderAdapter for MockAdapter {
    fn name(&self) -> &'static str {
      "mock-openai"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities::all()
    }

    async fn chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Ok(json!({"id":"chat_123","object":"chat.completion"}))
    }

    async fn responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Ok(json!({"id":"resp_123","object":"response"}))
    }

    async fn stream_chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStream, crate::providers::ProviderError> {
      let s = stream::iter(vec![Ok("data: {\"id\":\"chunk-1\"}\n\n".to_string())]);
      Ok(Box::pin(s))
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStream, crate::providers::ProviderError> {
      let s = stream::iter(vec![
        Ok("{\"id\":\"resp-chunk-1\"}".to_string()),
        Ok("[DONE]".to_string()),
      ]);
      Ok(Box::pin(s))
    }
  }

  #[async_trait]
  impl ProviderAdapter for ChatOnlyAdapter {
    fn name(&self) -> &'static str {
      "chat-only"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities {
        chat_completion: true,
        responses: false,
        stream_chat_completion: true,
        stream_responses: false,
      }
    }

    async fn chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Ok(json!({
        "id":"chat_123",
        "object":"chat.completion",
        "choices":[{"index":0,"message":{"role":"assistant","content":"hello from chat-only"}}]
      }))
    }

    async fn responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "responses unsupported".to_string(),
      ))
    }

    async fn stream_chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStream, crate::providers::ProviderError> {
      let s = stream::iter(vec![
        Ok(
          json!({
            "id":"chatcmpl_1",
            "object":"chat.completion.chunk",
            "choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":Value::Null}]
          })
          .to_string(),
        ),
        Ok("[DONE]".to_string()),
      ]);
      Ok(Box::pin(s))
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStream, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming responses unsupported".to_string(),
      ))
    }
  }

  fn write_config(dir: &Path, base_url: &str) {
    std::fs::write(
      dir.join("providers.yaml"),
      format!("providers:\n  openai:\n    provider_type: openai\n    base_url: {base_url}\n    enabled: true\n"),
    )
    .unwrap();

    std::fs::write(
            dir.join("models.yaml"),
            "models:\n  - openai_name: gpt-test\n    provider: openai\n    provider_model: gpt-upstream\n    is_default: true\n",
        )
        .unwrap();

    std::fs::write(
      dir.join("credentials.yaml"),
      "providers:\n  openai:\n    api_key: test-key\n",
    )
    .unwrap();
  }

  async fn test_app() -> Router {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    write_config(dir.path(), "http://unused.local");
    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    adapters.insert("openai".to_string(), Arc::new(MockAdapter));
    let registry = ProviderRegistry::from_adapters(adapters);
    let state = Arc::new(
      crate::app_state::AppState::new_for_tests(dir.path().to_path_buf(), registry)
        .await
        .unwrap(),
    );
    build_router(state)
  }

  async fn test_app_with_adapter(adapter: Arc<dyn ProviderAdapter>) -> Router {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    write_config(dir.path(), "http://unused.local");
    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    adapters.insert("openai".to_string(), adapter);
    let registry = ProviderRegistry::from_adapters(adapters);
    let state = Arc::new(
      crate::app_state::AppState::new_for_tests(dir.path().to_path_buf(), registry)
        .await
        .unwrap(),
    );
    build_router(state)
  }

  #[tokio::test]
  async fn routes_chat_endpoint() {
    let app = test_app().await;
    let req = axum::http::Request::builder()
      .method("POST")
      .uri("/v1/chat/completions")
      .header("content-type", "application/json")
      .body(axum::body::Body::from(
        json!({
            "model": "gpt-test",
            "messages": [{"role":"user","content":"hi"}]
        })
        .to_string(),
      ))
      .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("chat_123"));
  }

  #[tokio::test]
  async fn routes_response_endpoint() {
    let app = test_app().await;
    let req = axum::http::Request::builder()
      .method("POST")
      .uri("/v1/responses")
      .header("content-type", "application/json")
      .body(axum::body::Body::from(
        json!({
            "model": "gpt-test",
            "input": "hello"
        })
        .to_string(),
      ))
      .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("resp_123"));
  }

  #[tokio::test]
  async fn supports_sse_streaming_chat() {
    let app = test_app().await;
    let req = axum::http::Request::builder()
      .method("POST")
      .uri("/v1/chat/completions")
      .header("content-type", "application/json")
      .body(axum::body::Body::from(
        json!({
            "model": "gpt-test",
            "stream": true,
            "messages": [{"role":"user","content":"hi"}]
        })
        .to_string(),
      ))
      .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
      .headers()
      .get("content-type")
      .and_then(|v| v.to_str().ok())
      .unwrap_or_default();
    assert!(content_type.contains("text/event-stream"));
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("chunk-1"));
  }

  #[tokio::test]
  async fn supports_sse_streaming_responses() {
    let app = test_app().await;
    let req = axum::http::Request::builder()
      .method("POST")
      .uri("/v1/responses")
      .header("content-type", "application/json")
      .body(axum::body::Body::from(
        json!({
            "model": "gpt-test",
            "stream": true,
            "input": "hi"
        })
        .to_string(),
      ))
      .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
      .headers()
      .get("content-type")
      .and_then(|v| v.to_str().ok())
      .unwrap_or_default();
    assert!(content_type.contains("text/event-stream"));
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body_text = String::from_utf8_lossy(&body);
    assert!(body_text.contains("resp-chunk-1"));
    assert!(body_text.contains("[DONE]"));
  }

  #[tokio::test]
  async fn converts_chat_to_responses_for_chat_only_adapter() {
    let app = test_app_with_adapter(Arc::new(ChatOnlyAdapter)).await;
    let req = axum::http::Request::builder()
      .method("POST")
      .uri("/v1/responses")
      .header("content-type", "application/json")
      .body(axum::body::Body::from(
        json!({
          "model": "gpt-test",
          "input": "hi"
        })
        .to_string(),
      ))
      .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body_text = String::from_utf8_lossy(&body);
    assert!(body_text.contains("\"object\":\"response\""));
    assert!(body_text.contains("hello from chat-only"));
  }

  #[tokio::test]
  async fn converts_chat_stream_to_responses_stream_for_chat_only_adapter() {
    let app = test_app_with_adapter(Arc::new(ChatOnlyAdapter)).await;
    let req = axum::http::Request::builder()
      .method("POST")
      .uri("/v1/responses")
      .header("content-type", "application/json")
      .body(axum::body::Body::from(
        json!({
          "model": "gpt-test",
          "stream": true,
          "input": "hi"
        })
        .to_string(),
      ))
      .unwrap();

    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let content_type = res
      .headers()
      .get("content-type")
      .and_then(|v| v.to_str().ok())
      .unwrap_or_default();
    assert!(content_type.contains("text/event-stream"));
    let body = res.into_body().collect().await.unwrap().to_bytes();
    let body_text = String::from_utf8_lossy(&body);
    assert!(body_text.contains("response.output_text.delta"));
    assert!(body_text.contains("[DONE]"));
  }
}
