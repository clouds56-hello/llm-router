use crate::common::{self, Credential};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::Value;
use std::sync::Arc;
use tokn_core::account::AccountConfig;
use tokn_headers::HeaderMap;
use tracing::{debug, instrument, warn};

use crate::{
  error, AuthKind, Endpoint, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, TemplateVars, ID_OPENAI,
};

pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiProvider {
  pub id: String,
  credential: Credential,
  base_url: String,
  info: ProviderInfo,
}

impl OpenAiProvider {
  pub fn from_account(a: Arc<AccountConfig>) -> Result<Self> {
    validate(a.as_ref())?;
    let credential = Credential::ApiKey(a.api_key.clone().expect("validated api_key"));
    let base = a.base_url.clone().unwrap_or_else(|| OPENAI_BASE_URL.to_string());
    Ok(Self {
      id: format!("{}:{}", a.provider, a.id),
      credential,
      base_url: base.clone(),
      info: ProviderInfo {
        id: a.provider.clone(),
        aliases: &[],
        display_name: "OpenAI",
        upstream_url: base,
        auth_kind: AuthKind::StaticApiKey,
        default_models: crate::catalogue::default_models_for(ID_OPENAI),
        default_endpoints: crate::DEFAULT_ENDPOINTS_OPENAI,
        model_cache: Arc::new(tokn_core::provider::ModelCache::default()),
      },
    })
  }

  fn url(&self, path: &str) -> String {
    common::url(&self.base_url, path)
  }

  async fn upstream_post(&self, ctx: RequestCtx<'_>, path: &str, what: &'static str) -> Result<reqwest::Response> {
    let url = self.url(path);
    debug!(%url, "POST upstream");
    let mut headers = ctx.client_headers.clone().unwrap_or_default();
    self.patch_headers(
      &mut headers,
      &HeaderPatchCtx {
        endpoint: ctx.endpoint,
        body: ctx.body,
        bearer_token: None,
        content_encoding: ctx.content_encoding,
        stream: ctx.stream,
        initiator: ctx.initiator,
        inbound_headers: ctx.inbound_headers,
        vars: &ctx.vars,
      },
    )?;
    let body_bytes = ctx.request_body_bytes();
    crate::util::http::send(
      ctx.http,
      Method::POST,
      &url,
      headers,
      Some(body_bytes),
      ctx.outbound.as_ref(),
      what,
    )
    .await
  }
}

pub fn validate(account: &AccountConfig) -> Result<()> {
  if account.provider != ID_OPENAI {
    return error::ProviderMismatchSnafu {
      expected: ID_OPENAI,
      got: account.provider.clone(),
    }
    .fail();
  }
  let _ = account
    .api_key
    .clone()
    .filter(|s| !s.expose().trim().is_empty())
    .ok_or(error::Error::MissingCredential {
      account: account.id.clone(),
      what: "api_key",
    })?;
  Ok(())
}

pub fn build(account: Arc<AccountConfig>) -> Result<Arc<dyn Provider>> {
  Ok(Arc::new(OpenAiProvider::from_account(account)?))
}

