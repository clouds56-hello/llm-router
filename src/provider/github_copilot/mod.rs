//! GitHub Copilot provider.

pub mod headers;
pub mod models;
pub mod oauth;
pub mod token;
pub mod user;

use crate::config::{Account, CopilotHeaders, InitiatorMode};
use crate::util::redact::{token_fingerprint, BehaveAs};
use crate::util::secret::Secret;
use async_trait::async_trait;
use parking_lot::RwLock;
use reqwest::header::HeaderMap;
use serde_json::Value;
use snafu::ResultExt;
use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, instrument, warn};

use super::{error, AuthKind, Endpoint, Provider, ProviderInfo, RequestCtx, Result};

#[allow(dead_code)]
pub const GITHUB_API: &str = "https://api.github.com";
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
pub const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Cached short-lived API token state.
struct ApiToken {
  token: Option<Secret<String>>,
  expires_at: Option<i64>,
}

pub struct CopilotProvider {
  #[allow(dead_code)]
  pub id: String,
  pub github_token: Secret<String>,
  pub headers: CopilotHeaders,
  refresh_lock: AsyncMutex<()>,
  cache: RwLock<ApiToken>,
  info: ProviderInfo,
}

fn copilot_info() -> &'static ProviderInfo {
  static CELL: OnceLock<ProviderInfo> = OnceLock::new();
  CELL.get_or_init(|| ProviderInfo {
    id: super::ID_GITHUB_COPILOT.to_string(),
    aliases: &[super::ID_GITHUB_COPILOT],
    display_name: "GitHub Copilot",
    upstream_url: COPILOT_API.to_string(),
    auth_kind: AuthKind::OAuthDeviceFlow,
    // Copilot's `/models` upstream is the source of truth for model
    // *identity*; the catalogue below provides metadata overlay for the
    // ids that models.dev tracks. Unknown ids still pass through
    // `/v1/models` — they just lack the `x_llm_router` enrichment block.
    default_models: crate::catalogue::default_models_for(super::ID_GITHUB_COPILOT),
  })
}

