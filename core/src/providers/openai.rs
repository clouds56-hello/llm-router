use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use super::openai_compatible::{self, HttpErrorFormat};
use super::{ProviderAdapter, ProviderCapabilities, ProviderError, ProviderStream, UpstreamLogContext};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

#[derive(Default)]
pub struct OpenAiAdapter {
  client: reqwest::Client,
}

impl OpenAiAdapter {
  pub fn new() -> Self {
    Self {
      client: reqwest::Client::new(),
    }
  }

  fn headers(&self, config: &ProviderDefinition, creds: Option<&ProviderCredential>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(token) = creds.and_then(|c| c.api_key.clone()) {
      if let Ok(header_val) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, header_val);
      }
    }
    openai_compatible::apply_config_headers(&mut headers, &config.headers);
    headers
  }
}

#[async_trait]
impl ProviderAdapter for OpenAiAdapter {
  fn name(&self) -> &'static str {
    "openai"
  }

  fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
    ProviderCapabilities::all()
  }

  async fn chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let body = openai_compatible::with_model(route, request_body);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/chat/completions".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    openai_compatible::post_json(
      &self.client,
      ctx,
      format!("{}/v1/chat/completions", config.base_url),
      self.headers(config, creds),
      body,
      HttpErrorFormat::StatusOnly,
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
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/responses".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    openai_compatible::post_json(
      &self.client,
      ctx,
      format!("{}/v1/responses", config.base_url),
      self.headers(config, creds),
      body,
      HttpErrorFormat::StatusOnly,
    )
    .await
  }

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let body = openai_compatible::with_stream(openai_compatible::with_model(route, request_body));
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/chat/completions".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    openai_compatible::post_stream(
      &self.client,
      ctx,
      format!("{}/v1/chat/completions", config.base_url),
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
  ) -> Result<ProviderStream, ProviderError> {
    let body = openai_compatible::with_stream(openai_compatible::with_model(route, request_body));
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/responses".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    openai_compatible::post_stream(
      &self.client,
      ctx,
      format!("{}/v1/responses", config.base_url),
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
  use futures::StreamExt;
  use reqwest::header::HeaderName;
  use tokio::sync::oneshot;
  use tracing::Instrument;
  use tracing_subscriber::layer::SubscriberExt;

  use crate::db::logging::{LogCaptureLayer, LogQuery, LogStore};

  #[derive(Clone)]
  struct UpstreamStub {
    status: StatusCode,
    body: String,
    content_type: &'static str,
  }

  async fn stub_handler(State(stub): State<UpstreamStub>) -> (StatusCode, [(String, String); 1], String) {
    (
      stub.status,
      [("content-type".to_string(), stub.content_type.to_string())],
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

  fn route(provider: &str, provider_model: &str) -> ModelRoute {
    ModelRoute {
      openai_name: "gpt-test".to_string(),
      provider: provider.to_string(),
      provider_model: provider_model.to_string(),
      is_default: true,
    }
  }

  fn provider_def(base_url: &str) -> ProviderDefinition {
    ProviderDefinition {
      provider_type: "openai".to_string(),
      base_url: base_url.to_string(),
      enabled: true,
      headers: HashMap::new(),
      metadata: HashMap::new(),
    }
  }

  #[test]
  fn config_headers_can_override_and_remove() {
    let adapter = OpenAiAdapter::new();
    let mut config = provider_def("http://unused");
    config
      .headers
      .insert("Authorization".to_string(), Some("Bearer override".to_string()));
    config.headers.insert("Content-Type".to_string(), None);

    let headers = adapter.headers(&config, None);
    assert_eq!(
      headers
        .get(HeaderName::from_static("authorization"))
        .and_then(|v| v.to_str().ok()),
      Some("Bearer override")
    );
    assert!(headers.get(HeaderName::from_static("content-type")).is_none());
  }

  #[tokio::test]
  async fn logs_upstream_success_and_failure() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 1_000).expect("store");
    let subscriber = tracing_subscriber::registry().with(LogCaptureLayer::new(store.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);

    let adapter = OpenAiAdapter::new();

    let (ok_addr, ok_shutdown) = start_stub_server(UpstreamStub {
      status: StatusCode::OK,
      body: r#"{"id":"ok","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]}"#.to_string(),
      content_type: "application/json",
    })
    .await;
    let ok_config = provider_def(&format!("http://{ok_addr}"));
    let ok_route = route("openai", "gpt-5-mini");
    let span = tracing::info_span!("http.request", request_id = "req-openai-ok");
    async {
      let _ = adapter
        .chat_completion(
          &ok_config,
          None,
          &ok_route,
          serde_json::json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .expect("chat completion");
    }
    .instrument(span)
    .await;
    let _ = ok_shutdown.send(());

    let (err_addr, err_shutdown) = start_stub_server(UpstreamStub {
      status: StatusCode::NOT_FOUND,
      body: r#"{"error":"missing upstream path"}"#.to_string(),
      content_type: "application/json",
    })
    .await;
    let err_config = provider_def(&format!("http://{err_addr}"));
    let err_route = route("openai", "gpt-5-mini");
    let span = tracing::info_span!("http.request", request_id = "req-openai-err");
    async {
      let err = adapter
        .chat_completion(
          &err_config,
          None,
          &err_route,
          serde_json::json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .expect_err("expected failure");
      assert!(err.to_string().contains("upstream returned status 404"));
    }
    .instrument(span)
    .await;
    let _ = err_shutdown.send(());

    let ok_logs = store
      .query(LogQuery {
        limit: Some(200),
        level: None,
        request_id: Some("req-openai-ok".to_string()),
      })
      .expect("query");
    assert!(ok_logs.iter().any(|l| l.message == "upstream request started"));
    let completed = ok_logs
      .iter()
      .find(|l| l.message == "upstream request completed")
      .expect("completed log");
    assert_eq!(completed.metadata.get("provider").map(String::as_str), Some("openai"));
    assert_eq!(completed.metadata.get("adapter").map(String::as_str), Some("openai"));
    assert_eq!(
      completed.metadata.get("upstream_path").map(String::as_str),
      Some("/v1/chat/completions")
    );

    let err_logs = store
      .query(LogQuery {
        limit: Some(200),
        level: None,
        request_id: Some("req-openai-err".to_string()),
      })
      .expect("query");
    let failed = err_logs
      .iter()
      .find(|l| l.message == "upstream request failed")
      .expect("failed log");
    assert_eq!(failed.metadata.get("status").map(String::as_str), Some("404"));
    assert!(failed
      .metadata
      .get("upstream_error_snippet")
      .map(|s| s.contains("missing upstream path"))
      .unwrap_or(false));
  }

  #[tokio::test]
  async fn logs_upstream_stream_success() {
    let temp = tempfile::tempdir().expect("tempdir");
    let store = LogStore::new(&temp.path().join("state.db"), 1_000).expect("store");
    let subscriber = tracing_subscriber::registry().with(LogCaptureLayer::new(store.clone()));
    let _guard = tracing::subscriber::set_default(subscriber);

    let adapter = OpenAiAdapter::new();
    let (addr, shutdown) = start_stub_server(UpstreamStub {
      status: StatusCode::OK,
      body: "data: {\"id\":\"chunk\"}\n\ndata: [DONE]\n\n".to_string(),
      content_type: "text/event-stream",
    })
    .await;
    let config = provider_def(&format!("http://{addr}"));
    let route = route("openai", "gpt-5-mini");
    let span = tracing::info_span!("http.request", request_id = "req-openai-stream");
    async {
      let mut stream = adapter
        .stream_chat_completion(
          &config,
          None,
          &route,
          serde_json::json!({"messages":[{"role":"user","content":"hello"}]}),
        )
        .await
        .expect("stream ok");
      let _ = stream.next().await;
    }
    .instrument(span)
    .await;
    let _ = shutdown.send(());

    let logs = store
      .query(LogQuery {
        limit: Some(200),
        level: None,
        request_id: Some("req-openai-stream".to_string()),
      })
      .expect("query");
    let completed = logs
      .iter()
      .find(|l| l.message == "upstream request completed")
      .expect("completed");
    assert_eq!(completed.metadata.get("stream").map(String::as_str), Some("true"));
    assert_eq!(completed.metadata.get("status").map(String::as_str), Some("200"));
  }
}
