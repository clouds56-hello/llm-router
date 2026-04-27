//! Provider abstraction.
//!
//! A provider knows how to authenticate, list models, and execute a chat
//! completion request. The gateway pool stores providers behind a trait object
//! so adding new upstreams (Anthropic, Gemini, …) is purely additive.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::sync::Arc;

pub mod github_copilot;
pub mod profiles;

pub const ID_GITHUB_COPILOT: &str = "github-copilot";

/// Per-request context handed to a provider.
pub struct ChatCtx<'a> {
    pub http: &'a reqwest::Client,
    pub body: &'a Value,
    pub stream: bool,
    /// "user" or "agent" — pre-classified by the server. Providers that don't
    /// care can ignore it.
    pub initiator: &'a str,
    /// Inbound headers from the downstream client. Providers may forward
    /// selected ones (e.g. an explicit `X-Initiator`).
    pub inbound_headers: &'a HeaderMap,
    /// Persona override from the inbound `X-Behave-As` request header, if any.
    /// Takes precedence over config-file `behave_as` settings.
    pub behave_as: Option<&'a str>,
}

#[async_trait]
pub trait Provider: Send + Sync {
    /// Stable id, e.g. "github-copilot".
    #[allow(dead_code)]
    fn id(&self) -> &str;

    /// Model routing hint. For providers that don't know their model list
    /// upfront, return `true` to accept everything.
    fn supports_model(&self, _model: &str) -> bool { true }

    /// `GET /models`. Result must be a JSON object suitable for inclusion in
    /// an OpenAI `/v1/models` response (an `{ "object":"list", "data":[...] }`
    /// object — the server unions `data` arrays across providers).
    async fn list_models(&self, http: &reqwest::Client) -> Result<Value>;

    /// Execute a chat completion. Returns the raw upstream `reqwest::Response`
    /// so the caller can stream or buffer the body verbatim.
    async fn chat(&self, ctx: ChatCtx<'_>) -> Result<reqwest::Response>;

    /// Called by the pool when an upstream 401 occurs, so the provider may
    /// invalidate any cached short-lived token. Default: no-op.
    fn on_unauthorized(&self) {}
}

/// Build a provider from an account config row.
pub fn build_for_account(
    a: &crate::config::Account,
    global_headers: &crate::config::CopilotHeaders,
) -> Result<Arc<dyn Provider>> {
    match a.provider.as_str() {
        ID_GITHUB_COPILOT => {
            let p = github_copilot::CopilotProvider::from_account(a, global_headers)?;
            Ok(Arc::new(p))
        }
        other => Err(anyhow!(
            "unknown provider '{other}' for account '{}'. Known: {ID_GITHUB_COPILOT}",
            a.id
        )),
    }
}
