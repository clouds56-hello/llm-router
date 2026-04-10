use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{ProviderAdapter, ProviderCapabilities, ProviderError, ProviderStream};
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

  async fn post_json(&self, url: String, headers: HeaderMap, body: Value) -> Result<Value, ProviderError> {
    let res = self
      .client
      .post(url)
      .headers(headers)
      .json(&body)
      .send()
      .await
      .map_err(|e| ProviderError::Http(e.to_string()))?;
    let status = res.status();
    if status.as_u16() == 401 {
      return Err(ProviderError::Unauthorized);
    }
    if !status.is_success() {
      let text = res.text().await.unwrap_or_default();
      return Err(ProviderError::Http(format!(
        "upstream returned status {status}: {text}"
      )));
    }
    res
      .json::<Value>()
      .await
      .map_err(|e| ProviderError::Http(e.to_string()))
  }

  async fn post_stream(&self, url: String, headers: HeaderMap, body: Value) -> Result<ProviderStream, ProviderError> {
    let res = self
      .client
      .post(url)
      .headers(headers)
      .json(&body)
      .send()
      .await
      .map_err(|e| ProviderError::Http(e.to_string()))?;
    let status = res.status();
    if status.as_u16() == 401 {
      return Err(ProviderError::Unauthorized);
    }
    if !status.is_success() {
      return Err(ProviderError::Http(format!("upstream returned status {status}")));
    }
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
    self
      .post_json(
        format!("{}/v1/chat/completions", config.base_url),
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
    self
      .post_json(format!("{}/v1/responses", config.base_url), self.headers(creds), body)
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
    self
      .post_stream(
        format!("{}/v1/chat/completions", config.base_url),
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
    self
      .post_stream(format!("{}/v1/responses", config.base_url), self.headers(creds), body)
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
