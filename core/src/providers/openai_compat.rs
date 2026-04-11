use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;

use super::utils::{self, HttpErrorFormat};
use super::{
  join_upstream_url, ProviderAdapter, ProviderCapabilities, ProviderError, ProviderOperation, ProviderStreamResponse,
  UpstreamLogContext,
};
use crate::config::{ModelRoute, ProviderCredential, ProviderDefinition};

#[derive(Default)]
pub struct OpenAiCompatibleAdapter {
  client: reqwest::Client,
}

impl OpenAiCompatibleAdapter {
  pub fn new() -> Self {
    Self {
      client: reqwest::Client::new(),
    }
  }

  fn headers(&self, config: &ProviderDefinition, creds: Option<&ProviderCredential>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(token) = creds.and_then(|c| c.api_key.clone()) {
      if let Ok(header_val) = HeaderValue::from_str(&format!("Bearer {token}")) {
        headers.insert(AUTHORIZATION, header_val);
      }
    }
    utils::apply_config_headers(&mut headers, &config.headers);
    headers
  }
}

#[async_trait]
impl ProviderAdapter for OpenAiCompatibleAdapter {
  fn name(&self) -> &'static str {
    "openai-compatible"
  }

  fn capabilities(&self, _route: &ModelRoute) -> ProviderCapabilities {
    ProviderCapabilities::all()
  }

  fn upstream_request_body(
    &self,
    _operation: ProviderOperation,
    stream: bool,
    route: &ModelRoute,
    _provider: &ProviderDefinition,
    request_body: &Value,
  ) -> Value {
    let mut body = utils::with_model(route, request_body.clone());
    if stream {
      body = utils::with_stream(body);
    }
    body
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
    let body = utils::with_model(route, request_body);
    let upstream_path = self.upstream_path(ProviderOperation::ChatCompletions, false, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    utils::post_json(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
      HttpErrorFormat::StatusOnly,
    )
    .await
  }

  async fn responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<Value, ProviderError> {
    let body = utils::with_model(route, request_body);
    let upstream_path = self.upstream_path(ProviderOperation::Responses, false, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: false,
    };
    utils::post_json(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
      HttpErrorFormat::StatusOnly,
    )
    .await
  }

  async fn stream_chat_completion(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let body = utils::with_stream(utils::with_model(route, request_body));
    let upstream_path = self.upstream_path(ProviderOperation::ChatCompletions, true, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    utils::post_stream(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
    )
    .await
  }

  async fn stream_responses(
    &self,
    config: &ProviderDefinition,
    creds: Option<&ProviderCredential>,
    route: &ModelRoute,
    request_body: Value,
  ) -> Result<ProviderStreamResponse, ProviderError> {
    let body = utils::with_stream(utils::with_model(route, request_body));
    let upstream_path = self.upstream_path(ProviderOperation::Responses, true, route, config);
    let ctx = UpstreamLogContext {
      provider: route.provider.clone(),
      adapter: self.name().to_string(),
      upstream_path: upstream_path.clone(),
      method: "POST",
      model: body.get("model").and_then(|v| v.as_str()).map(str::to_string),
      stream: true,
    };
    utils::post_stream(
      &self.client,
      ctx,
      join_upstream_url(&config.base_url, &upstream_path),
      self.headers(config, creds),
      body,
    )
    .await
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use std::collections::HashMap;

  fn route() -> ModelRoute {
    ModelRoute {
      openai_name: "gpt-test".to_string(),
      provider: "utils".to_string(),
      provider_model: "upstream-model".to_string(),
      is_default: true,
    }
  }

  fn provider_def_with_codex_metadata() -> ProviderDefinition {
    ProviderDefinition {
      provider_type: "openai-compatible".to_string(),
      base_url: "http://unused".to_string(),
      enabled: true,
      headers: HashMap::new(),
      metadata: HashMap::from([("codex_api_mode".to_string(), "responses".to_string())]),
    }
  }

  #[test]
  fn ignores_codex_metadata_for_path_and_payload() {
    let adapter = OpenAiCompatibleAdapter::new();
    let route = route();
    let provider = provider_def_with_codex_metadata();
    assert_eq!(
      adapter.upstream_path(ProviderOperation::Responses, false, &route, &provider),
      "/v1/responses"
    );
    let body = adapter.upstream_request_body(
      ProviderOperation::Responses,
      false,
      &route,
      &provider,
      &serde_json::json!({"input":"hi"}),
    );
    assert!(body.get("instructions").is_none());
    assert!(body.get("store").is_none());
    assert_eq!(body.get("input").and_then(|v| v.as_str()), Some("hi"));
  }
}
