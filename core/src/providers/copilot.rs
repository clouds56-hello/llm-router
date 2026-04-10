use async_trait::async_trait;
use serde_json::Value;

use super::{ProviderAdapter, ProviderError, ProviderStream};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

pub trait CopilotRequestDecorator: Send + Sync {
  fn classify_route(&self, route: &ModelRoute) -> String;
  fn decorate_headers(&self, headers: &mut reqwest::header::HeaderMap, creds: Option<&ProviderCredential>);
}

pub struct DefaultCopilotRequestDecorator;

impl CopilotRequestDecorator for DefaultCopilotRequestDecorator {
  fn classify_route(&self, route: &ModelRoute) -> String {
    if route.provider_model.contains("claude") {
      "anthropic-compatible".to_string()
    } else {
      "openai-compatible".to_string()
    }
  }

  fn decorate_headers(&self, headers: &mut reqwest::header::HeaderMap, creds: Option<&ProviderCredential>) {
    if let Some(token) = creds.and_then(|c| c.api_key.clone()) {
      let value = format!("Bearer {token}");
      if let Ok(header_val) = reqwest::header::HeaderValue::from_str(&value) {
        headers.insert(reqwest::header::AUTHORIZATION, header_val);
      }
    }
    headers.insert(
      reqwest::header::HeaderName::from_static("x-copilot-client"),
      reqwest::header::HeaderValue::from_static("llm-router"),
    );
  }
}

pub struct GitHubCopilotAdapter {
  decorator: Box<dyn CopilotRequestDecorator>,
}

impl GitHubCopilotAdapter {
  pub fn new() -> Self {
    Self {
      decorator: Box::new(DefaultCopilotRequestDecorator),
    }
  }
}

#[async_trait]
impl ProviderAdapter for GitHubCopilotAdapter {
  fn name(&self) -> &'static str {
    "github-copilot"
  }

  async fn chat_completion(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    _request_body: Value,
  ) -> Result<Value, ProviderError> {
    let route_kind = self.decorator.classify_route(route);
    Err(ProviderError::Unsupported(format!(
      "github-copilot adapter TODO: request path/headers for {route_kind}"
    )))
  }

  async fn responses(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<Value, ProviderError> {
    Err(ProviderError::Unsupported("github-copilot responses TODO".to_string()))
  }

  async fn stream_chat_completion(
    &self,
    _config: &ProviderDefinition,
    _creds: Option<&ProviderCredential>,
    _route: &ModelRoute,
    _request_body: Value,
  ) -> Result<ProviderStream, ProviderError> {
    Err(ProviderError::Unsupported("github-copilot streaming TODO".to_string()))
  }
}
