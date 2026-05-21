//! GitHub Copilot provider.

pub use crate::{headers, models, oauth, token, user};

use crate::config::{CopilotHeaders, InitiatorMode};
use crate::util::secret::Secret;
use async_trait::async_trait;
use parking_lot::RwLock;
use reqwest::Method;
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex as AsyncMutex;
use tokn_core::account::AccountConfig;
use tokn_core::pipeline::InputTransformer;
use tokn_headers::keys::{ACCEPT, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE};
use tokn_headers::{HeaderMap, HeaderName, HeaderValue};
use tracing::{debug, instrument};

use crate::{
  error, AuthKind, Endpoint, EndpointRule, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result,
  ID_GITHUB_COPILOT,
};

#[allow(dead_code)]
pub const GITHUB_API: &str = "https://api.github.com";
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
/// Cached short-lived API token state.
struct ApiToken {
  token: Option<Secret<String>>,
  expires_at: Option<i64>,
}

pub struct CopilotProvider {
  #[allow(dead_code)]
  pub id: String,
  pub refresh_token: Secret<String>,
  pub headers: CopilotHeaders,
  refresh_lock: AsyncMutex<()>,
  cache: RwLock<ApiToken>,
  info: ProviderInfo,
}

fn copilot_info() -> &'static ProviderInfo {
  static CELL: OnceLock<ProviderInfo> = OnceLock::new();
  CELL.get_or_init(|| ProviderInfo {
    id: ID_GITHUB_COPILOT.to_string(),
    aliases: &[ID_GITHUB_COPILOT],
    display_name: "GitHub Copilot",
    upstream_url: COPILOT_API.to_string(),
    auth_kind: AuthKind::OAuthDeviceFlow,
    // Copilot's `/models` upstream is the source of truth for model
    // *identity*; the catalogue below provides metadata overlay for the
    // ids that models.dev tracks. Unknown ids still pass through
    // `/v1/models` — they just lack the `x_tokn_router` enrichment block.
    default_models: crate::catalogue::default_models_for(ID_GITHUB_COPILOT),
    default_endpoints: crate::DEFAULT_ENDPOINTS,
    model_cache: std::sync::Arc::new(tokn_core::provider::ModelCache::default()),
  })
}

impl CopilotProvider {
  pub fn validate_account(a: &AccountConfig) -> Result<()> {
    let _ = a.refresh_token.clone().ok_or(error::Error::MissingCredential {
      account: a.id.clone(),
      what: "refresh_token",
    })?;
    let headers = headers_from_settings(a)?;
    headers.validate()?;
    Ok(())
  }

  pub fn from_account(a: Arc<AccountConfig>) -> Result<Self> {
    Self::validate_account(&a)?;
    let gh = a.refresh_token.clone().expect("validated refresh_token");
    let headers = headers_from_settings(&a)?;
    Ok(Self {
      id: format!("github-copilot:{}", a.id),
      refresh_token: gh,
      headers,
      refresh_lock: AsyncMutex::new(()),
      cache: RwLock::new(ApiToken {
        token: a.access_token.clone(),
        expires_at: a.access_token_expires_at,
      }),
      info: copilot_info().clone(),
    })
  }

  fn snapshot(&self) -> (Option<Secret<String>>, Option<i64>) {
    let g = self.cache.read();
    (g.token.clone(), g.expires_at)
  }

  /// Ensure we have a non-expired Copilot API token; refresh if needed.
  #[instrument(name = "ensure_api_token", skip_all, fields(account = %self.id, refreshed = tracing::field::Empty, fp = tracing::field::Empty))]
  pub async fn ensure_api_token(&self, http: &reqwest::Client) -> Result<Secret<String>> {
    const SKEW_SECS: i64 = 300;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    if let (Some(tok), Some(exp)) = self.snapshot() {
      if exp - SKEW_SECS > now {
        let span = tracing::Span::current();
        span.record("refreshed", false);
        span.record("fp", tracing::field::display(tok.fingerprint()));
        return Ok(tok);
      }
    }
    let _g = self.refresh_lock.lock().await;
    if let (Some(tok), Some(exp)) = self.snapshot() {
      if exp - SKEW_SECS > now {
        let span = tracing::Span::current();
        span.record("refreshed", false);
        span.record("fp", tracing::field::display(tok.fingerprint()));
        return Ok(tok);
      }
    }
    debug!("api token expired or missing; refreshing");
    let resp = token::exchange(http, self.refresh_token.expose(), &self.headers).await?;
    let token = Secret::new(resp.token);
    {
      let mut g = self.cache.write();
      g.token = Some(token.clone());
      g.expires_at = Some(resp.expires_at);
    }
    let span = tracing::Span::current();
    span.record("refreshed", true);
    span.record("fp", tracing::field::display(token.fingerprint()));
    Ok(token)
  }

