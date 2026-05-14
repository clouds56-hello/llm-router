use crate::util::secret::Secret;
use async_trait::async_trait;
use llm_core::account::AccountConfig;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use reqwest::Method;
use serde_json::Value;
use snafu::ResultExt;
use std::sync::Arc;
use tracing::{debug, instrument, warn};

use crate::{error, AuthKind, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, ID_DEEPSEEK};

pub const DEFAULT_BASE_URL: &str = "https://api.deepseek.com";

pub struct DeepSeekProvider {
  pub id: String,
  api_key: Secret<String>,
  base_url: String,
  info: ProviderInfo,
}

impl DeepSeekProvider {
  pub fn validate_account(a: &AccountConfig) -> Result<()> {
    if a.provider != ID_DEEPSEEK {
      return error::ProviderMismatchSnafu {
        expected: ID_DEEPSEEK,
        got: a.provider.clone(),
      }
      .fail();
    }
    let _ = a
      .api_key
      .clone()
      .filter(|s| !s.expose().trim().is_empty())
      .ok_or(error::Error::MissingCredential {
        account: a.id.clone(),
        what: "api_key",
      })?;
    Ok(())
  }

  pub fn from_account(a: Arc<AccountConfig>) -> Result<Self> {
    Self::validate_account(&a)?;
    let key = a.api_key.clone().expect("validated api_key");
    let base_url = a.base_url.clone().unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    Ok(Self {
      id: format!("deepseek:{}", a.id),
      api_key: key,
      base_url: base_url.clone(),
      info: ProviderInfo {
        id: ID_DEEPSEEK.to_string(),
        aliases: &[ID_DEEPSEEK],
        display_name: "DeepSeek",
        upstream_url: base_url,
        auth_kind: AuthKind::StaticApiKey,
        default_models: crate::catalogue::default_models_for(ID_DEEPSEEK),
      },
    })
  }

  fn url(&self, path: &str) -> String {
    format!("{}{}", self.base_url.trim_end_matches('/'), path)
  }
}

#[async_trait]
impl Provider for DeepSeekProvider {
  fn id(&self) -> &str {
    &self.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  fn patch_headers(&self, headers: &mut HeaderMap, ctx: &HeaderPatchCtx<'_>) -> Result<()> {
    headers.insert(
      AUTHORIZATION,
      HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose()))
        .context(error::HeaderValueSnafu { name: "authorization" })?,
    );
    headers.insert(
      ACCEPT,
      HeaderValue::from_static(if ctx.stream {
        "text/event-stream"
      } else {
        "application/json"
      }),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(encoding) = ctx.content_encoding {
      headers.insert(
        reqwest::header::CONTENT_ENCODING,
        HeaderValue::from_str(encoding).context(error::HeaderValueSnafu {
          name: "content-encoding",
        })?,
      );
    }
    Ok(())
  }

  async fn list_models(&self, http: &reqwest::Client) -> Result<Value> {
    let mut headers = HeaderMap::new();
    self.patch_headers(
      &mut headers,
      &HeaderPatchCtx {
        endpoint: crate::Endpoint::ChatCompletions,
        body: &Value::Null,
        bearer_token: None,
        content_encoding: None,
        stream: false,
        initiator: "user",
        inbound_headers: &HeaderMap::new(),
      },
    )?;
    let resp = crate::util::http::send(
      http,
      Method::GET,
      &self.url("/models"),
      headers,
      None,
      None,
      "deepseek /models",
    )
    .await?;
    crate::util::http::read_json(resp, "deepseek /models").await
  }

  #[instrument(name = "deepseek_chat", skip_all, fields(account = %self.id, stream = ctx.stream))]
  async fn chat(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    let url = self.url("/chat/completions");
    debug!(%url, "POST deepseek chat");
    let mut headers = ctx.profile_headers.clone().unwrap_or_default();
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
      },
    )?;
    let body_bytes = ctx.request_body_bytes();
    let resp = crate::util::http::send(
      ctx.http,
      Method::POST,
      &url,
      headers,
      Some(body_bytes),
      ctx.outbound.as_ref(),
      "deepseek chat",
    )
    .await?;
    Ok(resp)
  }

  fn on_unauthorized(&self) {
    warn!(account = %self.id, key_fp = %self.api_key.fingerprint(), "deepseek upstream returned 401: api_key likely revoked or expired");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use llm_core::account::AccountTier;

  fn acct(key: Option<&str>) -> AccountConfig {
    AccountConfig {
      id: "test".into(),
      provider: ID_DEEPSEEK.into(),
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
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  #[test]
  fn rejects_missing_api_key() {
    let err = DeepSeekProvider::from_account(Arc::new(acct(None))).err().unwrap();
    assert!(err.to_string().contains("api_key"));
  }

  #[test]
  fn constructs_with_default_url() {
    let p = DeepSeekProvider::from_account(Arc::new(acct(Some("sk-test")))).unwrap();
    assert_eq!(p.info().id, ID_DEEPSEEK);
    assert_eq!(p.info().upstream_url, DEFAULT_BASE_URL);
  }
}
