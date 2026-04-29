use crate::config::CopilotHeaders;
use crate::provider::{error, Result};
use crate::util::redact::token_fingerprint;
use serde::Deserialize;
use snafu::ResultExt;
use tracing::{debug, instrument};

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
#[instrument(
  name = "copilot_token_exchange",
  skip_all,
  fields(
    github_token_fp = %token_fingerprint(github_token),
    api_token_fp = tracing::field::Empty,
    expires_at = tracing::field::Empty,
    status = tracing::field::Empty,
  ),
)]
pub async fn exchange(
  client: &reqwest::Client,
  github_token: &str,
  headers: &CopilotHeaders,
) -> Result<CopilotTokenResp> {
  let h = super::headers::token_exchange_headers(github_token, headers)?;
  debug!("posting token exchange");
  let resp = client
    .get(super::TOKEN_EXCHANGE_URL)
    .headers(h)
    .send()
    .await
    .context(error::HttpSnafu { what: "token exchange" })?;
  let status = resp.status();
  tracing::Span::current().record("status", status.as_u16());
  let body = resp.text().await.unwrap_or_default();
  if !status.is_success() {
    return error::HttpStatusSnafu {
      what: "token exchange",
      status,
      body,
    }
    .fail();
  }
  let parsed: CopilotTokenResp = serde_json::from_str(&body).context(error::JsonSnafu {
    what: "token exchange",
    body: body.clone(),
  })?;
  let span = tracing::Span::current();
  span.record(
    "api_token_fp",
    tracing::field::display(token_fingerprint(&parsed.token)),
  );
  span.record("expires_at", parsed.expires_at);
  debug!("token exchange ok");
  Ok(parsed)
}
