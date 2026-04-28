use crate::config::CopilotHeaders;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

/// Subset of the `/copilot_internal/v2/token` response that we actually use.
///
/// Upstream returns many more fields (sku, chat_enabled, tracking_id, …);
/// `serde` silently drops anything we don't list.
///
/// Note: this endpoint *does* expose `limited_user_quotas` /
/// `limited_user_reset_date`, but they are `null` for paid plans
/// (`plus_monthly_subscriber_quota`, business, …). For human-visible
/// remaining-quota figures use `super::user::fetch_user_info` which hits
/// `/copilot_internal/user` and returns per-feature `quota_snapshots`.
#[derive(Debug, Clone, Deserialize)]
pub struct CopilotTokenResp {
    pub token: String,
    pub expires_at: i64,
    #[serde(default)]
    #[allow(dead_code)]
    pub refresh_in: Option<i64>,
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
