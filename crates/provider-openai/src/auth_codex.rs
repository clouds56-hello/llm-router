//! ChatGPT Codex authentication.
//!
//! Mirrors the OAuth flow implemented by `opencode/src/plugin/codex.ts`:
//!
//! * **Device flow** (no browser available on the host) — `POST` to
//!   `https://auth.openai.com/api/accounts/deviceauth/usercode` to obtain a
//!   `user_code`, ask the user to visit `/codex/device`, then poll
//!   `/api/accounts/deviceauth/token` until the user authorises. The
//!   final response carries an `authorization_code` + `code_verifier`
//!   that we exchange at `/oauth/token` for `access_token` / `refresh_token`
//!   / `id_token`.
//!
//! * **Manual API key** — surfaced as the static-key onboarding path.
//!
//! Token refresh uses `grant_type=refresh_token` against the same
//! `/oauth/token` endpoint. The `id_token` is parsed (no signature
//! verification) so we can persist `chatgpt_account_id` for the
//! outbound `ChatGPT-Account-Id` header.
//!
//! Verification ([`ProviderAuth::verify_credential`]) probes the codex
//! responses endpoint and treats any non-`401`/`403` response as a
//! healthy credential — a true 200 would require a real model
//! invocation.

use async_trait::async_trait;
use tokn_auth::{
  AuthError, DeviceCodeHandle, DeviceFlowOutcome, ProviderAuth, QuotaSnapshot, RefreshOutcome, Result, VerifyOutcome,
};
use tokn_core::account::AccountConfig;
use serde::Deserialize;
use std::time::Duration;

use crate::jwt;
use crate::{
  CODEX_DEVICE_REDIRECT_URL, CODEX_DEVICE_TOKEN_URL, CODEX_DEVICE_USERCODE_URL, CODEX_DEVICE_VERIFY_URL,
  CODEX_OAUTH_TOKEN_URL,
};

pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const ISSUER: &str = "https://auth.openai.com";
const DEFAULT_EXPIRES_IN_SECS: u64 = 3600;
const REFRESH_SKEW_SECS: i64 = 60;

pub struct CodexAuth;

static CODEX: CodexAuth = CodexAuth;

