use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{ProviderAdapter, ProviderCapabilities, ProviderError, ProviderStream, UpstreamLogContext};
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

  fn with_model(route: &ModelRoute, mut body: Value) -> Value {
    if let Some(obj) = body.as_object_mut() {
      obj.insert("model".to_string(), Value::String(route.provider_model.clone()));
      body
    } else {
      json!({
        "model": route.provider_model,
        "input": body
      })
    }
  }

  fn headers(&self, creds: Option<&ProviderCredential>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    self.decorator.decorate_headers(&mut headers, creds);
    headers
  }

  async fn post_json(
    &self,
    log_ctx: UpstreamLogContext,
    url: String,
    headers: HeaderMap,
    body: Value,
  ) -> Result<Value, ProviderError> {
    let started = log_ctx.started(&body);
    let res = self
      .client
      .post(url)
      .headers(headers)
      .json(&body)
      .send()
      .await
      .map_err(|e| {
        log_ctx.failed(started, None, Some(&e.to_string()));
        ProviderError::Http(e.to_string())
      })?;
    let status = res.status();
    if status.as_u16() == 401 {
      log_ctx.failed(started, Some(401), Some("unauthorized"));
      return Err(ProviderError::Unauthorized);
    }
    if !status.is_success() {
      let text = res.text().await.unwrap_or_default();
      log_ctx.failed(started, Some(status.as_u16()), Some(&text));
      return Err(ProviderError::Http(format!(
        "upstream returned status {status}: {text}"
      )));
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
    headers: HeaderMap,
    body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let started = log_ctx.started(&body);
    let res = self
      .client
      .post(url)
      .headers(headers)
      .json(&body)
      .send()
      .await
      .map_err(|e| {
        log_ctx.failed(started, None, Some(&e.to_string()));
        ProviderError::Http(e.to_string())
      })?;
    let status = res.status();
    if status.as_u16() == 401 {
      log_ctx.failed(started, Some(401), Some("unauthorized"));
      return Err(ProviderError::Unauthorized);
    }
    if !status.is_success() {
      let text = res.text().await.unwrap_or_default();
      log_ctx.failed(started, Some(status.as_u16()), Some(&text));
      return Err(ProviderError::Http(format!("upstream returned status {status}")));
    }
    log_ctx.completed(started, status.as_u16());
    Ok(normalize_openai_sse(res))
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
      upstream_path: "/chat/completions".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    self
      .post_json(
        ctx,
        format!("{}/chat/completions", config.base_url),
        self.headers(creds),
        body,
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
      .post_json(
        ctx,
        format!("{}/v1/responses", config.base_url),
        self.headers(creds),
        body,
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
    let mut body = Self::with_model(route, request_body);
    if let Some(obj) = body.as_object_mut() {
      obj.insert("stream".to_string(), Value::Bool(true));
    }
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: "/chat/completions".to_string(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    self
      .post_stream(
        ctx,
        format!("{}/chat/completions", config.base_url),
        self.headers(creds),
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
      .post_stream(
        ctx,
        format!("{}/v1/responses", config.base_url),
        self.headers(creds),
        body,
      )
      .await
  }
}

fn normalize_openai_sse(res: reqwest::Response) -> ProviderStream {
  let (tx, rx) = mpsc::channel::<Result<String, ProviderError>>(32);
  tokio::spawn(async move {
    let mut upstream = res.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = upstream.next().await {
      let bytes = match chunk {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::Http(err.to_string()))).await;
          break;
        }
      };
      let part = match String::from_utf8(bytes.to_vec()) {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::Internal(err.to_string()))).await;
          break;
        }
      };
      buffer.push_str(&part);
      while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer = buffer[idx + 2..].to_string();
        for payload in parse_sse_data(&frame) {
          if tx.send(Ok(payload)).await.is_err() {
            return;
          }
        }
      }
    }
    if !buffer.trim().is_empty() {
      for payload in parse_sse_data(&buffer) {
        if tx.send(Ok(payload)).await.is_err() {
          return;
        }
      }
    }
  });
  Box::pin(ReceiverStream::new(rx))
}

fn parse_sse_data(frame: &str) -> Vec<String> {
  let payload = frame
    .lines()
    .filter_map(|line| line.strip_prefix("data:").map(|d| d.trim_start().to_string()))
    .collect::<Vec<String>>()
    .join("\n")
    .trim()
    .to_string();
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
      metadata: HashMap::new(),
    }
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
