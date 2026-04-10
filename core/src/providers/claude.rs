use async_trait::async_trait;
use serde_json::Value;

use super::{ProviderAdapter, ProviderError, ProviderStream};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

pub struct ClaudeAdapter;

impl ClaudeAdapter {
  pub fn new() -> Self {
    Self
  }
}

#[async_trait]
impl ProviderAdapter for ClaudeAdapter {
  fn name(&self) -> &'static str {
    "claude"
  }

  async fn chat_completion(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<Value, ProviderError> {
    Err(ProviderError::Unsupported(
      "claude adapter TODO: implement Anthropic request/response mapping".to_string(),
    ))
  }

  async fn responses(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<Value, ProviderError> {
    Err(ProviderError::Unsupported("claude responses TODO".to_string()))
  }

  async fn stream_chat_completion(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    Err(ProviderError::Unsupported("claude streaming TODO".to_string()))
  }
}