impl CopilotProvider {
  pub fn from_account(a: &Account, global: &CopilotHeaders) -> Result<Self> {
    let gh = a.github_token.clone().ok_or(error::Error::MissingCredential {
      account: a.id.clone(),
      what: "github_token",
    })?;
    Ok(Self {
      id: format!("github-copilot:{}", a.id),
      github_token: gh,
      headers: global.merged(a.copilot.as_ref()),
      refresh_lock: AsyncMutex::new(()),
      cache: RwLock::new(ApiToken {
        token: a.api_token.clone(),
        expires_at: a.api_token_expires_at,
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
        span.record("fp", tracing::field::display(token_fingerprint(tok.expose())));
        return Ok(tok);
      }
    }
    let _g = self.refresh_lock.lock().await;
    if let (Some(tok), Some(exp)) = self.snapshot() {
      if exp - SKEW_SECS > now {
        let span = tracing::Span::current();
        span.record("refreshed", false);
        span.record("fp", tracing::field::display(token_fingerprint(tok.expose())));
        return Ok(tok);
      }
    }
    debug!("api token expired or missing; refreshing");
    let resp = token::exchange(http, self.github_token.expose(), &self.headers).await?;
    let token = Secret::new(resp.token);
    {
      let mut g = self.cache.write();
      g.token = Some(token.clone());
      g.expires_at = Some(resp.expires_at);
    }
    let span = tracing::Span::current();
    span.record("refreshed", true);
    span.record("fp", tracing::field::display(token_fingerprint(token.expose())));
    Ok(token)
  }

  fn invalidate_api_token(&self) {
    debug!(account = %self.id, "invalidating cached copilot api token");
    let mut g = self.cache.write();
    g.token = None;
    g.expires_at = None;
  }

  /// Apply an inbound `X-Behave-As` persona override on top of the
  /// account-resolved headers. The user-explicit fields stored on
  /// `self.headers` continue to win — the inbound override only fills the
  /// fields that the user did not pin in config.
  fn headers_for_request(&self, inbound_persona: Option<&str>) -> CopilotHeaders {
    let Some(persona) = inbound_persona else {
      return self.headers.clone();
    };
    if Some(persona) == self.headers.behave_as.as_deref() {
      return self.headers.clone();
    }
    let profiles = crate::provider::profiles::Profiles::global();
    let Some(resolved) = profiles.resolve(persona, super::ID_GITHUB_COPILOT) else {
      warn!(persona, "unknown inbound X-Behave-As persona; ignoring");
      return self.headers.clone();
    };
    crate::provider::profiles::warn_if_unverified(persona, super::ID_GITHUB_COPILOT, &resolved);

    // Re-resolve from compile-time defaults so the inbound persona can
    // displace previously-active persona values; user-explicit fields are
    // not detectable at this point, so we trust the new persona wholesale
    // for the known wire-name fields.
    let mut h = CopilotHeaders {
      initiator_mode: self.headers.initiator_mode,
      behave_as: Some(persona.to_string()),
      extra_headers: self.headers.extra_headers.clone(),
      ..CopilotHeaders::default()
    };
    for (name, val) in &resolved.headers {
      match name.as_str() {
        "editor-version" => h.editor_version = val.clone(),
        "editor-plugin-version" => h.editor_plugin_version = val.clone(),
        "user-agent" => h.user_agent = val.clone(),
        "copilot-integration-id" => h.copilot_integration_id = val.clone(),
        "openai-intent" => h.openai_intent = val.clone(),
        other => {
          h.extra_headers.insert(other.to_string(), val.clone());
        }
      }
    }
    h
  }

  /// Resolve the X-Initiator value to send.
  /// Precedence: inbound `X-Initiator` header > config mode > auto-classify.
  fn resolve_initiator(&self, body: &Value, inbound: &HeaderMap, fallback: &str) -> String {
    if let Some(v) = inbound.get("x-initiator").and_then(|v| v.to_str().ok()) {
      let v = v.trim().to_ascii_lowercase();
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
        headers::classify_initiator(body).into()
      }
    }
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

  /// Capability matrix for Copilot's three upstream surfaces.
  ///
  /// We do best-effort pattern matching on the model id rather than a hard
  /// allowlist, because Copilot ships new models continuously and the
  /// upstream `/models` response does not yet annotate per-endpoint
  /// support. The patterns here mirror what the official Copilot CLI /
  /// VSCode plugin route.
  fn supports(&self, model: &str, endpoint: Endpoint) -> bool {
    match endpoint {
      // Every Copilot model speaks the OpenAI Chat Completions surface.
      Endpoint::ChatCompletions => true,
      // Anthropic Messages API: Claude family routes here natively.
      Endpoint::Messages => model.starts_with("claude-"),
      // OpenAI Responses API: o-series and gpt-5+ families.
      Endpoint::Responses => {
        let m = model;
        m.starts_with("gpt-5") || m.starts_with("o1") || m.starts_with("o3") || m.starts_with("o4")
      }
    }
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
}

impl CopilotProvider {
  /// Shared upstream POST path used by every endpoint surface. The
  /// per-surface methods only differ in `path` and the wrapping error
  /// context — auth, header construction, persona, and initiator handling
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
      behave_as = %BehaveAs(ctx.behave_as),
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
    let headers = self.headers_for_request(ctx.behave_as);
    let mut h = headers::copilot_request_headers(token.expose(), &headers, ctx.stream, &initiator)?;
    h.insert(
      reqwest::header::CONTENT_TYPE,
      reqwest::header::HeaderValue::from_static("application/json"),
    );
    let url = format!("{COPILOT_API}{path}");
    crate::server::record_upstream_url(&url);
    debug!(%url, "POST upstream");
    let body_bytes = bytes::Bytes::from(serde_json::to_vec(ctx.body).unwrap_or_default());
    ctx.capture_outbound("POST", &url, &h, body_bytes.clone());
    let resp = ctx
      .http
      .post(&url)
      .headers(h)
      .body(body_bytes)
      .send()
      .await
      .context(error::HttpSnafu { what })?;
    debug!(status = %resp.status(), "upstream returned");
    Ok(resp)
  }

  /// Variant of [`Self::resolve_initiator`] for the Responses API, whose
  /// body is shaped `{ input: …, instructions: …, … }` rather than
  /// `{ messages: [...] }`.
  fn resolve_initiator_responses(&self, body: &Value, inbound: &HeaderMap, fallback: &str) -> String {
    if let Some(v) = inbound.get("x-initiator").and_then(|v| v.to_str().ok()) {
      let v = v.trim().to_ascii_lowercase();
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
        headers::classify_initiator_responses(body).into()
      }
    }
  }
}