#[async_trait]
impl Provider for OpenAiProvider {
  fn id(&self) -> &str {
    &self.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  fn patch_headers(&self, headers: &mut HeaderMap, ctx: &HeaderPatchCtx<'_>) -> Result<()> {
    common::patch_openai_headers(headers, self.credential.expose(), ctx)
  }

  async fn list_models(&self, http: &reqwest::Client) -> Result<Value> {
    let mut headers = HeaderMap::new();
    self.patch_headers(
      &mut headers,
      &HeaderPatchCtx {
        endpoint: Endpoint::ChatCompletions,
        body: &Value::Null,
        bearer_token: None,
        content_encoding: None,
        stream: false,
        initiator: "user",
        inbound_headers: &HeaderMap::new(),
        vars: &TemplateVars::default(),
      },
    )?;
    let resp = crate::util::http::send(
      http,
      Method::GET,
      &self.url("/models"),
      headers,
      None,
      None,
      "openai /models",
    )
    .await?;
    crate::util::http::read_json(resp, "openai /models").await
  }

  #[instrument(name = "openai_chat", skip_all, fields(account = %self.id, stream = ctx.stream))]
  async fn chat(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    self.upstream_post(ctx, "/chat/completions", "openai chat").await
  }

  #[instrument(name = "openai_responses", skip_all, fields(account = %self.id, stream = ctx.stream))]
  async fn responses(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    self.upstream_post(ctx, "/responses", "openai responses").await
  }

  fn on_unauthorized(&self) {
    warn!(account = %self.id, "openai upstream returned 401: credential likely revoked or expired");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::util::secret::Secret;
  use tokn_core::account::AccountTier;
  use tokn_mock_server::{HeaderExpectation, MockAuthConfig, MockLlmConfig, MockLlmServer, MockRoute};

  fn acct(key: Option<&str>) -> AccountConfig {
    AccountConfig {
      id: "test".into(),
      provider: ID_OPENAI.into(),
      enabled: true,
      tier: AccountTier::Active,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: None,
      username: None,
      api_key: key.map(|s| Secret::new(s.into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  fn patch_ctx() -> HeaderPatchCtx<'static> {
    HeaderPatchCtx {
      endpoint: Endpoint::Responses,
      body: &Value::Null,
      bearer_token: None,
      content_encoding: None,
      stream: false,
      initiator: "user",
      inbound_headers: Box::leak(Box::new(HeaderMap::new())),
      vars: Box::leak(Box::new(TemplateVars::default())),
    }
  }

  #[test]
  fn openai_requires_api_key() {
    let err = OpenAiProvider::from_account(Arc::new(acct(None))).err().unwrap();
    assert!(err.to_string().contains("api_key"));
  }

  #[test]
  fn openai_patch_headers_never_sets_account_id() {
    let mut a = acct(Some("sk-test"));
    a.provider_account_id = Some("acc-ignored".into());
    let openai = OpenAiProvider::from_account(Arc::new(a)).unwrap();
    let mut h = HeaderMap::new();
    openai.patch_headers(&mut h, &patch_ctx()).unwrap();
    assert!(h.get("chatgpt-account-id").is_none());
  }

  #[test]
  fn openai_constructs() {
    let openai = OpenAiProvider::from_account(Arc::new(acct(Some("sk-test")))).unwrap();
    assert_eq!(openai.info().upstream_url, OPENAI_BASE_URL);
    assert!(openai.supports("", Endpoint::Responses));
    assert!(openai.supports("", Endpoint::ChatCompletions));
  }

  #[tokio::test]
  async fn openai_list_models_works_with_mock_server() {
    let server = MockLlmServer::start(
      MockLlmConfig {
        routes: vec![MockRoute::models(["gpt-4o-mini", "gpt-4.1"])],
        ..Default::default()
      }
      .with_auth(MockAuthConfig::bearer(["sk-test"]))
      .require_header(HeaderExpectation::equals("accept", "application/json"))
      .require_header(HeaderExpectation::equals("content-type", "application/json")),
    )
    .await;

    let mut account = acct(Some("sk-test"));
    account.base_url = Some(server.base_url().to_string());
    let provider = OpenAiProvider::from_account(Arc::new(account)).unwrap();

    let models = provider.list_models(&reqwest::Client::new()).await.unwrap();
    let ids: Vec<&str> = models["data"]
      .as_array()
      .unwrap()
      .iter()
      .filter_map(|model| model["id"].as_str())
      .collect();

    assert_eq!(ids, vec!["gpt-4o-mini", "gpt-4.1"]);

    let captured = server.last_request().expect("captured models request");
    assert_eq!(captured.method, reqwest::Method::GET);
    assert_eq!(captured.path, "/models");
    assert_eq!(captured.header("authorization"), Some("Bearer sk-test"));
  }

  #[tokio::test]
  async fn openai_chat_works_with_mock_server() {
    let server = MockLlmServer::start(
      MockLlmConfig {
        routes: vec![MockRoute::chat_completions()],
        ..Default::default()
      }
      .with_auth(MockAuthConfig::bearer(["sk-test"]))
      .require_header(HeaderExpectation::equals("accept", "application/json"))
      .require_header(HeaderExpectation::equals("content-type", "application/json")),
    )
    .await;

    let mut account = acct(Some("sk-test"));
    account.base_url = Some(server.base_url().to_string());
    let provider = OpenAiProvider::from_account(Arc::new(account)).unwrap();

    let http = reqwest::Client::new();
    let body = serde_json::json!({
      "model": "gpt-4o-mini",
      "messages": [{"role": "user", "content": "hi"}]
    });
    let inbound = HeaderMap::new();
    let response = provider
      .chat(RequestCtx {
        endpoint: Endpoint::ChatCompletions,
        http: &http,
        body: &body,
        body_bytes: None,
        content_encoding: None,
        stream: false,
        initiator: "user",
        inbound_headers: &inbound,
        client_headers: None,
        outbound: None,
        vars: TemplateVars::default(),
      })
      .await
      .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let captured = server.last_request().expect("captured chat request");
    assert_eq!(captured.method, reqwest::Method::POST);
    assert_eq!(captured.path, "/chat/completions");
    assert_eq!(captured.header("authorization"), Some("Bearer sk-test"));
    let payload: Value = serde_json::from_slice(&captured.body).unwrap();
    assert_eq!(payload["model"], "gpt-4o-mini");
  }
}
