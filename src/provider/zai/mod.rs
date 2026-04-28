//! Z.ai (a.k.a. Zhipu AI / bigmodel.cn) provider.
//!
//! Targets Z.ai's OpenAI-compatible coding-plan endpoint
//! (`https://api.z.ai/api/coding/paas/v4`). The same backend implementation is
//! exposed under four wire-aliases that all behave identically:
//!   - `zai-coding-plan` (canonical)
//!   - `zai`
//!   - `zhipuai-coding-plan`
//!   - `zhipuai`
//!
//! Authentication is a single static `Authorization: Bearer <api_key>` header;
//! no token exchange. For models flagged `capabilities.reasoning = true` we
//! inject a `thinking: { type: "enabled", clear_thinking: false }` block into
//! the outgoing request body, mirroring the contract upstream coding tools
//! (Claude Code, opencode) rely on.

pub mod models;
pub mod quota;
pub mod transform;

use crate::config::Account;
use crate::util::redact::token_fingerprint;
use crate::util::secret::Secret;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::Value;
use snafu::ResultExt;
use tracing::{debug, instrument, warn};

use super::{error, AuthKind, ModelInfo, Provider, ProviderInfo, RequestCtx, Result, ZAI_ALIASES};

/// Default upstream for the coding plan. Override per-account via
/// `[accounts.<id>.zai] base_url = "..."`.
pub const DEFAULT_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

pub struct ZaiProvider {
  pub id: String,
  api_key: Secret<String>,
  base_url: String,
  info: ProviderInfo,
}

impl std::fmt::Debug for ZaiProvider {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    // Deliberately omit `api_key` so it never lands in logs or test
    // panic output.
    f.debug_struct("ZaiProvider")
      .field("id", &self.id)
      .field("base_url", &self.base_url)
      .field("provider", &self.info.id)
      .finish()
  }
}

impl ZaiProvider {
  pub fn from_account(a: &Account) -> Result<Self> {
    if !ZAI_ALIASES.contains(&a.provider.as_str()) {
      return error::ProviderMismatchSnafu {
        expected: "zai-coding-plan|zai|zhipuai-coding-plan|zhipuai",
        got: a.provider.clone(),
      }
      .fail();
    }
    let key = a
      .api_key
      .clone()
      .filter(|s| !s.expose().trim().is_empty())
      .ok_or(error::Error::MissingCredential {
        account: a.id.clone(),
        what: "api_key",
      })?;
    let base_url = a
      .zai
      .as_ref()
      .and_then(|z| z.base_url.clone())
      .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());

    let info = ProviderInfo {
      id: a.provider.clone(),
      aliases: ZAI_ALIASES,
      display_name: "Z.ai (GLM Coding Plan)",
      upstream_url: base_url.clone(),
      auth_kind: AuthKind::StaticApiKey,
      default_models: models::catalogue_for(&a.provider),
    };
    Ok(Self {
      id: format!("{}:{}", a.provider, a.id),
      api_key: key,
      base_url,
      info,
    })
  }

  fn auth_headers(&self, streaming: bool) -> Result<HeaderMap> {
    let mut m = HeaderMap::new();
    m.insert(
      AUTHORIZATION,
      HeaderValue::from_str(&format!("Bearer {}", self.api_key.expose()))
        .context(error::HeaderValueSnafu { name: "authorization" })?,
    );
    m.insert(
      ACCEPT,
      HeaderValue::from_static(if streaming {
        "text/event-stream"
      } else {
        "application/json"
      }),
    );
    m.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    Ok(m)
  }
}

