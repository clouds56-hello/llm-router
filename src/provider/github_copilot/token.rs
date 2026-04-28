use crate::config::CopilotHeaders;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Subset of the `/copilot_internal/v2/token` response that we actually use.
///
/// Upstream returns many more fields (sku, chat_enabled, tracking_id, …);
/// `serde` silently drops anything we don't list.
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotTokenResp {
    pub token: String,
    pub expires_at: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub refresh_in: Option<i64>,

    /// Per-feature monthly premium-request remainder (numbers, not percents).
    /// Present on entitled paid plans; absent or `null` for org/free.
    #[serde(default)]
    pub limited_user_quotas: Option<LimitedUserQuotas>,

    /// ISO-8601 (e.g. `"2026-05-15"`) reset date for `limited_user_quotas`.
    /// Reset is monthly on the user's billing anchor day.
    #[serde(default)]
    pub limited_user_reset_date: Option<String>,
}

/// Remaining premium-request budget for the current monthly window.
///
/// Field semantics (observed): each value is a *remaining count* of premium
/// requests, **not** a percentage. We expose what upstream sends and let the
/// renderer decide how to display it.
#[derive(Debug, Clone, Deserialize)]
pub struct LimitedUserQuotas {
    #[serde(default)]
    pub chat: Option<u64>,
    #[serde(default)]
    pub completions: Option<u64>,
    #[serde(default, alias = "premium_interactions")]
    pub premium_interactions: Option<u64>,
}

/// Exchange a long-lived GitHub OAuth token for a short-lived Copilot API token.
pub async fn exchange(
    client: &reqwest::Client,
    github_token: &str,
    headers: &CopilotHeaders,
) -> Result<CopilotTokenResp> {
    let h = super::headers::token_exchange_headers(github_token, headers)?;
    let resp = client
        .get(super::TOKEN_EXCHANGE_URL)
        .headers(h)
        .send()
        .await
        .context("token exchange request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("token exchange returned {status}: {body}"));
    }
    let parsed: CopilotTokenResp = serde_json::from_str(&body)
        .with_context(|| format!("parse token exchange response: {body}"))?;
    Ok(parsed)
}
