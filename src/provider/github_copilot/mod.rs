//! GitHub Copilot provider.

pub mod headers;
pub mod models;
pub mod oauth;
pub mod token;
pub mod user;

use crate::config::{Account, CopilotHeaders, InitiatorMode};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use parking_lot::RwLock;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::OnceLock;
use tokio::sync::Mutex as AsyncMutex;

use super::{AuthKind, ChatCtx, Provider, ProviderInfo};

#[allow(dead_code)]
pub const GITHUB_API: &str = "https://api.github.com";
pub const COPILOT_API: &str = "https://api.githubcopilot.com";
pub const TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";

/// Cached short-lived API token state.
struct ApiToken {
    token: Option<String>,
    expires_at: Option<i64>,
}

pub struct CopilotProvider {
    #[allow(dead_code)]
    pub id: String,
    pub github_token: String,
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
        // Copilot exposes a dynamic upstream model catalogue via /models; we
        // intentionally leave the static overlay empty rather than baking a
        // soon-stale list.
        default_models: Vec::new(),
    })
}

impl CopilotProvider {
    pub fn from_account(a: &Account, global: &CopilotHeaders) -> Result<Self> {
        let gh = a
            .github_token
            .clone()
            .ok_or_else(|| anyhow!("account '{}' missing github_token", a.id))?;
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

    fn snapshot(&self) -> (Option<String>, Option<i64>) {
        let g = self.cache.read();
        (g.token.clone(), g.expires_at)
    }

    /// Ensure we have a non-expired Copilot API token; refresh if needed.
    pub async fn ensure_api_token(&self, http: &reqwest::Client) -> Result<String> {
        const SKEW_SECS: i64 = 300;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if let (Some(tok), Some(exp)) = self.snapshot() {
            if exp - SKEW_SECS > now {
                return Ok(tok);
            }
        }
        let _g = self.refresh_lock.lock().await;
        if let (Some(tok), Some(exp)) = self.snapshot() {
            if exp - SKEW_SECS > now {
                return Ok(tok);
            }
        }
        let resp = token::exchange(http, &self.github_token, &self.headers)
            .await
            .context("Copilot token exchange failed")?;
        {
            let mut g = self.cache.write();
            g.token = Some(resp.token.clone());
            g.expires_at = Some(resp.expires_at);
        }
        Ok(resp.token)
    }

    fn invalidate_api_token(&self) {
        let mut g = self.cache.write();
        g.token = None;
        g.expires_at = None;
    }

    /// Apply an inbound `X-Behave-As` persona override on top of the
    /// account-resolved headers. The user-explicit fields stored on
    /// `self.headers` continue to win — the inbound override only fills the
    /// fields that the user did not pin in config.
    fn headers_for_request(&self, inbound_persona: Option<&str>) -> CopilotHeaders {
        let Some(persona) = inbound_persona else { return self.headers.clone(); };
        if Some(persona) == self.headers.behave_as.as_deref() {
            return self.headers.clone();
        }
        let profiles = crate::provider::profiles::Profiles::global();
        let Some(resolved) = profiles.resolve(persona, super::ID_GITHUB_COPILOT) else {
            tracing::warn!(persona, "unknown inbound X-Behave-As persona; ignoring");
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
                other => { h.extra_headers.insert(other.to_string(), val.clone()); }
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
    fn id(&self) -> &str { &self.id }

    fn info(&self) -> &ProviderInfo { &self.info }

    async fn list_models(&self, http: &reqwest::Client) -> Result<Value> {
        let token = self.ensure_api_token(http).await?;
        models::list(http, &token, &self.headers).await
    }

    async fn chat(&self, ctx: ChatCtx<'_>) -> Result<reqwest::Response> {
        let token = self.ensure_api_token(ctx.http).await?;
        let initiator = self.resolve_initiator(ctx.body, ctx.inbound_headers, ctx.initiator);
        let headers = self.headers_for_request(ctx.behave_as);
        let h = headers::copilot_request_headers(&token, &headers, ctx.stream, &initiator)?;
        let url = format!("{COPILOT_API}/chat/completions");
        let resp = ctx
            .http
            .post(&url)
            .headers(h)
            .json(ctx.body)
            .send()
            .await
            .context("upstream chat request failed")?;
        Ok(resp)
    }

    fn on_unauthorized(&self) { self.invalidate_api_token(); }
}