pub fn codex_auth() -> &'static dyn ProviderAuth {
  &CODEX
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UserCodeResponse {
  device_auth_id: String,
  user_code: String,
  #[serde(default)]
  interval: Option<serde_json::Value>,
  #[serde(default)]
  expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DevicePollResponse {
  authorization_code: String,
  code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
  access_token: String,
  #[serde(default)]
  refresh_token: Option<String>,
  #[serde(default)]
  id_token: Option<String>,
  #[serde(default)]
  expires_in: Option<u64>,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn now_unix() -> i64 {
  time::OffsetDateTime::now_utc().unix_timestamp()
}

fn parse_interval(raw: &Option<serde_json::Value>) -> u64 {
  match raw {
    Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(5).max(1),
    Some(serde_json::Value::String(s)) => s.parse::<u64>().unwrap_or(5).max(1),
    _ => 5,
  }
}

async fn http_form(
  client: &reqwest::Client,
  url: &str,
  body: Vec<(&'static str, String)>,
) -> Result<reqwest::Response> {
  client
    .post(url)
    .header("content-type", "application/x-www-form-urlencoded")
    .header("user-agent", concat!("tokn-router/", env!("CARGO_PKG_VERSION")))
    .form(&body)
    .send()
    .await
    .map_err(|e| AuthError::Network(e.to_string()))
}

async fn http_json(client: &reqwest::Client, url: &str, body: serde_json::Value) -> Result<reqwest::Response> {
  client
    .post(url)
    .header("content-type", "application/json")
    .header("user-agent", concat!("tokn-router/", env!("CARGO_PKG_VERSION")))
    .json(&body)
    .send()
    .await
    .map_err(|e| AuthError::Network(e.to_string()))
}

async fn exchange_authorization_code(
  client: &reqwest::Client,
  code: &str,
  code_verifier: &str,
) -> Result<TokenResponse> {
  let resp = http_form(
    client,
    CODEX_OAUTH_TOKEN_URL,
    vec![
      ("grant_type", "authorization_code".into()),
      ("code", code.into()),
      ("redirect_uri", CODEX_DEVICE_REDIRECT_URL.into()),
      ("client_id", CLIENT_ID.into()),
      ("code_verifier", code_verifier.into()),
    ],
  )
  .await?;
  decode_token_response(resp, "authorization_code exchange").await
}

async fn refresh_with_token(client: &reqwest::Client, refresh_token: &str) -> Result<TokenResponse> {
  let resp = http_form(
    client,
    CODEX_OAUTH_TOKEN_URL,
    vec![
      ("grant_type", "refresh_token".into()),
      ("refresh_token", refresh_token.into()),
      ("client_id", CLIENT_ID.into()),
    ],
  )
  .await?;
  decode_token_response(resp, "refresh_token exchange").await
}

async fn decode_token_response(resp: reqwest::Response, what: &str) -> Result<TokenResponse> {
  let status = resp.status();
  let body = resp.text().await.unwrap_or_default();
  if !status.is_success() {
    return Err(AuthError::Upstream(format!(
      "{what} failed (HTTP {status}): {}",
      body.chars().take(300).collect::<String>()
    )));
  }
  serde_json::from_str(&body).map_err(|e| AuthError::Decode(format!("{what}: {e}")))
}

// ---------------------------------------------------------------------------
// ProviderAuth impl
// ---------------------------------------------------------------------------

#[async_trait]
impl ProviderAuth for CodexAuth {
  fn id(&self) -> &'static str {
    crate::ID_CODEX
  }

  fn supports_device_flow(&self) -> bool {
    true
  }

  /// Codex also accepts a manually-pasted API key (the third method in
  /// `opencode/src/plugin/codex.ts`).
  fn supports_static_key(&self) -> bool {
    true
  }

  fn default_account_id(&self) -> &'static str {
    crate::ID_CODEX
  }

  fn default_base_url(&self) -> Option<&'static str> {
    Some(crate::codex::CODEX_BASE_URL)
  }

  fn default_refresh_url(&self) -> Option<&'static str> {
    Some(CODEX_OAUTH_TOKEN_URL)
  }

  async fn request_device_code(&self, client: &reqwest::Client) -> Result<DeviceCodeHandle> {
    let resp = http_json(
      client,
      CODEX_DEVICE_USERCODE_URL,
      serde_json::json!({"client_id": CLIENT_ID}),
    )
    .await?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
      return Err(AuthError::Upstream(format!(
        "codex device-auth usercode failed (HTTP {status}): {}",
        body.chars().take(300).collect::<String>()
      )));
    }
    let parsed: UserCodeResponse =
      serde_json::from_str(&body).map_err(|e| AuthError::Decode(format!("codex usercode: {e}")))?;
    let interval = parse_interval(&parsed.interval);
    Ok(DeviceCodeHandle {
      device_code: parsed.device_auth_id,
      user_code: parsed.user_code,
      verification_uri: CODEX_DEVICE_VERIFY_URL.to_string(),
      expires_in: parsed.expires_in.unwrap_or(900),
      interval,
    })
  }

  async fn poll_device_code(&self, client: &reqwest::Client, handle: DeviceCodeHandle) -> Result<DeviceFlowOutcome> {
    let interval = Duration::from_secs(handle.interval.max(1) + 3 /* opencode SAFETY_MARGIN_MS / 1000 */);
    let deadline = std::time::Instant::now() + Duration::from_secs(handle.expires_in.max(60));
    loop {
      if std::time::Instant::now() >= deadline {
        return Err(AuthError::Other("codex device-auth poll timed out".into()));
      }
      let resp = http_json(
        client,
        CODEX_DEVICE_TOKEN_URL,
        serde_json::json!({
          "device_auth_id": handle.device_code,
          "user_code": handle.user_code,
        }),
      )
      .await?;
      let status = resp.status();
      if status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        let poll: DevicePollResponse =
          serde_json::from_str(&body).map_err(|e| AuthError::Decode(format!("codex device-token: {e}")))?;
        let tokens = exchange_authorization_code(client, &poll.authorization_code, &poll.code_verifier).await?;
        return Ok(make_outcome(tokens));
      }
      let code = status.as_u16();
      if code != 403 && code != 404 {
        let body = resp.text().await.unwrap_or_default();
        return Err(AuthError::Upstream(format!(
          "codex device-auth poll failed (HTTP {status}): {}",
          body.chars().take(300).collect::<String>()
        )));
      }
      tokio::time::sleep(interval).await;
    }
  }

  async fn refresh_credential(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<RefreshOutcome> {
    // API-key accounts have no refresh path.
    if account.refresh_token.is_none() {
      return Ok(RefreshOutcome::NotApplicable);
    }
    let refresh = account.refresh_token.as_ref().unwrap();
    let needs_refresh = match account.access_token_expires_at {
      Some(exp) => exp - REFRESH_SKEW_SECS <= now_unix(),
      None => true,
    };
    if !needs_refresh && account.access_token.is_some() {
      return Ok(RefreshOutcome::NotApplicable);
    }
    let tokens = refresh_with_token(client, refresh.expose()).await?;
    let outcome = make_outcome(tokens);
    Ok(RefreshOutcome::Refreshed {
      access_token: outcome.access_token,
      expires_at: outcome.access_token_expires_at,
      username: outcome.username,
      provider_account_id: outcome.provider_account_id,
    })
  }

  async fn verify_credential(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<VerifyOutcome> {
    let token = account
      .access_token
      .as_ref()
      .or(account.api_key.as_ref())
      .ok_or(AuthError::MissingCredential {
        account: account.id.clone(),
        field: "access_token",
      })?;
    let base = account
      .base_url
      .clone()
      .unwrap_or_else(|| crate::codex::CODEX_BASE_URL.to_string());
    let url = format!("{}/responses", base.trim_end_matches('/'));
    let mut req = client
      .post(url)
      .header("authorization", format!("Bearer {}", token.expose()))
      .header("content-type", "application/json")
      .header("accept", "application/json")
      .json(&serde_json::json!({}));
    if let Some(pid) = account.provider_account_id.as_deref().filter(|s| !s.is_empty()) {
      req = req.header("chatgpt-account-id", pid);
    }
    let resp = req.send().await.map_err(|e| AuthError::Network(e.to_string()))?;
    let status = resp.status();
    // 200/4xx-but-not-401/403 → credential is at least authenticated; the
    // upstream rejected the body we sent, which is expected because we
    // intentionally posted an empty payload.
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
      let body = resp.text().await.unwrap_or_default();
      return Err(AuthError::Upstream(format!(
        "codex rejected the credential (HTTP {status}): {}",
        body.chars().take(200).collect::<String>()
      )));
    }
    Ok(VerifyOutcome::default())
  }

  async fn probe_quota(&self, _client: &reqwest::Client, _account: &AccountConfig) -> Result<QuotaSnapshot> {
    Ok(QuotaSnapshot::default())
  }
}

