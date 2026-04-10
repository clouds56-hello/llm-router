use async_trait::async_trait;
use serde_json::Value;

use super::openai::OpenAiAdapter;
use super::{ProviderAdapter, ProviderCapabilities, ProviderError, ProviderStream};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

pub struct DeepSeekAdapter;

impl DeepSeekAdapter {
  pub fn new() -> Self {
    Self
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

  async fn chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    OpenAiAdapter::new()
      .chat_completion(config, creds, route, request_body)
      .await
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
  ) -> Result<ProviderStream, ProviderError> {
    OpenAiAdapter::new()
      .stream_chat_completion(config, creds, route, request_body)
      .await
  }

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    let _ = (config, creds, route, request_body);
    Err(ProviderError::Unsupported(
      "deepseek upstream does not support responses streaming".to_string(),
    ))
  }
}
