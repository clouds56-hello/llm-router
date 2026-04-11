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

#[derive(Clone, Copy)]
pub(super) enum StreamResponseKind {
  ChatCompletions,
  Responses,
}

#[derive(Clone)]
pub(super) struct StreamPersistence {
  state: Arc<AppState>,
  request_id: String,
  provider: String,
  account_id: Option<String>,
  model: String,
  upstream_status: Option<u16>,
  response_kind: StreamResponseKind,
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
  use std::path::PathBuf;
  use std::sync::Arc;

  use async_trait::async_trait;
  use axum::http::StatusCode;
  use futures::stream;
  use http_body_util::BodyExt;
  use rusqlite::Connection;
  use serde_json::{json, Value};
  use tower::ServiceExt;

  use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};
  use crate::providers::{
    ProviderAdapter, ProviderCapabilities, ProviderOperation, ProviderRegistry, ProviderStreamResponse,
  };

  struct MockAdapter;
  struct ChatOnlyAdapter;
  struct CopilotPathAdapter;
  struct StreamSuccessAdapter;
  struct StreamFailStatusAdapter;
  struct StreamUnauthorizedAdapter;

  struct TestHarness {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    app: Router,
  }

  #[async_trait]
  impl ProviderAdapter for MockAdapter {
    fn name(&self) -> &'static str {
      "mock-openai"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities::all()
    }

    fn upstream_path(
      &self,
      operation: ProviderOperation,
      _stream: bool,
      _route: &ModelRoute,
      _provider: &ProviderDefinition,
    ) -> String {
      match operation {
        ProviderOperation::ChatCompletions => "/v1/chat/completions".to_string(),
        ProviderOperation::Responses => "/v1/responses".to_string(),
      }
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
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      let s = stream::iter(vec![Ok("data: {\"id\":\"chunk-1\"}\n\n".to_string())]);
      Ok(ProviderStreamResponse {
        stream: Box::pin(s),
        upstream_status: 200,
      })
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      let s = stream::iter(vec![
        Ok("{\"id\":\"resp-chunk-1\"}".to_string()),
        Ok("[DONE]".to_string()),
      ]);
      Ok(ProviderStreamResponse {
        stream: Box::pin(s),
        upstream_status: 200,
      })
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

    fn upstream_path(
      &self,
      operation: ProviderOperation,
      _stream: bool,
      _route: &ModelRoute,
      _provider: &ProviderDefinition,
    ) -> String {
      match operation {
        ProviderOperation::ChatCompletions => "/v1/chat/completions".to_string(),
        ProviderOperation::Responses => "/v1/responses".to_string(),
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
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
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
      Ok(ProviderStreamResponse {
        stream: Box::pin(s),
        upstream_status: 200,
      })
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming responses unsupported".to_string(),
      ))
    }
  }

  #[async_trait]
  impl ProviderAdapter for CopilotPathAdapter {
    fn name(&self) -> &'static str {
      "copilot-path"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities {
        chat_completion: true,
        responses: false,
        stream_chat_completion: false,
        stream_responses: false,
      }
    }

    fn upstream_path(
      &self,
      operation: ProviderOperation,
      _stream: bool,
      _route: &ModelRoute,
      _provider: &ProviderDefinition,
    ) -> String {
      match operation {
        ProviderOperation::ChatCompletions => "/chat/completions".to_string(),
        ProviderOperation::Responses => "/v1/responses".to_string(),
      }
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
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming unsupported".to_string(),
      ))
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming unsupported".to_string(),
      ))
    }
  }

  #[async_trait]
  impl ProviderAdapter for StreamSuccessAdapter {
    fn name(&self) -> &'static str {
      "stream-success"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities {
        chat_completion: false,
        responses: false,
        stream_chat_completion: true,
        stream_responses: false,
      }
    }

    fn upstream_path(
      &self,
      operation: ProviderOperation,
      _stream: bool,
      _route: &ModelRoute,
      _provider: &ProviderDefinition,
    ) -> String {
      match operation {
        ProviderOperation::ChatCompletions => "/v1/chat/completions".to_string(),
        ProviderOperation::Responses => "/v1/responses".to_string(),
      }
    }

    async fn chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "chat unsupported".to_string(),
      ))
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
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      let s = stream::iter(vec![
        Ok(
          json!({
            "id":"chatcmpl_stream",
            "object":"chat.completion.chunk",
            "choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":Value::Null}]
          })
          .to_string(),
        ),
        Ok(json!({"usage":{"prompt_tokens":3,"completion_tokens":2,"total_tokens":5}}).to_string()),
        Ok("[DONE]".to_string()),
      ]);
      Ok(ProviderStreamResponse {
        stream: Box::pin(s),
        upstream_status: 207,
      })
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming responses unsupported".to_string(),
      ))
    }
  }

  #[async_trait]
  impl ProviderAdapter for StreamFailStatusAdapter {
    fn name(&self) -> &'static str {
      "stream-fail-status"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities {
        chat_completion: false,
        responses: false,
        stream_chat_completion: true,
        stream_responses: false,
      }
    }

    fn upstream_path(
      &self,
      operation: ProviderOperation,
      _stream: bool,
      _route: &ModelRoute,
      _provider: &ProviderDefinition,
    ) -> String {
      match operation {
        ProviderOperation::ChatCompletions => "/v1/chat/completions".to_string(),
        ProviderOperation::Responses => "/v1/responses".to_string(),
      }
    }

    async fn chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "chat unsupported".to_string(),
      ))
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
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::http_with_status(
        "upstream returned status 429",
        429,
      ))
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming responses unsupported".to_string(),
      ))
    }
  }

  #[async_trait]
  impl ProviderAdapter for StreamUnauthorizedAdapter {
    fn name(&self) -> &'static str {
      "stream-unauthorized"
    }

    fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
      ProviderCapabilities {
        chat_completion: false,
        responses: false,
        stream_chat_completion: true,
        stream_responses: false,
      }
    }

    fn upstream_path(
      &self,
      operation: ProviderOperation,
      _stream: bool,
      _route: &ModelRoute,
      _provider: &ProviderDefinition,
    ) -> String {
      match operation {
        ProviderOperation::ChatCompletions => "/v1/chat/completions".to_string(),
        ProviderOperation::Responses => "/v1/responses".to_string(),
      }
    }

    async fn chat_completion(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<Value, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "chat unsupported".to_string(),
      ))
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
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unauthorized { status_code: 401 })
    }

    async fn stream_responses(
      &self,
      _config: &ProviderDefinition,
      _creds: Option<&ProviderCredential>,
      _route: &ModelRoute,
      _request_body: Value,
    ) -> Result<ProviderStreamResponse, crate::providers::ProviderError> {
      Err(crate::providers::ProviderError::Unsupported(
        "streaming responses unsupported".to_string(),
      ))
    }
  }

  fn write_config(dir: &Path, base_url: &str) {
    write_config_with_provider_type(dir, base_url, "openai");
  }

  fn write_config_with_provider_type(dir: &Path, base_url: &str, provider_type: &str) {
    std::fs::write(
      dir.join("providers.yaml"),
      format!(
        "providers:\n  openai:\n    provider_type: {provider_type}\n    base_url: {base_url}\n    enabled: true\n"
      ),
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

  fn write_config_with_provider_type_and_metadata(
    dir: &Path,
    base_url: &str,
    provider_type: &str,
    metadata_yaml: &str,
  ) {
    std::fs::write(
      dir.join("providers.yaml"),
      format!(
        "providers:\n  openai:\n    provider_type: {provider_type}\n    base_url: {base_url}\n    enabled: true\n    metadata:\n{metadata_yaml}"
      ),
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
    test_harness_with_adapter(adapter).await.app
  }

  async fn test_harness_with_adapter(adapter: Arc<dyn ProviderAdapter>) -> TestHarness {
    test_harness_with_adapter_and_provider_type(adapter, "openai").await
  }

  async fn test_harness_with_adapter_and_provider_type(
    adapter: Arc<dyn ProviderAdapter>,
    provider_type: &str,
  ) -> TestHarness {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    write_config_with_provider_type(dir.path(), "http://unused.local", provider_type);
    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    adapters.insert(provider_type.to_string(), adapter);
    let registry = ProviderRegistry::from_adapters(adapters);
    let state = Arc::new(
      crate::app_state::AppState::new_for_tests(dir.path().to_path_buf(), registry)
        .await
        .unwrap(),
    );
    TestHarness {
      db_path: dir.path().join("state.db"),
      app: build_router(state),
      _dir: dir,
    }
  }

  async fn test_harness_with_adapter_provider_and_metadata(
    adapter: Arc<dyn ProviderAdapter>,
    provider_type: &str,
    metadata_yaml: &str,
  ) -> TestHarness {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    write_config_with_provider_type_and_metadata(dir.path(), "http://unused.local", provider_type, metadata_yaml);
    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    adapters.insert(provider_type.to_string(), adapter);
    let registry = ProviderRegistry::from_adapters(adapters);
    let state = Arc::new(
      crate::app_state::AppState::new_for_tests(dir.path().to_path_buf(), registry)
        .await
        .unwrap(),
    );
    TestHarness {
      db_path: dir.path().join("state.db"),
      app: build_router(state),
      _dir: dir,
    }
  }

  fn latest_request_row(
    db_path: &Path,
  ) -> Option<(String, Option<i64>, Option<String>, Option<String>, Option<String>)> {
    let conn = Connection::open(db_path).ok()?;
    conn
      .query_row(
        "SELECT endpoint, http_status, response_sse_text, response_body_json, error_text
         FROM llm_requests
         ORDER BY created_at DESC
         LIMIT 1",
        [],
        |row| {
          Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<String>>(4)?,
          ))
        },
      )
      .ok()
  }

  fn latest_upstream_request_payload(db_path: &Path) -> Option<String> {
    let conn = Connection::open(db_path).ok()?;
    conn
      .query_row(
        "SELECT upstream_request_body_json
         FROM llm_requests
         ORDER BY created_at DESC
         LIMIT 1",
        [],
        |row| row.get::<_, Option<String>>(0),
      )
      .ok()
      .flatten()
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
  async fn routes_chat_endpoint_with_openai_compatible_provider_type() {
    let app = test_harness_with_adapter_and_provider_type(Arc::new(MockAdapter), "openai-compatible")
      .await
      .app;
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

  #[tokio::test]
  async fn persists_endpoint_using_adapter_upstream_path() {
    let harness = test_harness_with_adapter_and_provider_type(Arc::new(CopilotPathAdapter), "github-copilot").await;
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
    let res = harness.app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let _ = res.into_body().collect().await.unwrap();

    let row = latest_request_row(&harness.db_path).expect("request row");
    assert_eq!(row.0, "http://unused.local/chat/completions");
    let upstream = latest_upstream_request_payload(&harness.db_path).expect("upstream payload");
    let upstream_json: Value = serde_json::from_str(&upstream).expect("json");
    assert_eq!(upstream_json.get("model").and_then(Value::as_str), Some("gpt-test"));
  }

  #[tokio::test]
  async fn codex_chat_endpoint_is_unsupported() {
    let dir = tempfile::tempdir_in("/tmp").unwrap();
    std::fs::write(
      dir.path().join("providers.yaml"),
      "providers:\n  codex:\n    provider_type: openai\n    base_url: http://unused.local\n    enabled: true\n    metadata:\n      codex_api_mode: responses\n",
    )
    .unwrap();
    std::fs::write(
      dir.path().join("models.yaml"),
      "models:\n  - openai_name: gpt-test\n    provider: codex\n    provider_model: gpt-upstream\n    is_default: true\n",
    )
    .unwrap();
    std::fs::write(
      dir.path().join("credentials.yaml"),
      "providers:\n  codex:\n    api_key: test-key\n",
    )
    .unwrap();

    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    adapters.insert("openai".to_string(), Arc::new(MockAdapter));
    let registry = ProviderRegistry::from_adapters(adapters);
    let state = Arc::new(
      crate::app_state::AppState::new_for_tests(dir.path().to_path_buf(), registry)
        .await
        .unwrap(),
    );
    let app = build_router(state);

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
    assert_eq!(res.status(), StatusCode::NOT_IMPLEMENTED);
  }

  #[tokio::test]
  async fn chat_route_prefers_responses_when_provider_requests_it() {
    let harness = test_harness_with_adapter_provider_and_metadata(
      Arc::new(MockAdapter),
      "openai",
      "      prefer_responses: \"true\"\n",
    )
    .await;
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
    let res = harness.app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let _ = res.into_body().collect().await.unwrap();

    let row = latest_request_row(&harness.db_path).expect("request row");
    assert_eq!(row.0, "http://unused.local/v1/responses");
  }

  #[tokio::test]
  async fn persists_stream_upstream_status_and_final_response_body() {
    let harness = test_harness_with_adapter(Arc::new(StreamSuccessAdapter)).await;
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
    let res = harness.app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let _ = res.into_body().collect().await.unwrap();

    let row = latest_request_row(&harness.db_path).expect("request row");
    assert_eq!(row.1, Some(207));
    assert!(row.2.as_deref().unwrap_or_default().contains("[DONE]"));
    let response_body = row.3.expect("response body");
    assert!(response_body.contains("\"object\":\"chat.completion\""));
    assert!(response_body.contains("\"content\":\"hello\""));
    assert!(response_body.contains("\"total_tokens\":5"));
  }

  #[tokio::test]
  async fn persists_stream_failure_status_from_provider_error() {
    let harness = test_harness_with_adapter(Arc::new(StreamFailStatusAdapter)).await;
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
    let res = harness.app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    let _ = res.into_body().collect().await.unwrap();

    let row = latest_request_row(&harness.db_path).expect("request row");
    assert_eq!(row.1, Some(429));
    assert!(row
      .4
      .as_deref()
      .unwrap_or_default()
      .contains("upstream returned status 429"));
  }

  #[tokio::test]
  async fn persists_stream_unauthorized_status_from_provider_error() {
    let harness = test_harness_with_adapter(Arc::new(StreamUnauthorizedAdapter)).await;
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
    let res = harness.app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    let _ = res.into_body().collect().await.unwrap();

    let row = latest_request_row(&harness.db_path).expect("request row");
    assert_eq!(row.1, Some(401));
  }
}