  fn invalidate_api_token(&self) {
    debug!(account = %self.id, "invalidating cached copilot api token");
    let mut g = self.cache.write();
    g.token = None;
    g.expires_at = None;
  }

  /// Resolve the X-Initiator value to send.
  /// Precedence: inbound `X-Initiator` header > config mode > auto-classify.
  fn resolve_initiator(&self, body: &Value, inbound: &HeaderMap, fallback: &str) -> String {
    if let Some(v) = inbound.get("x-initiator") {
      let v = v.as_str().trim().to_ascii_lowercase();
      if v == "user" || v == "agent" {
        return v;
      }
    }
    match self.headers.initiator_mode {
      InitiatorMode::AlwaysUser => "user".into(),
      InitiatorMode::AlwaysAgent => "agent".into(),
      InitiatorMode::Auto => {
        // If caller already classified, trust it.
        if fallback == "user" || fallback == "agent" {
          return fallback.into();
        }
        crate::util::initiator::classify_initiator(body).into()
      }
    }
  }
}

impl InputTransformer for CopilotProvider {
  fn transform_input(&self, _endpoint: Endpoint, body: Value) -> Result<Value> {
    Ok(body)
  }
}

#[async_trait]
impl Provider for CopilotProvider {
  fn id(&self) -> &str {
    &self.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  fn input_transformer(&self) -> Option<&dyn InputTransformer> {
    Some(self)
  }

  fn endpoint_rules(&self) -> Option<&'static [EndpointRule]> {
    crate::DESCRIPTOR.model_endpoint_rules
  }

