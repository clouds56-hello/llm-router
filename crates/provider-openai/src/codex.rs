use crate::common::{self, Credential};
use async_trait::async_trait;
use reqwest::Method;
use serde_json::Value;
use std::sync::Arc;
use tokn_core::account::AccountConfig;
use tokn_headers::keys::CHATGPT_ACCOUNT_ID;
use tokn_headers::{HeaderMap, HeaderValue};
use tracing::{debug, instrument, warn};

use crate::{
  error, AuthKind, Endpoint, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, TemplateVars, ID_CODEX,
};

pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
pub const CODEX_CLIENT_VERSION: &str = "0.130.0";
const CODEX_MODELS_PATH: &str = "/models?client_version=0.130.0";

pub struct CodexProvider {
  pub id: String,
  credential: Credential,
  base_url: String,
  provider_account_id: Option<String>,
  info: ProviderInfo,
}

impl CodexProvider {
  pub fn from_account(a: Arc<AccountConfig>) -> Result<Self> {
    validate(a.as_ref())?;
    let credential = Credential::AccessToken(
      a.access_token
        .clone()
        .or_else(|| a.api_key.clone())
        .expect("validated access token"),
    );
    let base = a.base_url.clone().unwrap_or_else(|| CODEX_BASE_URL.to_string());
    Ok(Self {
      id: format!("{}:{}", a.provider, a.id),
      credential,
      base_url: base.clone(),
      provider_account_id: a.provider_account_id.clone(),
      info: ProviderInfo {
        id: a.provider.clone(),
        aliases: &[],
        display_name: "ChatGPT Codex",
        upstream_url: base,
        auth_kind: AuthKind::OAuthDeviceFlow,
        default_models: crate::catalogue::default_models_for(crate::ID_OPENAI),
        default_endpoints: crate::DEFAULT_ENDPOINTS_CODEX,
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
  if account.provider != ID_CODEX {
    return error::ProviderMismatchSnafu {
      expected: ID_CODEX,
      got: account.provider.clone(),
    }
    .fail();
  }
  let _ = account
    .access_token
    .clone()
    .or_else(|| account.api_key.clone())
    .filter(|s| !s.expose().trim().is_empty())
    .ok_or(error::Error::MissingCredential {
      account: account.id.clone(),
      what: "access_token",
    })?;
  Ok(())
}

pub fn build(account: Arc<AccountConfig>) -> Result<Arc<dyn Provider>> {
  Ok(Arc::new(CodexProvider::from_account(account)?))
}

#[async_trait]
impl Provider for CodexProvider {
  fn id(&self) -> &str {
    &self.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  fn patch_headers(&self, headers: &mut HeaderMap, ctx: &HeaderPatchCtx<'_>) -> Result<()> {
    common::patch_openai_headers(headers, self.credential.expose(), ctx)?;
    if let Some(pid) = self.provider_account_id.as_deref().filter(|s| !s.is_empty()) {
      headers.insert(&CHATGPT_ACCOUNT_ID, HeaderValue::from_string(pid.to_string()));
    }
    Ok(())
  }

  async fn list_models(&self, http: &reqwest::Client) -> Result<Value> {
    let headers = self.models_headers()?;
    let resp = crate::util::http::send(
      http,
      Method::GET,
      &self.url(CODEX_MODELS_PATH),
      headers,
      None,
      None,
      "codex /models",
    )
    .await?;
    let value: Value = crate::util::http::read_json(resp, "codex /models").await?;
    Ok(normalize_models_response(value))
  }

  #[instrument(name = "codex_responses", skip_all, fields(account = %self.id, stream = ctx.stream))]
  async fn responses(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    self.upstream_post(ctx, "/responses", "codex responses").await
  }

  async fn chat(&self, _ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    error::UnsupportedEndpointSnafu {
      provider: self.info.id.clone(),
      endpoint: "/v1/chat/completions",
    }
    .fail()
  }

  fn on_unauthorized(&self) {
    warn!(account = %self.id, "codex upstream returned 401: credential likely revoked or expired");
  }
}

impl CodexProvider {
  fn models_headers(&self) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    self.patch_headers(
      &mut headers,
      &HeaderPatchCtx {
        endpoint: Endpoint::Responses,
        body: &Value::Null,
        bearer_token: None,
        content_encoding: None,
        stream: false,
        initiator: "user",
        inbound_headers: &HeaderMap::new(),
        vars: &TemplateVars::default(),
      },
    )?;
    Ok(headers)
  }
}

fn normalize_models_response(value: Value) -> Value {
  if value.get("data").and_then(|v| v.as_array()).is_some() {
    return value;
  }
  let models = find_models_array(&value).cloned().unwrap_or_default();
  let data = models.into_iter().filter_map(normalize_model_entry).collect::<Vec<_>>();
  serde_json::json!({ "object": "list", "data": data })
}

fn find_models_array(value: &Value) -> Option<&Vec<Value>> {
  if let Some(arr) = value.as_array() {
    return Some(arr);
  }
  if let Some(arr) = value.get("models").and_then(|v| v.as_array()) {
    return Some(arr);
  }
  value
    .as_object()
    .and_then(|obj| obj.values().find_map(|v| v.get("models").and_then(|m| m.as_array())))
}

fn normalize_model_entry(entry: Value) -> Option<Value> {
  if let Some(id) = entry.as_str().filter(|id| !id.trim().is_empty()) {
    return Some(serde_json::json!({ "id": id, "object": "model" }));
  }
  let obj = entry.as_object()?;
  let id = ["id", "model", "slug", "name"]
    .iter()
    .find_map(|key| obj.get(*key).and_then(|v| v.as_str()))?
    .trim();
  if id.is_empty() {
    return None;
  }
  Some(serde_json::json!({
    "id": id,
    "object": "model",
    "x_codex": entry,
  }))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::util::secret::Secret;
  use tokn_core::account::AccountTier;

  fn acct(access: Option<&str>) -> AccountConfig {
    AccountConfig {
      id: "test".into(),
      provider: ID_CODEX.into(),
      enabled: true,
      tier: AccountTier::Active,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: None,
      username: None,
      api_key: None,
      api_key_expires_at: None,
      access_token: access.map(|s| Secret::new(s.into())),
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
  fn codex_requires_access_token() {
    let err = CodexProvider::from_account(Arc::new(acct(None))).err().unwrap();
    assert!(err.to_string().contains("access_token"));
  }

  #[test]
  fn codex_patch_headers_injects_account_id_when_present() {
    let mut a = acct(Some("atk-test"));
    a.provider_account_id = Some("acc-77".into());
    let codex = CodexProvider::from_account(Arc::new(a)).unwrap();
    let mut h = HeaderMap::new();
    codex.patch_headers(&mut h, &patch_ctx()).unwrap();
    assert_eq!(h.get("authorization").unwrap().as_str(), "Bearer atk-test");
    assert_eq!(h.get("chatgpt-account-id").unwrap().as_str(), "acc-77");
  }

  #[test]
  fn codex_patch_headers_omits_account_id_when_absent_or_blank() {
    for blank in [None, Some(String::new())] {
      let mut a = acct(Some("atk-test"));
      a.provider_account_id = blank;
      let codex = CodexProvider::from_account(Arc::new(a)).unwrap();
      let mut h = HeaderMap::new();
      codex.patch_headers(&mut h, &patch_ctx()).unwrap();
      assert!(h.get("chatgpt-account-id").is_none());
    }
  }

  #[test]
  fn codex_constructs() {
    let codex = CodexProvider::from_account(Arc::new(acct(Some("atk-test")))).unwrap();
    assert_eq!(codex.info().upstream_url, CODEX_BASE_URL);
    assert!(codex.supports("", Endpoint::Responses));
    assert!(!codex.supports("", Endpoint::ChatCompletions));
  }

  #[test]
  fn codex_models_url_uses_backend_api_path() {
    let codex = CodexProvider::from_account(Arc::new(acct(Some("atk-test")))).unwrap();
    assert_eq!(codex.url("/models"), "https://chatgpt.com/backend-api/codex/models");
    assert_eq!(
      codex.url(CODEX_MODELS_PATH),
      "https://chatgpt.com/backend-api/codex/models?client_version=0.130.0"
    );
  }

  #[test]
  fn normalizes_models_array_response() {
    let out = normalize_models_response(serde_json::json!({ "models": [{ "slug": "gpt-5" }, "o3"] }));
    let data = out.get("data").and_then(|v| v.as_array()).unwrap();
    assert_eq!(data[0].get("id").and_then(|v| v.as_str()), Some("gpt-5"));
    assert_eq!(data[1].get("id").and_then(|v| v.as_str()), Some("o3"));
  }

  #[test]
  fn normalizes_nested_models_array_response() {
    let out = normalize_models_response(serde_json::json!({ "app": { "models": [{ "model": "codex-mini" }] } }));
    let data = out.get("data").and_then(|v| v.as_array()).unwrap();
    assert_eq!(data[0].get("id").and_then(|v| v.as_str()), Some("codex-mini"));
  }

  #[test]
  fn leaves_openai_data_response_unchanged() {
    let input = serde_json::json!({ "object": "list", "data": [{ "id": "x" }] });
    assert_eq!(normalize_models_response(input.clone()), input);
  }
}