#[async_trait]
impl Provider for ZaiProvider {
  fn id(&self) -> &str {
    &self.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  fn model_info(&self, model: &str) -> Option<&ModelInfo> {
    self.info.default_models.iter().find(|m| m.id == model)
  }

  #[instrument(
    name = "zai_list_models",
    skip_all,
    fields(account = %self.id, key_fp = %token_fingerprint(self.api_key.expose()), status = tracing::field::Empty, count = tracing::field::Empty),
  )]
  async fn list_models(&self, http: &reqwest::Client) -> Result<Value> {
    let url = format!("{}/models", self.base_url.trim_end_matches('/'));
    crate::server::record_upstream_url(&url);
    debug!(%url, "GET zai models");
    let resp = http
      .get(&url)
      .headers(self.auth_headers(false)?)
      .send()
      .await
      .context(error::HttpSnafu { what: "zai /models" })?;
    let status = resp.status();
    tracing::Span::current().record("status", status.as_u16());
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
      return error::HttpStatusSnafu {
        what: "zai /models",
        status,
        body,
      }
      .fail();
    }
    // Some Z.ai deployments return `{ data: [...] }`, others return a bare
    // array. Normalise either way.
    let v: Value = serde_json::from_str(&body).context(error::JsonSnafu {
      what: "zai /models",
      body: body.clone(),
    })?;
    let data: Vec<Value> = match &v {
      Value::Object(_) => v.get("data").and_then(|d| d.as_array()).cloned().unwrap_or_default(),
      Value::Array(a) => a.clone(),
      _ => Vec::new(),
    };
    tracing::Span::current().record("count", data.len());
    Ok(serde_json::json!({ "object": "list", "data": data }))
  }

  #[instrument(
    name = "zai_chat",
    skip_all,
    fields(
      account = %self.id,
      key_fp = %token_fingerprint(self.api_key.expose()),
      stream = ctx.stream,
      model = tracing::field::Empty,
      reasoning = tracing::field::Empty,
      status = tracing::field::Empty,
    ),
  )]
  async fn chat(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    let model_id = ctx.body.get("model").and_then(|v| v.as_str()).unwrap_or("");
    // Reasoning gating: known models drive it explicitly; unknown GLM
    // models default to enabled (matches Z.ai's own clients).
    let reasoning = self
      .model_info(model_id)
      .map(|m| m.capabilities.reasoning)
      .unwrap_or_else(|| model_id.starts_with("glm-"));
    let span = tracing::Span::current();
    span.record("model", model_id);
    span.record("reasoning", reasoning);

    let body = transform::shape_request(ctx.body, reasoning);

    let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
    crate::server::record_upstream_url(&url);
    debug!(%url, "POST zai chat");
    let headers = self.auth_headers(ctx.stream)?;
    let body_bytes = bytes::Bytes::from(serde_json::to_vec(&body).unwrap_or_default());
    ctx.capture_outbound("POST", &url, &headers, body_bytes.clone());
    let resp = ctx
      .http
      .post(&url)
      .headers(headers)
      .body(body_bytes)
      .send()
      .await
      .context(error::HttpSnafu { what: "zai chat" })?;
    span.record("status", resp.status().as_u16());
    Ok(resp)
  }

  fn on_unauthorized(&self) {
    // Static API keys cannot be silently refreshed; the operator must
    // rotate. We log loudly so they notice.
    warn!(
      account = %self.id,
      key_fp = %token_fingerprint(self.api_key.expose()),
      "zai upstream returned 401: api_key likely revoked or expired"
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::Account as AcctCfg;
  use crate::provider::{new_outbound_capture, Endpoint, RequestCtx};
  use axum::http::HeaderMap;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};

  fn acct(provider: &str, key: Option<&str>) -> AcctCfg {
    AcctCfg {
      id: "test".into(),
      provider: provider.into(),
      github_token: None,
      api_token: None,
      api_token_expires_at: None,
      api_key: key.map(|s| Secret::new(s.into())),
      copilot: None,
      zai: None,
      behave_as: None,
    }
  }

  #[test]
  fn rejects_missing_api_key() {
    let err = ZaiProvider::from_account(&acct("zai-coding-plan", None)).unwrap_err();
    assert!(err.to_string().contains("api_key"), "{err}");
  }

  #[test]
  fn rejects_blank_api_key() {
    let err = ZaiProvider::from_account(&acct("zai-coding-plan", Some("   "))).unwrap_err();
    assert!(err.to_string().contains("api_key"), "{err}");
  }

  #[test]
  fn rejects_non_zai_provider_id() {
    let err = ZaiProvider::from_account(&acct("github-copilot", Some("sk-x"))).unwrap_err();
    assert!(err.to_string().contains("provider mismatch"), "{err}");
  }

  #[test]
  fn all_four_aliases_construct_and_preserve_canonical_id() {
    for alias in ZAI_ALIASES {
      let p = ZaiProvider::from_account(&acct(alias, Some("sk-x"))).unwrap();
      assert_eq!(p.info().id, *alias, "info().id should preserve operator alias");
      assert_eq!(p.info().display_name, "Z.ai (GLM Coding Plan)");
      assert_eq!(p.info().auth_kind, AuthKind::StaticApiKey);
      assert!(!p.info().default_models.is_empty());
    }
  }

  #[test]
  fn defaults_to_official_endpoint() {
    let p = ZaiProvider::from_account(&acct("zai", Some("sk-x"))).unwrap();
    assert_eq!(p.base_url, DEFAULT_BASE_URL);
  }

  #[test]
  fn respects_base_url_override() {
    let mut a = acct("zhipuai", Some("sk-x"));
    a.zai = Some(crate::config::ZaiAccountConfig {
      base_url: Some("https://open.bigmodel.cn/api/paas/v4".into()),
    });
    let p = ZaiProvider::from_account(&a).unwrap();
    assert_eq!(p.base_url, "https://open.bigmodel.cn/api/paas/v4");
  }

  #[tokio::test]
  async fn captures_transformed_outbound_body() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = vec![0_u8; 8192];
      let n = stream.read(&mut buf).await.unwrap();
      assert!(String::from_utf8_lossy(&buf[..n]).contains("POST /chat/completions"));
      stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}")
        .await
        .unwrap();
    });

    let mut cfg = acct("zai-coding-plan", Some("sk-test"));
    cfg.zai = Some(crate::config::ZaiAccountConfig {
      base_url: Some(format!("http://{addr}")),
    });
    let provider = ZaiProvider::from_account(&cfg).unwrap();
    let http = reqwest::Client::new();
    let body = serde_json::json!({ "model": "glm-4.6", "messages": [{ "role": "user", "content": "hi" }] });
    let inbound = HeaderMap::new();
    let capture = new_outbound_capture();
    let ctx = RequestCtx {
      endpoint: Endpoint::ChatCompletions,
      http: &http,
      body: &body,
      stream: false,
      initiator: "user",
      inbound_headers: &inbound,
      behave_as: None,
      outbound: Some(capture.clone()),
    };
    let resp = provider.chat(ctx).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    server.await.unwrap();
    let captured = capture.get().expect("captured outbound");
    let captured_body: Value = serde_json::from_slice(captured.body.as_ref()).unwrap();
    assert_eq!(captured.method.as_deref(), Some("POST"));
    assert!(
      captured_body.get("thinking").is_some(),
      "body was not transformed: {captured_body}"
    );
  }
}
