use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

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

  fn build_auth(&self, req: reqwest::RequestBuilder, creds: Option<&ProviderCredential>) -> reqwest::RequestBuilder {
    if let Some(token) = creds.and_then(|c| c.api_key.clone()) {
      req.header(AUTHORIZATION, format!("Bearer {token}"))
    } else {
      req
    }
  }

  fn with_model(route: &ModelRoute, mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("model".to_string(), Value::String(route.provider_model.clone()));
      return body;
    }

    json!({
        "model": route.provider_model,
        "input": body
    })
  }

  async fn post_json(
    &self,
    log_ctx: UpstreamLogContext,
    url: String,
    creds: Option<&ProviderCredential>,
    body: Value,
  ) -> Result<Value, ProviderError> {
    let started = log_ctx.started(&body);
    let req = self
      .client
      .post(url)
      .header(CONTENT_TYPE, "application/json")
      .json(&body);

    let res = self.build_auth(req, creds).send().await.map_err(|e| {
      log_ctx.failed(started, None, Some(&e.to_string()));
      ProviderError::Http(e.to_string())
    })?;

    let status = res.status();
    if status.as_u16() == 401 {
      log_ctx.failed(started, Some(401), Some("unauthorized"));
      return Err(ProviderError::Unauthorized);
    }

    if !status.is_success() {
      let details = res.text().await.unwrap_or_default();
      log_ctx.failed(started, Some(status.as_u16()), Some(&details));
      return Err(ProviderError::Http(format!("upstream returned status {status}")));
    }

    let parsed = res.json::<Value>().await.map_err(|e| {
      log_ctx.failed(started, Some(status.as_u16()), Some(&e.to_string()));
      ProviderError::Http(e.to_string())
    })?;
    log_ctx.completed(started, status.as_u16());
    Ok(parsed)
  }

  async fn post_stream(
    &self,
    log_ctx: UpstreamLogContext,
    url: String,
    creds: Option<&ProviderCredential>,
    body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let started = log_ctx.started(&body);
    let req = self
      .client
      .post(url)
      .header(CONTENT_TYPE, "application/json")
      .json(&body);
    let res = self.build_auth(req, creds).send().await.map_err(|e| {
      log_ctx.failed(started, None, Some(&e.to_string()));
      ProviderError::Http(e.to_string())
    })?;

    let status = res.status();
    if status.as_u16() == 401 {
      log_ctx.failed(started, Some(401), Some("unauthorized"));
      return Err(ProviderError::Unauthorized);
    }
    if !status.is_success() {
      let details = res.text().await.unwrap_or_default();
      log_ctx.failed(started, Some(status.as_u16()), Some(&details));
      return Err(ProviderError::Http(format!("upstream returned status {status}")));
    }

    log_ctx.completed(started, status.as_u16());
    Ok(normalize_sse_stream(res))
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
    let body = Self::with_model(route, request_body);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/chat/completions".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    self
      .post_json(ctx, format!("{}/v1/chat/completions", config.base_url), creds, body)
      .await
  }

  async fn responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let body = Self::with_model(route, request_body);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/responses".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    self
      .post_json(ctx, format!("{}/v1/responses", config.base_url), creds, body)
      .await
  }

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let mut body = Self::with_model(route, request_body);
    if let Some(obj) = body.as_object_mut() {
      obj.insert("stream".to_string(), Value::Bool(true));
    }
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/chat/completions".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };

    self
      .post_stream(ctx, format!("{}/v1/chat/completions", config.base_url), creds, body)
      .await
  }

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let mut body = Self::with_model(route, request_body);
    if let Some(obj) = body.as_object_mut() {
      obj.insert("stream".to_string(), Value::Bool(true));
    }
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/v1/responses".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    self
      .post_stream(ctx, format!("{}/v1/responses", config.base_url), creds, body)
      .await
  }
}

fn normalize_sse_stream(res: reqwest::Response) -> ProviderStream {
  let (tx, rx) = mpsc::channel::<Result<String, ProviderError>>(32);
  tokio::spawn(async move {
    let mut upstream = res.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = upstream.next().await {
      let bytes = match chunk {
        Ok(bytes) => bytes,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::Http(err.to_string()))).await;
          break;
        }
      };
      let chunk_str = match String::from_utf8(bytes.to_vec()) {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::Internal(err.to_string()))).await;
          break;
        }
      };
      buffer.push_str(&chunk_str);
      while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer = buffer[idx + 2..].to_string();
        for payload in parse_sse_frame_payloads(&frame) {
          if tx.send(Ok(payload)).await.is_err() {
            return;
          }
        }
      }
    }
    if !buffer.trim().is_empty() {
      for payload in parse_sse_frame_payloads(&buffer) {
        if tx.send(Ok(payload)).await.is_err() {
          return;
        }
      }
    }
  });
  Box::pin(ReceiverStream::new(rx))
}

fn parse_sse_frame_payloads(frame: &str) -> Vec<String> {
  let mut data_lines: Vec<String> = Vec::new();
  for raw in frame.lines() {
    let line = raw.trim_end_matches('\r');
    if let Some(data) = line.strip_prefix("data:") {
      data_lines.push(data.trim_start().to_string());
    }
  }
  if data_lines.is_empty() {
    return Vec::new();
  }
  let payload = data_lines.join("\n").trim().to_string();
  if payload.is_empty() {
    Vec::new()
  } else {
    vec![payload]
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
      metadata: HashMap::new(),
    }
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
