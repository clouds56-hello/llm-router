use async_trait::async_trait;
use serde_json::Value;

use super::{ProviderAdapter, ProviderError, ProviderStream};
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

  async fn chat_completion(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<Value, ProviderError> {
    Err(ProviderError::Unsupported(
      "deepseek adapter TODO: implement provider-specific behavior".to_string(),
    ))
  }

  async fn responses(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<Value, ProviderError> {
    Err(ProviderError::Unsupported("deepseek responses TODO".to_string()))
  }

  async fn stream_chat_completion(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    Err(ProviderError::Unsupported("deepseek streaming TODO".to_string()))
  }
}