  fn patch_headers(&self, headers: &mut HeaderMap, ctx: &HeaderPatchCtx<'_>) -> Result<()> {
    let token = ctx.bearer_token.ok_or_else(|| error::Error::Profiles {
      message: "missing copilot bearer token for header patch".to_string(),
    })?;
    headers.insert(&AUTHORIZATION, HeaderValue::from_string(format!("Bearer {token}")));
    headers.insert(
      &ACCEPT,
      HeaderValue::from_static(if ctx.stream {
        "text/event-stream"
      } else {
        "application/json"
      }),
    );
    headers.insert(&CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(
      HeaderName::new("x-initiator"),
      HeaderValue::from_string(ctx.initiator.to_string()),
    );
    if let Some(encoding) = ctx.content_encoding {
      headers.insert(&CONTENT_ENCODING, HeaderValue::from_string(encoding.to_string()));
    }
    Ok(())
  }

  async fn list_models(&self, http: &reqwest::Client) -> Result<Value> {
    let token = self.ensure_api_token(http).await?;
    models::list(http, token.expose(), &self.headers).await
  }

  async fn chat(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    self.upstream_post(ctx, "/chat/completions", "chat").await
  }

  async fn responses(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    self.upstream_post(ctx, "/responses", "responses").await
  }

  async fn messages(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    self.upstream_post(ctx, "/v1/messages", "messages").await
  }

  fn on_unauthorized(&self) {
    self.invalidate_api_token();
  }

  fn needs_refresh(&self, cfg: &AccountConfig) -> bool {
    const SKEW_SECS: i64 = 300;
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    cfg
      .access_token_expires_at
      .map(|exp| exp - SKEW_SECS <= now)
      .unwrap_or(true)
      || cfg.access_token.is_none()
  }

  async fn refresh(&self, cfg: &AccountConfig, http: &reqwest::Client) -> Result<AccountConfig> {
    let token = self.ensure_api_token(http).await?;
    let (_, expires_at) = self.snapshot();
    let mut next = cfg.clone();
    next.access_token = Some(token);
    next.access_token_expires_at = expires_at;
    next.last_refresh = Some(time::OffsetDateTime::now_utc().unix_timestamp());
    Ok(next)
  }
}

fn headers_from_settings(a: &AccountConfig) -> Result<CopilotHeaders> {
  let value = serde_json::to_value(&a.settings).map_err(|source| error::Error::Json {
    what: "copilot account settings",
    body: format!("{:?}", a.settings),
    source,
  })?;
  let mut headers = CopilotHeaders::from_value(&value)?;
  for (name, value) in &a.headers {
    headers.extra_headers.insert(name.clone(), value.clone());
  }
  Ok(headers)
}

impl CopilotProvider {
  /// Shared upstream POST path used by every endpoint surface. The
  /// per-surface methods only differ in `path` and the wrapping error
  /// context — auth, header construction, client identity, and initiator handling
  /// are identical because Copilot proxies all three on the same host with
  /// the same auth scheme.
  #[instrument(
    name = "copilot_upstream",
    skip_all,
    fields(
      account = %self.id,
      what,
      path,
      stream = ctx.stream,
      initiator = tracing::field::Empty,
    ),
  )]
  async fn upstream_post(&self, ctx: RequestCtx<'_>, path: &str, what: &'static str) -> Result<reqwest::Response> {
    let token = self.ensure_api_token(ctx.http).await?;
    let initiator = match ctx.endpoint {
      // For /v1/responses the inbound body uses `input`, not `messages`,
      // so the chat-style classifier would always fall through to
      // "user". Use the responses-aware variant instead.
      Endpoint::Responses => self.resolve_initiator_responses(ctx.body, ctx.inbound_headers, ctx.initiator),
      _ => self.resolve_initiator(ctx.body, ctx.inbound_headers, ctx.initiator),
    };
    tracing::Span::current().record("initiator", initiator.as_str());
    let mut h = ctx.client_headers.clone().unwrap_or_else(|| {
      headers::copilot_request_headers(token.expose(), &self.headers, ctx.stream, &initiator).unwrap_or_default()
    });
    self.patch_headers(
      &mut h,
      &HeaderPatchCtx {
        endpoint: ctx.endpoint,
        body: ctx.body,
        bearer_token: Some(token.expose()),
        content_encoding: ctx.content_encoding,
        stream: ctx.stream,
        initiator: &initiator,
        inbound_headers: ctx.inbound_headers,
        vars: &ctx.vars,
      },
    )?;
    let url = format!("{COPILOT_API}{path}");
    debug!(%url, "POST upstream");
    let body_bytes = ctx.request_body_bytes();
    let resp = crate::util::http::send(
      ctx.http,
      Method::POST,
      &url,
      h,
      Some(body_bytes),
      ctx.outbound.as_ref(),
      what,
    )
    .await?;
    debug!(status = %resp.status(), "upstream returned");
    Ok(resp)
  }

  /// Variant of [`Self::resolve_initiator`] for the Responses API, whose
  /// body is shaped `{ input: …, instructions: …, … }` rather than
  /// `{ messages: [...] }`.
  fn resolve_initiator_responses(&self, body: &Value, inbound: &HeaderMap, fallback: &str) -> String {
    if let Some(v) = inbound.get("x-initiator") {
      let v = v.as_str().trim().to_ascii_lowercase();
      if v == "user" || v == "agent" {
        return v;
      }
    }
    match self.headers.initiator_mode {
      InitiatorMode::AlwaysUser => "user".into(),
      InitiatorMode::AlwaysAgent => "agent".into(),
      InitiatorMode::Auto => {
        if fallback == "user" || fallback == "agent" {
          return fallback.into();
        }
        crate::util::initiator::classify_initiator_responses(body).into()
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::TemplateVars;
  use tokn_core::account::AccountTier;

  fn acct(refresh: Option<&str>) -> AccountConfig {
    AccountConfig {
      id: "test".into(),
      provider: ID_GITHUB_COPILOT.into(),
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
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: refresh.map(|s| Secret::new(s.into())),
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  fn provider() -> CopilotProvider {
    CopilotProvider::from_account(Arc::new(acct(Some("gh-test-fixture")))).unwrap()
  }

  fn patch_ctx(
    endpoint: Endpoint,
    stream: bool,
    initiator: &'static str,
    content_encoding: Option<&'static str>,
  ) -> HeaderPatchCtx<'static> {
    HeaderPatchCtx {
      endpoint,
      body: Box::leak(Box::new(Value::Null)),
      bearer_token: Some("api-tok-fixture"),
      content_encoding,
      stream,
      initiator,
      inbound_headers: Box::leak(Box::new(HeaderMap::new())),
      vars: Box::leak(Box::new(TemplateVars::default())),
    }
  }

  #[test]
  fn copilot_patch_headers_chat_streaming_user_initiator() {
    let p = provider();
    let mut h = HeaderMap::new();
    p.patch_headers(&mut h, &patch_ctx(Endpoint::ChatCompletions, true, "user", None))
      .unwrap();
    assert_eq!(h.get("authorization").unwrap().as_str(), "Bearer api-tok-fixture");
    assert_eq!(h.get("accept").unwrap().as_str(), "text/event-stream");
    assert_eq!(h.get("content-type").unwrap().as_str(), "application/json");
    assert_eq!(h.get("x-initiator").unwrap().as_str(), "user");
    assert!(h.get("content-encoding").is_none());
    let names: Vec<_> = h.iter().map(|(n, _)| n.as_str().to_string()).collect();
    assert_eq!(names.len(), 4, "unexpected extra headers: {names:?}");
  }

  #[test]
  fn copilot_patch_headers_chat_non_streaming_agent_initiator() {
    let p = provider();
    let mut h = HeaderMap::new();
    p.patch_headers(&mut h, &patch_ctx(Endpoint::ChatCompletions, false, "agent", None))
      .unwrap();
    assert_eq!(h.get("accept").unwrap().as_str(), "application/json");
    assert_eq!(h.get("x-initiator").unwrap().as_str(), "agent");
  }

  #[test]
  fn copilot_patch_headers_responses_streaming() {
    let p = provider();
    let mut h = HeaderMap::new();
    p.patch_headers(&mut h, &patch_ctx(Endpoint::Responses, true, "user", None))
      .unwrap();
    // patch_headers shape does not vary by endpoint.
    assert_eq!(h.get("accept").unwrap().as_str(), "text/event-stream");
    assert_eq!(h.get("x-initiator").unwrap().as_str(), "user");
  }

  #[test]
  fn copilot_patch_headers_messages_streaming() {
    let p = provider();
    let mut h = HeaderMap::new();
    p.patch_headers(&mut h, &patch_ctx(Endpoint::Messages, true, "user", None))
      .unwrap();
    assert_eq!(h.get("accept").unwrap().as_str(), "text/event-stream");
    assert_eq!(h.get("x-initiator").unwrap().as_str(), "user");
  }

  #[test]
  fn copilot_patch_headers_round_trips_content_encoding() {
    let p = provider();
    let mut h = HeaderMap::new();
    p.patch_headers(
      &mut h,
      &patch_ctx(Endpoint::ChatCompletions, false, "user", Some("gzip")),
    )
    .unwrap();
    assert_eq!(h.get("content-encoding").unwrap().as_str(), "gzip");
  }

  #[test]
  fn copilot_patch_headers_requires_bearer_token() {
    let p = provider();
    let mut h = HeaderMap::new();
    let ctx = HeaderPatchCtx {
      endpoint: Endpoint::ChatCompletions,
      body: &Value::Null,
      bearer_token: None,
      content_encoding: None,
      stream: false,
      initiator: "user",
      inbound_headers: &HeaderMap::new(),
      vars: &TemplateVars::default(),
    };
    let err = p.patch_headers(&mut h, &ctx).unwrap_err();
    assert!(err.to_string().contains("copilot bearer token"), "{err}");
  }
}
