use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use async_trait::async_trait;
use futures::Stream;
use serde_json::Value;

use crate::config::{LoadedConfig, ModelRoute, ProviderCredential, ProviderDefinition};

pub mod claude;
pub mod copilot;
pub mod deepseek;
pub mod openai;
mod openai_compatible;
mod upstream_logging;

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

#[derive(Debug, Clone)]
pub(crate) struct UpstreamLogContext {
  pub provider: String,
  pub adapter: String,
  pub upstream_path: String,
  pub method: &'static str,
  pub model: Option<String>,
  pub stream: bool,
}

impl UpstreamLogContext {
  pub(crate) fn started(&self, body: &Value) -> Instant {
    upstream_logging::log_started(self, body)
  }

  pub(crate) fn completed(&self, started: Instant, status: u16) {
    upstream_logging::log_completed(self, started, status)
  }

  pub(crate) fn failed(&self, started: Instant, status: Option<u16>, snippet: Option<&str>) {
    upstream_logging::log_failed(self, started, status, snippet)
  }
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

pub struct ResolvedProvider {
  pub adapter: Arc<dyn ProviderAdapter>,
  pub provider_cfg: ProviderDefinition,
  pub creds: Option<ProviderCredential>,
  pub effective_account_id: Option<String>,
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
    account_id: Option<&str>,
  ) -> Result<ResolvedProvider> {
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

    let resolved = if let Some(account_id) = account_id {
      loaded
        .credentials
        .resolve_runtime_credential_for_account_with_account(&route.provider, account_id)?
    } else {
      loaded
        .credentials
        .resolve_runtime_credential_with_account(&route.provider)?
    };
    let (effective_account_id, creds) = match resolved {
      Some((id, cred)) => (Some(id), Some(cred)),
      None => (None, None),
    };
    Ok(ResolvedProvider {
      adapter,
      provider_cfg: provider_def,
      creds,
      effective_account_id,
    })
  }

  pub fn provider_status(&self, loaded: &LoadedConfig) -> Vec<serde_json::Value> {
    let mut items = loaded.providers.providers.iter().collect::<Vec<_>>();
    items.sort_by(|(name_a, _), (name_b, _)| name_a.cmp(name_b));

    items
      .into_iter()
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
