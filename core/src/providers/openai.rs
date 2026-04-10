use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::{ProviderAdapter, ProviderError, ProviderStream};
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
}

#[async_trait]
impl ProviderAdapter for OpenAiAdapter {
  fn name(&self) -> &'static str {
    "openai"
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

    let req = self
      .client
      .post(format!("{}/v1/chat/completions", config.base_url))
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

    let stream = res.bytes_stream().map(|chunk| {
      chunk
        .map_err(|e| ProviderError::Http(e.to_string()))
        .and_then(|bytes| String::from_utf8(bytes.to_vec()).map_err(|e| ProviderError::Internal(e.to_string())))
    });

    Ok(Box::pin(stream))
  }
}
