use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;

use crate::config::{LoadedConfig, ModelRoute, ProviderCredential, ProviderDefinition};

pub mod claude;
pub mod copilot;
pub mod deepseek;
pub mod openai;

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send + 'static>>;

#[derive(Debug, Clone, Copy)]
pub struct ProviderCapabilities {
  pub chat_completion: bool,
  pub responses: bool,
  pub stream_chat_completion: bool,
  pub stream_responses: bool,
}

impl ProviderCapabilities {
  pub const fn all() -> Self {
    Self {
      chat_completion: true,
      responses: true,
      stream_chat_completion: true,
      stream_responses: true,
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
  #[error("http error: {0}")]
  Http(String),
  #[error("unauthorized")]
  Unauthorized,
  #[error("unsupported provider behavior: {0}")]
  Unsupported(String),
  #[error("internal provider error: {0}")]
  Internal(String),
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
  fn name(&self) -> &'static str;
  fn capabilities(&self, route: &ModelRoute) -> ProviderCapabilities;

  async fn chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError>;

  async fn responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError>;

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStream, ProviderError>;

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStream, ProviderError>;
}

#[derive(Clone)]
pub struct ProviderRegistry {
  adapters: Arc<HashMap<String, Arc<dyn ProviderAdapter>>>,
}

impl ProviderRegistry {
  pub fn new() -> Self {
    let mut adapters: HashMap<String, Arc<dyn ProviderAdapter>> = HashMap::new();
    adapters.insert("openai".to_string(), Arc::new(openai::OpenAiAdapter::new()));
    adapters.insert("deepseek".to_string(), Arc::new(deepseek::DeepSeekAdapter::new()));
    adapters.insert("claude".to_string(), Arc::new(claude::ClaudeAdapter::new()));
    adapters.insert(
      "github-copilot".to_string(),
      Arc::new(copilot::GitHubCopilotAdapter::new()),
    );

    Self {
      adapters: Arc::new(adapters),
    }
  }

  pub fn from_adapters(adapters: HashMap<String, Arc<dyn ProviderAdapter>>) -> Self {
    Self {
      adapters: Arc::new(adapters),
    }
  }

  pub fn adapter_for_provider(
    &self,
    loaded: &LoadedConfig,
    route: &ModelRoute,
  ) -> Result<(Arc<dyn ProviderAdapter>, ProviderDefinition, Option<ProviderCredential>)> {
    let provider_def = loaded
      .providers
      .providers
      .get(&route.provider)
      .ok_or_else(|| anyhow::anyhow!("provider '{}' not found", route.provider))?
      .clone();

    if !provider_def.enabled {
      anyhow::bail!("provider '{}' is disabled", route.provider);
    }

    let adapter = self
      .adapters
      .get(&provider_def.provider_type)
      .ok_or_else(|| anyhow::anyhow!("adapter '{}' not registered", provider_def.provider_type))?
      .clone();

    let creds = loaded.credentials.resolve_runtime_credential(&route.provider)?;
    Ok((adapter, provider_def, creds))
  }

  pub fn provider_status(&self, loaded: &LoadedConfig) -> Vec<serde_json::Value> {
    loaded
      .providers
      .providers
      .iter()
      .map(|(name, provider)| {
        serde_json::json!({
            "name": name,
            "provider_type": provider.provider_type,
            "base_url": provider.base_url,
            "enabled": provider.enabled,
            "adapter_registered": self.adapters.contains_key(&provider.provider_type)
        })
      })
      .collect()
  }
}
