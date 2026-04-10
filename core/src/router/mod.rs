use std::sync::Arc;
use std::time::Instant;

use axum::http::Request;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use axum::{middleware, middleware::Next};
use tower_http::cors::CorsLayer;
use tower_http::trace::TraceLayer;
use tracing::Instrument;
use uuid::Uuid;

use crate::app_state::AppState;

mod api;
mod chat_completions;
mod helpers;
mod responses;

#[derive(Clone)]
pub(super) struct RequestContext {
  request_id: String,
  started_at: Instant,
}

#[derive(Clone)]
pub(super) struct StreamPersistence {
  state: Arc<AppState>,
  request_id: String,
  provider: String,
  account_id: Option<String>,
  model: String,
  started_at: Instant,
}

pub fn build_router(state: Arc<AppState>) -> Router {
  Router::new()
    .route("/health", get(api::health))
    .route("/v1/chat/completions", post(chat_completions::handle))
    .route("/v1/responses", post(responses::handle))
    .route("/api/providers/status", get(api::provider_status))
    .route("/api/models", get(api::model_list))
    .route("/api/config", get(api::active_config))
    .route("/api/logs", get(api::request_logs))
    .with_state(state)
    .layer(middleware::from_fn(with_request_span))
    .layer(CorsLayer::permissive())
    .layer(TraceLayer::new_for_http())
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

#[cfg(test)]
mod tests {
  use super::*;

  use std::collections::HashMap;
  use std::path::Path;
  use std::sync::Arc;

  use async_trait::async_trait;
  use axum::http::StatusCode;
  use futures::stream;
  use http_body_util::BodyExt;
  use serde_json::{json, Value};
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
