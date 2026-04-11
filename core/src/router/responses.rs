use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use crate::app_state::AppState;
use crate::providers::{ProviderError, ProviderOperation, ProviderStream};

use super::helpers::{
  account_override_from_headers, apply_usage, extract_usage, join_url, json_error,
  persist_assistant_message_from_response_payload, persist_chat_history, persist_provider_error,
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
  let upstream_operation = if stream_requested {
    if caps.stream_responses {
      ProviderOperation::Responses
    } else if caps.stream_chat_completion {
      ProviderOperation::ChatCompletions
    } else {
      ProviderOperation::Responses
    }
  } else if caps.responses {
    ProviderOperation::Responses
  } else if caps.chat_completion {
    ProviderOperation::ChatCompletions
  } else {
    ProviderOperation::Responses
  };
  let upstream_path = adapter.upstream_path(upstream_operation, stream_requested);
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
          provider_stream.stream,
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            upstream_status: Some(provider_stream.upstream_status),
            response_kind: StreamResponseKind::Responses,
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
          convert_chat_stream_to_response_stream(provider_stream.stream),
          Some(StreamPersistence {
            state: Arc::clone(&state),
            request_id: ctx.request_id.clone(),
            provider: route.provider.clone(),
            account_id: effective_account_id.clone(),
            model: route.openai_name.clone(),
            upstream_status: Some(provider_stream.upstream_status),
            response_kind: StreamResponseKind::Responses,
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
