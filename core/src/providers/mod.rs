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
pub mod openai_compat;
mod upstream_logging;
mod utils;

pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<String, ProviderError>> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderOperation {
  ChatCompletions,
  Responses,
}

pub struct ProviderStreamResponse {
  pub stream: ProviderStream,
  pub upstream_status: u16,
}

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
  #[error("http error: {message}")]
  Http { message: String, status_code: Option<u16> },
  #[error("unauthorized")]
  Unauthorized { status_code: u16 },
  #[error("unsupported provider behavior: {0}")]
  Unsupported(String),
  #[error("internal provider error: {message}")]
  Internal { message: String, status_code: Option<u16> },
}

impl ProviderError {
  pub fn http(message: impl Into<String>) -> Self {
    Self::Http {
      message: message.into(),
      status_code: None,
    }
  }

  pub fn http_with_status(message: impl Into<String>, status_code: u16) -> Self {
    Self::Http {
      message: message.into(),
      status_code: Some(status_code),
    }
  }

  pub fn internal(message: impl Into<String>) -> Self {
    Self::Internal {
      message: message.into(),
      status_code: None,
    }
  }

  pub fn status_code(&self) -> Option<u16> {
    match self {
      ProviderError::Http { status_code, .. } | ProviderError::Internal { status_code, .. } => *status_code,
      ProviderError::Unauthorized { status_code } => Some(*status_code),
      ProviderError::Unsupported(_) => None,
    }
  }
}

pub(crate) fn join_upstream_url(base: &str, path: &str) -> String {
  let left = base.trim_end_matches('/');
  let right = path.trim_start_matches('/');
  format!("{left}/{right}")
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
  fn upstream_request_body(
    &self,
    _operation: ProviderOperation,
    _stream: bool,
    _route: &ModelRoute,
    _provider: &ProviderDefinition,
    request_body: &Value,
  ) -> Value {
    request_body.clone()
  }
  fn upstream_path(
    &self,
    operation: ProviderOperation,
    stream: bool,
    route: &ModelRoute,
    provider: &ProviderDefinition,
  ) -> String;

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
  ) -> Result<ProviderStreamResponse, ProviderError>;

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError>;
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
    adapters.insert(
      "openai-compatible".to_string(),
      Arc::new(openai_compat::OpenAiCompatibleAdapter::new()),
    );
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
