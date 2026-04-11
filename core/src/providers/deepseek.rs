use async_trait::async_trait;
use serde_json::Value;

use super::openai::OpenAiAdapter;
use super::{ProviderAdapter, ProviderCapabilities, ProviderError, ProviderOperation, ProviderStreamResponse};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

pub struct DeepSeekAdapter {
  inner: OpenAiAdapter,
}

impl DeepSeekAdapter {
  pub fn new() -> Self {
    Self {
      inner: OpenAiAdapter::new(),
    }
  }
}

#[async_trait]
impl ProviderAdapter for DeepSeekAdapter {
  fn name(&self) -> &'static str {
    "deepseek"
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
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    self.inner.chat_completion(config, creds, route, request_body).await
  }

  async fn responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let _ = (config, creds, route, request_body);
    Err(ProviderError::Unsupported(
      "deepseek upstream does not support responses".to_string(),
    ))
  }

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    self
      .inner
      .stream_chat_completion(config, creds, route, request_body)
      .await
  }

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let _ = (config, creds, route, request_body);
    Err(ProviderError::Unsupported(
      "deepseek upstream does not support responses streaming".to_string(),
    ))
  }
}