fn make_outcome(tokens: TokenResponse) -> DeviceFlowOutcome {
  let TokenResponse {
    access_token,
    refresh_token,
    id_token,
    expires_in,
  } = tokens;
  let provider_account_id = id_token
    .as_deref()
    .and_then(jwt::parse_jwt_claims)
    .as_ref()
    .and_then(jwt::extract_account_id);
  let username = id_token
    .as_deref()
    .and_then(jwt::parse_jwt_claims)
    .and_then(|c| c.email);
  DeviceFlowOutcome {
    refresh_token: refresh_token.unwrap_or_default(),
    access_token,
    access_token_expires_at: now_unix() + expires_in.unwrap_or(DEFAULT_EXPIRES_IN_SECS) as i64,
    username,
    provider_account_id,
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use base64::Engine;

  fn jwt_with(payload: serde_json::Value) -> String {
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b"{\"alg\":\"none\"}");
    let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string().as_bytes());
    format!("{header}.{body}.")
  }

  #[test]
  fn make_outcome_extracts_account_id_from_id_token() {
    let tok = jwt_with(serde_json::json!({"chatgpt_account_id": "acc-9", "email": "u@x"}));
    let out = make_outcome(TokenResponse {
      access_token: "atk".into(),
      refresh_token: Some("rtk".into()),
      id_token: Some(tok),
      expires_in: Some(120),
    });
    assert_eq!(out.access_token, "atk");
    assert_eq!(out.refresh_token, "rtk");
    assert_eq!(out.provider_account_id.as_deref(), Some("acc-9"));
    assert_eq!(out.username.as_deref(), Some("u@x"));
    let drift = out.access_token_expires_at - now_unix();
    assert!((110..=130).contains(&drift), "expires_at drift {drift}");
  }

  #[test]
  fn make_outcome_without_id_token_yields_no_account_id() {
    let out = make_outcome(TokenResponse {
      access_token: "atk".into(),
      refresh_token: None,
      id_token: None,
      expires_in: None,
    });
    assert!(out.provider_account_id.is_none());
    assert!(out.username.is_none());
  }

  #[test]
  fn parse_interval_accepts_string_and_number() {
    assert_eq!(parse_interval(&Some(serde_json::json!(7))), 7);
    assert_eq!(parse_interval(&Some(serde_json::json!("4"))), 4);
    assert_eq!(parse_interval(&None), 5);
    assert_eq!(parse_interval(&Some(serde_json::json!(0))), 1);
  }
}
