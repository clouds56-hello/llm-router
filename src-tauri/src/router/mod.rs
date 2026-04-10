use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;

use crate::app_state::AppState;
use crate::providers::ProviderError;

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

async fn request_logs(State(state): State<Arc<AppState>>) -> Json<Value> {
  Json(json!({ "logs": state.logs().list(200) }))
}

async fn chat_completions(State(state): State<Arc<AppState>>, Json(mut body): Json<Value>) -> Response {
  let loaded = state.config().get();
  let model_name = body.get("model").and_then(|v| v.as_str()).unwrap_or_default();

  let Some(route) = loaded.resolve_model(model_name) else {
    return json_error(StatusCode::BAD_REQUEST, "model routing config is empty");
  };

  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".to_string(), Value::String(route.openai_name.clone()));
  }

  let stream_requested = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);

  let (adapter, provider_cfg, creds) = match state.providers().adapter_for_provider(&loaded, route) {
    Ok(v) => v,
    Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
  };

  state.log_sink().info(
    "router",
    format!(
      "chat request model={} provider={} adapter={}",
      route.openai_name,
      route.provider,
      adapter.name(),
    ),
  );

  if stream_requested {
    match adapter
      .stream_chat_completion(&provider_cfg, creds.as_ref(), route, body)
      .await
    {
      Ok(provider_stream) => {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(32);
        tokio::spawn(async move {
          futures::pin_mut!(provider_stream);
          while let Some(item) = provider_stream.next().await {
            let event = match item {
              Ok(chunk) => Event::default().data(chunk),
              Err(err) => Event::default()
                .event("error")
                .data(json!({"error": err.to_string()}).to_string()),
            };
            if tx.send(Ok(event)).await.is_err() {
              break;
            }
          }
        });

        return Sse::new(ReceiverStream::new(rx))
          .keep_alive(KeepAlive::default())
          .into_response();
      }
      Err(err) => {
        return provider_error_response(err);
      }
    }
  }

  match adapter
    .chat_completion(&provider_cfg, creds.as_ref(), route, body)
    .await
  {
    Ok(data) => Json(data).into_response(),
    Err(err) => provider_error_response(err),
  }
}

async fn responses(State(state): State<Arc<AppState>>, Json(mut body): Json<Value>) -> Response {
  let loaded = state.config().get();
  let model_name = body.get("model").and_then(|v| v.as_str()).unwrap_or_default();

  let Some(route) = loaded.resolve_model(model_name) else {
    return json_error(StatusCode::BAD_REQUEST, "model routing config is empty");
  };

  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".to_string(), Value::String(route.openai_name.clone()));
  }

  let (adapter, provider_cfg, creds) = match state.providers().adapter_for_provider(&loaded, route) {
    Ok(v) => v,
    Err(err) => return json_error(StatusCode::BAD_REQUEST, &err.to_string()),
  };

  state.log_sink().info(
    "router",
    format!(
      "responses request model={} provider={} adapter={}",
      route.openai_name,
      route.provider,
      adapter.name(),
    ),
  );

  match adapter.responses(&provider_cfg, creds.as_ref(), route, body).await {
    Ok(data) => Json(data).into_response(),
    Err(err) => provider_error_response(err),
  }
}

fn provider_error_response(err: ProviderError) -> Response {
  match err {
    ProviderError::Unauthorized => json_error(StatusCode::UNAUTHORIZED, "unauthorized"),
    ProviderError::Unsupported(msg) => json_error(StatusCode::NOT_IMPLEMENTED, &msg),
    ProviderError::Http(msg) | ProviderError::Internal(msg) => json_error(StatusCode::BAD_GATEWAY, &msg),
  }
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
  use crate::providers::{ProviderAdapter, ProviderRegistry, ProviderStream};

  struct MockAdapter;

  #[async_trait]
  impl ProviderAdapter for MockAdapter {
    fn name(&self) -> &'static str {
      "mock-openai"
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
}
