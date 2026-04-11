use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::app_state::AppState;
use crate::providers::{ProviderError, ProviderOperation, ProviderStream};

use super::helpers::{
  account_override_from_headers, apply_usage, extract_usage, join_url, json_error,
  persist_assistant_message_from_chat_completion, persist_chat_history, persist_provider_error,
  persist_request_completed, persist_request_failed, persist_request_started, provider_error_response, sse_response,
};
use super::{RequestContext, StreamPersistence, StreamResponseKind};

pub(super) async fn handle(
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
  if route.provider == "codex" {
    if let Err(err) = state.codex_auth().ensure_fresh_api_key(account_override.clone()).await {
      return json_error(StatusCode::UNAUTHORIZED, &format!("codex oauth refresh failed: {err}"));
    }
  }
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
  let prefer_responses = provider_cfg
    .metadata
    .get("prefer_responses")
    .map(|v| v.eq_ignore_ascii_case("true"))
    .unwrap_or(false);
  let upstream_operation = if stream_requested {
    if prefer_responses && caps.stream_responses {
      ProviderOperation::Responses
    } else if caps.stream_chat_completion {
      ProviderOperation::ChatCompletions
    } else if caps.stream_responses {
      ProviderOperation::Responses
    } else {
      ProviderOperation::ChatCompletions
    }
  } else if prefer_responses && caps.responses {
    ProviderOperation::Responses
  } else if caps.chat_completion {
    ProviderOperation::ChatCompletions
  } else if caps.responses {
    ProviderOperation::Responses
  } else {
    ProviderOperation::ChatCompletions
  };
  let upstream_path = adapter.upstream_path(upstream_operation, stream_requested, route, &provider_cfg);
  let upstream_endpoint = join_url(&provider_cfg.base_url, &upstream_path);
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
    if prefer_responses && caps.stream_responses {
      let converted = chat_request_to_response_request(body);
      return match adapter
        .stream_responses(&provider_cfg, creds.as_ref(), route, converted)
        .await
      {
        Ok(provider_stream) => sse_response(
          convert_response_stream_to_chat_stream(provider_stream.stream, route),
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            upstream_status: Some(provider_stream.upstream_status),
            response_kind: StreamResponseKind::ChatCompletions,
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
      return match adapter
        .stream_chat_completion(&provider_cfg, creds.as_ref(), route, body)
        .await
      {
        Ok(provider_stream) => sse_response(
          provider_stream.stream,
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            upstream_status: Some(provider_stream.upstream_status),
            response_kind: StreamResponseKind::ChatCompletions,
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
          convert_response_stream_to_chat_stream(provider_stream.stream, route),
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            upstream_status: Some(provider_stream.upstream_status),
            response_kind: StreamResponseKind::ChatCompletions,
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

  if prefer_responses && caps.responses {
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
