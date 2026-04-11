use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use super::openai_compatible::{self, HttpErrorFormat};
use super::{
  join_upstream_url, ProviderAdapter, ProviderCapabilities, ProviderError, ProviderOperation, ProviderStreamResponse,
  UpstreamLogContext,
};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

pub trait CopilotRequestDecorator: Send + Sync {
  fn decorate_headers(&self, headers: &mut HeaderMap, creds: Option<&ProviderCredential>);
}

pub struct DefaultCopilotRequestDecorator;

impl CopilotRequestDecorator for DefaultCopilotRequestDecorator {
  fn decorate_headers(&self, headers: &mut HeaderMap, creds: Option<&ProviderCredential>) {
    if let Some(token) = creds.and_then(|c| c.api_key.clone()) {
      let value = format!("Bearer {token}");
      if let Ok(header_val) = HeaderValue::from_str(&value) {
        headers.insert(AUTHORIZATION, header_val);
      }
    }
    headers.insert(
      HeaderName::from_static("x-copilot-client"),
      HeaderValue::from_static("llm-router"),
    );
    headers.insert(
      HeaderName::from_static("editor-version"),
      HeaderValue::from_static("vscode/1.85.1"),
    );
    headers.insert(
      HeaderName::from_static("editor-plugin-version"),
      HeaderValue::from_static("copilot/1.155.0"),
    );
    headers.insert(
      HeaderName::from_static("user-agent"),
      HeaderValue::from_static("GithubCopilot/1.155.0"),
    );
  }
}

pub struct GitHubCopilotAdapter {
  client: reqwest::Client,
  decorator: Box<dyn CopilotRequestDecorator>,
}

impl GitHubCopilotAdapter {
  pub fn new() -> Self {
    Self {
      client: reqwest::Client::new(),
      decorator: Box::new(DefaultCopilotRequestDecorator),
    }
  }

  fn headers(&self, config: &ProviderDefinition, creds: Option<&ProviderCredential>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    self.decorator.decorate_headers(&mut headers, creds);
    openai_compatible::apply_config_headers(&mut headers, &config.headers);
    headers
  }
}

#[async_trait]
impl ProviderAdapter for GitHubCopilotAdapter {
  fn name(&self) -> &'static str {
    "github-copilot"
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
    let mut body = openai_compatible::with_model(route, request_body.clone());
    if stream {
      body = openai_compatible::with_stream(body);
    }
    body
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
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let body = openai_compatible::with_model(route, request_body);
    let upstream_path = self.upstream_path(ProviderOperation::ChatCompletions, false, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    openai_compatible::post_json(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
      HttpErrorFormat::StatusAndBody,
    )
    .await
  }

  async fn responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let body = openai_compatible::with_model(route, request_body);
    let upstream_path = self.upstream_path(ProviderOperation::Responses, false, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    openai_compatible::post_json(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
      HttpErrorFormat::StatusAndBody,
    )
    .await
  }

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let body = openai_compatible::with_stream(openai_compatible::with_model(route, request_body));
    let upstream_path = self.upstream_path(ProviderOperation::ChatCompletions, true, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    openai_compatible::post_stream(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
    )
    .await
  }

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let body = openai_compatible::with_stream(openai_compatible::with_model(route, request_body));
    let upstream_path = self.upstream_path(ProviderOperation::Responses, true, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    openai_compatible::post_stream(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
    )
    .await
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
  use serde_json::json;
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
    let app = Router::new()
      .route("/v1/chat/completions", post(stub_handler))
      .route("/v1/responses", post(stub_handler))
      .with_state(stub);
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
      openai_name: "gpt-5-mini".to_string(),
      provider: "github_copilot".to_string(),
      provider_model: "gpt-5-mini".to_string(),
      is_default: true,
    }
  }

  fn provider_def(base_url: &str) -> ProviderDefinition {
    ProviderDefinition {
      provider_type: "github-copilot".to_string(),
      base_url: base_url.to_string(),
      enabled: true,
      headers: HashMap::new(),
      metadata: HashMap::new(),
    }
  }

  #[test]
  fn adds_default_copilot_headers() {
    let adapter = GitHubCopilotAdapter::new();
    let config = provider_def("http://unused");
    let headers = adapter.headers(&config, None);
    assert_eq!(
      headers
        .get(HeaderName::from_static("editor-version"))
        .and_then(|v| v.to_str().ok()),
      Some("vscode/1.85.1")
    );
    assert_eq!(
      headers
        .get(HeaderName::from_static("editor-plugin-version"))
        .and_then(|v| v.to_str().ok()),
      Some("copilot/1.155.0")
    );
    assert_eq!(
      headers
        .get(HeaderName::from_static("user-agent"))
        .and_then(|v| v.to_str().ok()),
      Some("GithubCopilot/1.155.0")
    );
  }

  #[test]
  fn config_headers_override_defaults() {
    let adapter = GitHubCopilotAdapter::new();
    let mut config = provider_def("http://unused");
    config
      .headers
      .insert("Editor-Version".to_string(), Some("vscode/9.9.9".to_string()));
    config
      .headers
      .insert("User-Agent".to_string(), Some("CustomCopilot/9.9.9".to_string()));

    let headers = adapter.headers(&config, None);
    assert_eq!(
      headers
        .get(HeaderName::from_static("editor-version"))
        .and_then(|v| v.to_str().ok()),
      Some("vscode/9.9.9")
    );
    assert_eq!(
      headers
        .get(HeaderName::from_static("user-agent"))
        .and_then(|v| v.to_str().ok()),
      Some("CustomCopilot/9.9.9")
    );
  }

  #[test]
  fn config_null_header_removes_default() {
    let adapter = GitHubCopilotAdapter::new();
    let mut config = provider_def("http://unused");
    config.headers.insert("Editor-Version".to_string(), None);

    let headers = adapter.headers(&config, None);
    assert!(headers.get(HeaderName::from_static("editor-version")).is_none());
  }

  #[tokio::test]
  async fn logs_upstream_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 1_000).expect("store");
    let subscriber = tracing_subscriber::registry().with(LogCaptureLayer::new(store.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);

    let adapter = GitHubCopilotAdapter::new();
    let (addr, shutdown) = start_stub_server(UpstreamStub {
      status: StatusCode::NOT_FOUND,
      body: r#"{"error":"copilot route missing"}"#.to_string(),
    })
    .await;
    let config = provider_def(&format!("http://{addr}"));
    let span = tracing::info_span!("http.request", request_id = "req-copilot-err");
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
        request_id: Some("req-copilot-err".to_string()),
      })
      .expect("query");
    let failed = logs
      .iter()
      .find(|l| l.message == "upstream request failed")
      .expect("failed log");
    assert_eq!(
      failed.metadata.get("provider").map(String::as_str),
      Some("github_copilot")
    );
    assert_eq!(failed.metadata.get("status").map(String::as_str), Some("404"));
  }
}
