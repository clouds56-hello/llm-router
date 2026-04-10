use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{ProviderAdapter, ProviderCapabilities, ProviderError, ProviderStream};
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
    url: String,
    creds: Option<&ProviderCredential>,
    body: Value,
  ) -> Result<Value, ProviderError> {
    let req = self
      .client
      .post(url)
      .header(CONTENT_TYPE, "application/json")
      .json(&body);

    let res = self
      .build_auth(req, creds)
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

    res
      .json::<Value>()
      .await
      .map_err(|e| ProviderError::Http(e.to_string()))
  }

  async fn post_stream(
    &self,
    url: String,
    creds: Option<&ProviderCredential>,
    body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let req = self
      .client
      .post(url)
      .header(CONTENT_TYPE, "application/json")
      .json(&body);
    let res = self
      .build_auth(req, creds)
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
    self
      .post_json(format!("{}/v1/chat/completions", config.base_url), creds, body)
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
      .post_json(format!("{}/v1/responses", config.base_url), creds, body)
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
      .post_stream(format!("{}/v1/chat/completions", config.base_url), creds, body)
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
      .post_stream(format!("{}/v1/responses", config.base_url), creds, body)
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
