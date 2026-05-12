//! [`ProviderAuth`] impl for github-copilot.
//!
//! Wraps the existing `oauth`, `token`, and `user` modules in the
//! provider-agnostic contract defined by `llm-core::auth`. Holds no state;
//! exposed via [`provider_auth()`].

use crate::config::CopilotHeaders;
use async_trait::async_trait;
use llm_core::account::AccountConfig;
use llm_auth::{
  AuthError, DeviceFlowOutcome, ProviderAuth, QuotaSnapshot, RefreshOutcome, Result,
};

/// Singleton impl. Zero-sized; safe to hand out as `&'static`.
pub struct CopilotAuth;

/// Static accessor used by `llm-auth`'s dispatch table.
pub fn provider_auth() -> &'static dyn ProviderAuth {
  &CopilotAuth
}

#[async_trait]
impl ProviderAuth for CopilotAuth {
  fn id(&self) -> &'static str {
    crate::ID_GITHUB_COPILOT
  }

  fn supports_device_flow(&self) -> bool {
    true
  }

  async fn device_flow_login(&self, client: &reqwest::Client) -> Result<DeviceFlowOutcome> {
    // Step 1: device-code dance.
    let dc = crate::oauth::request_device_code(client)
      .await
      .map_err(|e| AuthError::Upstream(e.to_string()))?;
    // Side-effecting prompts (the user-facing "Open: …" message) are the
    // CLI's responsibility — `llm-auth` just orchestrates. We surface the
    // verification URI / user code via `tracing` so callers can hook in
    // before polling.
    tracing::info!(
      target: "llm_auth::login",
      verification_uri = %dc.verification_uri,
      user_code = %dc.user_code,
      expires_in = dc.expires_in,
      "device code issued"
    );
    let gh_token = crate::oauth::poll_for_token(client, &dc)
      .await
      .map_err(|e| AuthError::Upstream(e.to_string()))?;

    // Step 2: exchange long-lived OAuth token for short-lived access token.
    let headers = CopilotHeaders::default();
    let exchange = crate::token::exchange(client, &gh_token, &headers)
      .await
      .map_err(|e| AuthError::Upstream(e.to_string()))?;

    // Step 3: best-effort GitHub username lookup for id suggestion.
    let username = fetch_github_username(client, &gh_token).await.ok();

    Ok(DeviceFlowOutcome {
      refresh_token: gh_token,
      access_token: exchange.token,
      access_token_expires_at: exchange.expires_at,
      username,
    })
  }

  async fn refresh_credential(
    &self,
    client: &reqwest::Client,
    account: &AccountConfig,
  ) -> Result<RefreshOutcome> {
    let gh_token = account
      .refresh_token
      .as_ref()
      .ok_or(AuthError::MissingCredential {
        account: account.id.clone(),
        field: "refresh_token",
      })?;
    let headers = headers_from_settings(account)?;
    let resp = crate::token::exchange(client, gh_token.expose(), &headers)
      .await
      .map_err(|e| AuthError::Upstream(e.to_string()))?;
    Ok(RefreshOutcome::Refreshed {
      access_token: resp.token,
      expires_at: resp.expires_at,
    })
  }

  async fn verify_credential(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<()> {
    // A successful refresh proves the long-lived token still works.
    self.refresh_credential(client, account).await.map(|_| ())
  }

  async fn probe_quota(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<QuotaSnapshot> {
    let gh_token = account
      .refresh_token
      .as_ref()
      .ok_or(AuthError::MissingCredential {
        account: account.id.clone(),
        field: "refresh_token",
      })?;
    let headers = headers_from_settings(account)?;
    let info = crate::user::fetch(client, gh_token.expose(), &headers)
      .await
      .map_err(|e| AuthError::Upstream(e.to_string()))?;

    // Pick a sensible headline: prefer premium_interactions, fall back to
    // any quota with remaining < entitlement.
    let headline = info.quota_snapshots.iter().find_map(|(k, q)| {
      if q.unlimited {
        return None;
      }
      let r = q.remaining?;
      let e = q.entitlement.unwrap_or(0);
      Some(format!("{k}: {r}/{e}"))
    });

    Ok(QuotaSnapshot {
      plan: info.copilot_plan.clone(),
      headline,
      reset_date: info.quota_reset_date.clone(),
      // Forward the full snapshot blob for any UI that wants per-feature
      // detail. `serde_json::to_value` is infallible for our types.
      provider_extra: serde_json::to_value(&info).unwrap_or(serde_json::Value::Null),
    })
  }
}

/// Decode the `[settings]` table from an account into a `CopilotHeaders`
/// struct. Falls back to defaults when missing.
fn headers_from_settings(account: &AccountConfig) -> Result<CopilotHeaders> {
  let value = serde_json::to_value(toml::Value::Table(account.settings.clone()))
    .unwrap_or(serde_json::Value::Null);
  CopilotHeaders::from_value(&value).map_err(|e| AuthError::Decode(e.to_string()))
}

/// Best-effort `GET /user` to discover the upstream login. Used only to
/// suggest an account id during interactive login.
async fn fetch_github_username(client: &reqwest::Client, gh_token: &str) -> Result<String> {
  #[derive(serde::Deserialize)]
  struct Me {
    login: String,
  }
  let me: Me = client
    .get("https://api.github.com/user")
    .header("authorization", format!("token {gh_token}"))
    .header("accept", "application/json")
    .header("user-agent", "llm-router")
    .send()
    .await
    .map_err(|e| AuthError::Network(e.to_string()))?
    .error_for_status()
    .map_err(|e| AuthError::Upstream(e.to_string()))?
    .json()
    .await
    .map_err(|e| AuthError::Decode(e.to_string()))?;
  Ok(me.login)
}
