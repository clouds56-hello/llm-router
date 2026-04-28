//! GitHub OAuth Device Flow for the Copilot client_id.
//!
//! Reference: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#device-flow

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::time::Duration;

/// VS Code Copilot Chat client ID. Public, well-known.
pub const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

#[derive(Debug, Deserialize)]
pub struct DeviceCode {
  pub device_code: String,
  pub user_code: String,
  pub verification_uri: String,
  pub expires_in: u64,
  pub interval: u64,
}

#[derive(Debug, Deserialize)]
struct TokenPending {
  error: String,
  #[serde(default)]
  interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenOk {
  access_token: String,
  #[serde(default)]
  token_type: Option<String>,
  #[serde(default)]
  scope: Option<String>,
}

pub async fn request_device_code(client: &reqwest::Client) -> Result<DeviceCode> {
  let resp = client
    .post(DEVICE_CODE_URL)
    .header("accept", "application/json")
    .form(&[("client_id", COPILOT_CLIENT_ID), ("scope", "read:user")])
    .send()
    .await
    .context("device code request failed")?;
  let status = resp.status();
  let body = resp.text().await.unwrap_or_default();
  if !status.is_success() {
    return Err(anyhow!("device code request returned {status}: {body}"));
  }
  serde_json::from_str(&body).with_context(|| format!("parse device code: {body}"))
}

pub async fn poll_for_token(client: &reqwest::Client, dc: &DeviceCode) -> Result<String> {
  let mut interval = Duration::from_secs(dc.interval.max(1));
  let deadline = std::time::Instant::now() + Duration::from_secs(dc.expires_in);

  loop {
    if std::time::Instant::now() >= deadline {
      return Err(anyhow!("device code expired before authorization"));
    }
    tokio::time::sleep(interval).await;

    let resp = client
      .post(ACCESS_TOKEN_URL)
      .header("accept", "application/json")
      .form(&[
        ("client_id", COPILOT_CLIENT_ID),
        ("device_code", dc.device_code.as_str()),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
      ])
      .send()
      .await
      .context("access_token poll failed")?;
    let body = resp.text().await.unwrap_or_default();

    if let Ok(ok) = serde_json::from_str::<TokenOk>(&body) {
      if !ok.access_token.is_empty() {
        tracing::debug!(
            token_type = ?ok.token_type, scope = ?ok.scope, "received access token"
        );
        return Ok(ok.access_token);
      }
    }
    if let Ok(p) = serde_json::from_str::<TokenPending>(&body) {
      match p.error.as_str() {
        "authorization_pending" => {}
        "slow_down" => {
          interval = Duration::from_secs(p.interval.unwrap_or(interval.as_secs() + 5));
        }
        "expired_token" => return Err(anyhow!("device code expired")),
        "access_denied" => return Err(anyhow!("user denied authorization")),
        other => return Err(anyhow!("oauth error: {other}: {body}")),
      }
      continue;
    }
    return Err(anyhow!("unexpected token response: {body}"));
  }
}
