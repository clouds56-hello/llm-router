use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use reqwest::header::{CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ConfigManager, ConnectAccountInput, UpdateAccountInput};
use crate::db::{AccountInformationRecord, RequestStore};

const CODEX_PROVIDER_NAME: &str = "codex";
const ISSUER: &str = "https://auth.openai.com";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const APP_USER_AGENT: &str = "llm-router";
const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexAuthState {
  pub logged_in: bool,
  pub account_id: Option<String>,
  pub access_token_preview: Option<String>,
  pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeviceAuthStartRequest {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthStartResponse {
  pub session_id: String,
  pub verification_uri: String,
  pub user_code: String,
  pub expires_in: u64,
  pub interval: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthCompleteRequest {
  pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthCompleteResponse {
  pub status: String,
  pub auth_state: Option<CodexAuthState>,
}

#[derive(Debug, Clone)]
struct PendingDeviceFlow {
  device_auth_id: String,
  user_code: String,
}

#[derive(Debug, Deserialize)]
struct DeviceStartResponse {
  device_auth_id: String,
  user_code: String,
  interval: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DeviceTokenReadyResponse {
  authorization_code: String,
  code_verifier: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
  access_token: Option<String>,
  refresh_token: Option<String>,
  id_token: Option<String>,
  expires_in: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexUsageResponse {
  plan_type: Option<String>,
  rate_limit: Option<CodexRateLimitInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexRateLimitInfo {
  primary_window: Option<CodexWindowInfo>,
  secondary_window: Option<CodexWindowInfo>,
}

#[derive(Debug, Clone, Deserialize)]
struct CodexWindowInfo {
  used_percent: Option<i64>,
  limit_window_seconds: Option<i64>,
  reset_after_seconds: Option<i64>,
  reset_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
struct CodexQuotaEntry {
  name: String,
  total: Option<f64>,
  percent: Option<f64>,
  remaining: Option<f64>,
  expires: Option<String>,
}

#[derive(Debug, Clone)]
struct CodexQuotaSnapshot {
  plan_type: Option<String>,
  quota_json: Option<String>,
  reset_date: Option<String>,
  user_id: Option<String>,
  email: Option<String>,
  name: Option<String>,
  metadata: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct CodexAuthManager {
  state_file: PathBuf,
  sessions: Arc<RwLock<HashMap<String, PendingDeviceFlow>>>,
  config: ConfigManager,
  requests: RequestStore,
  client: reqwest::Client,
}

impl CodexAuthManager {
  pub fn new(config_dir: PathBuf, config: ConfigManager, requests: RequestStore) -> Self {
    Self {
      state_file: config_dir.join("codex_auth_state.json"),
      sessions: Arc::new(RwLock::new(HashMap::new())),
      config,
      requests,
      client: reqwest::Client::new(),
    }
  }

  pub async fn start_device_authorization(&self, _request: DeviceAuthStartRequest) -> Result<DeviceAuthStartResponse> {
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      issuer = ISSUER,
      "starting Codex OAuth device authorization"
    );
    let response = self
      .client
      .post(format!("{ISSUER}/api/accounts/deviceauth/usercode"))
      .header(CONTENT_TYPE, "application/json")
      .header(USER_AGENT, APP_USER_AGENT)
      .json(&serde_json::json!({ "client_id": CLIENT_ID }))
      .send()
      .await
      .context("failed to initiate Codex device authorization")?;

    if !response.status().is_success() {
      tracing::warn!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        status = %response.status(),
        "Codex device authorization start failed"
      );
      anyhow::bail!("codex device authorization failed with {}", response.status());
    }

    let body: DeviceStartResponse = response
      .json()
      .await
      .context("failed to parse codex device authorization response")?;

    let session_id = uuid::Uuid::new_v4().to_string();
    self.sessions.write().insert(
      session_id.clone(),
      PendingDeviceFlow {
        device_auth_id: body.device_auth_id,
        user_code: body.user_code.clone(),
      },
    );
    let interval = body
      .interval
      .as_deref()
      .and_then(|v| v.parse::<u64>().ok())
      .unwrap_or(5);
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      session_id = %session_id,
      interval = interval,
      "Codex OAuth device authorization started"
    );
    Ok(DeviceAuthStartResponse {
      session_id,
      verification_uri: format!("{ISSUER}/codex/device"),
      user_code: body.user_code,
      expires_in: 900,
      interval,
    })
  }

  pub async fn complete_device_authorization(
    &self,
    request: DeviceAuthCompleteRequest,
  ) -> Result<DeviceAuthCompleteResponse> {
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      session_id = %request.session_id,
      "completing Codex OAuth device authorization"
    );
    let pending = self
      .sessions
      .read()
      .get(&request.session_id)
      .cloned()
      .ok_or_else(|| anyhow::anyhow!("unknown session_id"))?;

    let code_response = self
      .client
      .post(format!("{ISSUER}/api/accounts/deviceauth/token"))
      .header(CONTENT_TYPE, "application/json")
      .header(USER_AGENT, APP_USER_AGENT)
      .json(&serde_json::json!({
        "device_auth_id": pending.device_auth_id,
        "user_code": pending.user_code
      }))
      .send()
      .await
      .context("failed to request codex device token")?;

    if code_response.status().as_u16() == 403 || code_response.status().as_u16() == 404 {
      tracing::info!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        session_id = %request.session_id,
        status = %code_response.status(),
        "Codex OAuth device authorization still pending"
      );
      return Ok(DeviceAuthCompleteResponse {
        status: "authorization_pending".to_string(),
        auth_state: None,
      });
    }
    if !code_response.status().is_success() {
      tracing::warn!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        session_id = %request.session_id,
        status = %code_response.status(),
        "Codex device token polling failed"
      );
      anyhow::bail!(
        "codex device token endpoint failed with status {}",
        code_response.status()
      );
    }

    let code_payload: DeviceTokenReadyResponse = code_response
      .json()
      .await
      .context("failed to parse codex device token response")?;
    let token_response = self
      .client
      .post(format!("{ISSUER}/oauth/token"))
      .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
      .form(&[
        ("grant_type", "authorization_code"),
        ("code", code_payload.authorization_code.as_str()),
        ("redirect_uri", "https://auth.openai.com/deviceauth/callback"),
        ("client_id", CLIENT_ID),
        ("code_verifier", code_payload.code_verifier.as_str()),
      ])
      .send()
      .await
      .context("failed to exchange codex auth code for tokens")?;

    if !token_response.status().is_success() {
      tracing::warn!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        session_id = %request.session_id,
        status = %token_response.status(),
        "Codex token exchange failed"
      );
      anyhow::bail!("codex token exchange failed with status {}", token_response.status());
    }

    let tokens: TokenResponse = token_response
      .json()
      .await
      .context("failed to parse codex token response")?;
    let access_token = tokens
      .access_token
      .ok_or_else(|| anyhow::anyhow!("missing access_token from codex token response"))?;
    let refresh_token = tokens
      .refresh_token
      .ok_or_else(|| anyhow::anyhow!("missing refresh_token from codex token response"))?;
    let account_id = extract_account_id(tokens.id_token.as_deref().or(Some(access_token.as_str())));
    let resolved_account_id = self
      .upsert_oauth_account(account_id, access_token.clone(), refresh_token, tokens.expires_in)
      .await?;
    if let Err(err) = self
      .upsert_account_information(
        Utc::now(),
        &resolved_account_id,
        &access_token,
        extract_account_id(Some(access_token.as_str())),
      )
      .await
    {
      tracing::warn!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        account_id = %resolved_account_id,
        error = %err,
        "failed to upsert Codex account information after login"
      );
      if let Err(fallback_err) = self
        .requests
        .touch_account_information_connected(CODEX_PROVIDER_NAME, &resolved_account_id)
      {
        tracing::warn!(
          target: "auth",
          provider = CODEX_PROVIDER_NAME,
          account_id = %resolved_account_id,
          error = %fallback_err,
          "failed to touch Codex account information after login"
        );
      }
    }
    self.sessions.write().remove(&request.session_id);
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      session_id = %request.session_id,
      account_id = %resolved_account_id,
      "Codex OAuth login completed"
    );

    let state = CodexAuthState {
      logged_in: true,
      account_id: Some(resolved_account_id),
      access_token_preview: Some(token_preview(&access_token)),
      updated_at: Utc::now(),
    };
    self.write_state(&state)?;
    Ok(DeviceAuthCompleteResponse {
      status: "ok".to_string(),
      auth_state: Some(state),
    })
  }

  pub fn status(&self) -> Result<Option<CodexAuthState>> {
    let Some(state) = self.read_state()? else {
      return Ok(None);
    };
    let token = self
      .config
      .get()
      .credentials
      .resolve_runtime_credential(CODEX_PROVIDER_NAME)?
      .and_then(|c| c.api_key);
    if token.is_none() {
      return Ok(None);
    }
    Ok(Some(state))
  }

  pub async fn refresh_api_key(&self, account_id: Option<String>) -> Result<CodexAuthState> {
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      requested_account_id = ?account_id,
      "refreshing Codex API key"
    );
    let loaded = self.config.get();
    let resolved = if let Some(ref account) = account_id {
      loaded
        .credentials
        .resolve_runtime_credential_for_account_with_account(CODEX_PROVIDER_NAME, account)?
    } else {
      loaded
        .credentials
        .resolve_runtime_credential_with_account(CODEX_PROVIDER_NAME)?
    };
    let (resolved_account_id, credential) = resolved.ok_or_else(|| {
      anyhow::anyhow!(
        "no credential account configured for provider '{}'",
        CODEX_PROVIDER_NAME
      )
    })?;
    let account_meta = loaded
      .credentials
      .providers
      .get(CODEX_PROVIDER_NAME)
      .and_then(|cfg| cfg.accounts.iter().find(|a| a.id == resolved_account_id))
      .map(|a| a.meta.clone())
      .unwrap_or_default();
    let expires_at = account_meta
      .get("oauth_expires_at")
      .or_else(|| credential.extra.get("oauth_expires_at"))
      .and_then(|v| v.parse::<i64>().ok())
      .unwrap_or(0);
    let now_ms = Utc::now().timestamp_millis();
    if expires_at > now_ms + 30_000 {
      if let Some(access_token) = credential.extra.get("oauth_access_token").cloned() {
        let hint = credential.extra.get("chatgpt_account_id").cloned();
        if let Err(err) = self
          .upsert_account_information(Utc::now(), &resolved_account_id, &access_token, hint)
          .await
        {
          tracing::warn!(
            target: "auth",
            provider = CODEX_PROVIDER_NAME,
            account_id = %resolved_account_id,
            error = %err,
            "failed to refresh Codex quota info while token is still valid"
          );
        }
      }
      tracing::info!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        account_id = %resolved_account_id,
        "Codex API key still valid, refresh skipped"
      );
      return Ok(CodexAuthState {
        logged_in: true,
        account_id: Some(resolved_account_id),
        access_token_preview: credential.api_key.as_deref().map(token_preview),
        updated_at: Utc::now(),
      });
    }
    let refresh_token = credential
      .extra
      .get("oauth_refresh_token")
      .cloned()
      .ok_or_else(|| anyhow::anyhow!("oauth_refresh_token not found for codex account"))?;
    let response = self
      .client
      .post(format!("{ISSUER}/oauth/token"))
      .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
      .form(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.as_str()),
        ("client_id", CLIENT_ID),
      ])
      .send()
      .await
      .context("failed to refresh codex oauth access token")?;
    if !response.status().is_success() {
      tracing::warn!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        account_id = %resolved_account_id,
        status = %response.status(),
        "Codex token refresh failed"
      );
      anyhow::bail!("codex token refresh failed with status {}", response.status());
    }
    let tokens: TokenResponse = response
      .json()
      .await
      .context("failed to parse codex refresh token response")?;
    let access_token = tokens
      .access_token
      .ok_or_else(|| anyhow::anyhow!("missing access_token from codex refresh response"))?;
    let next_refresh = tokens.refresh_token.unwrap_or_else(|| refresh_token.clone());
    let account_from_claims = extract_account_id(tokens.id_token.as_deref().or(Some(access_token.as_str())));
    let next_account_id = account_from_claims.unwrap_or(resolved_account_id);
    let persisted_account_id = self
      .upsert_oauth_account(
        Some(next_account_id.clone()),
        access_token.clone(),
        next_refresh,
        tokens.expires_in,
      )
      .await?;
    if let Err(err) = self
      .upsert_account_information(
        Utc::now(),
        &persisted_account_id,
        &access_token,
        extract_account_id(Some(access_token.as_str())),
      )
      .await
    {
      tracing::warn!(
        target: "auth",
        provider = CODEX_PROVIDER_NAME,
        account_id = %persisted_account_id,
        error = %err,
        "failed to upsert Codex account information after refresh"
      );
      if let Err(fallback_err) = self
        .requests
        .touch_account_information_connected(CODEX_PROVIDER_NAME, &persisted_account_id)
      {
        tracing::warn!(
          target: "auth",
          provider = CODEX_PROVIDER_NAME,
          account_id = %persisted_account_id,
          error = %fallback_err,
          "failed to touch Codex account information after refresh"
        );
      }
    }
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      account_id = %persisted_account_id,
      "Codex API key refreshed"
    );
    let state = CodexAuthState {
      logged_in: true,
      account_id: Some(persisted_account_id),
      access_token_preview: Some(token_preview(&access_token)),
      updated_at: Utc::now(),
    };
    self.write_state(&state)?;
    Ok(state)
  }

  pub fn logout(&self) -> Result<()> {
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      "logging out Codex OAuth account"
    );
    if let Some(state) = self.read_state()? {
      if let Some(account_id) = state.account_id {
        self.config.disconnect_account(CODEX_PROVIDER_NAME, &account_id)?;
        self
          .requests
          .mark_account_information_disconnected(CODEX_PROVIDER_NAME, &account_id)?;
        tracing::info!(
          target: "auth",
          provider = CODEX_PROVIDER_NAME,
          account_id = %account_id,
          "Codex OAuth account disconnected"
        );
      }
    }
    if self.state_file.exists() {
      std::fs::remove_file(&self.state_file).context("failed to remove codex auth state")?;
    }
    Ok(())
  }

  async fn upsert_oauth_account(
    &self,
    account_id: Option<String>,
    access_token: String,
    refresh_token: String,
    expires_in: Option<u64>,
  ) -> Result<String> {
    let account_id = account_id.unwrap_or_else(|| "codex-default".to_string());
    let expires_at = Utc::now().timestamp_millis() + (expires_in.unwrap_or(3600) as i64) * 1000;
    let mut secrets = HashMap::from_iter(vec![
      ("api_key".to_string(), access_token.clone()),
      ("oauth_access_token".to_string(), access_token),
      ("oauth_refresh_token".to_string(), refresh_token),
    ]);
    if let Some(chatgpt_account_id) = extract_account_id(Some(secrets["oauth_access_token"].as_str())) {
      secrets.insert("chatgpt_account_id".to_string(), chatgpt_account_id);
    }
    let mut meta = HashMap::new();
    meta.insert("oauth".to_string(), "true".to_string());
    meta.insert("oauth_expires_at".to_string(), expires_at.to_string());
    meta.insert("oauth_issuer".to_string(), ISSUER.to_string());

    self.config.connect_account(ConnectAccountInput {
      provider: CODEX_PROVIDER_NAME.to_string(),
      account_id: Some(account_id.clone()),
      label: Some("Codex OAuth".to_string()),
      auth_type: Some("bearer".to_string()),
      secrets,
      meta,
      set_default: Some(true),
      enabled: Some(true),
    })?;
    self.config.update_account(UpdateAccountInput {
      provider: CODEX_PROVIDER_NAME.to_string(),
      account_id: account_id.clone(),
      label: Some(account_id),
      clear_secret_keys: Some(vec!["oauth_expires_at".to_string(), "oauth_issuer".to_string()]),
      ..Default::default()
    })?;
    Ok(
      self
        .config
        .get()
        .credentials
        .resolve_runtime_credential_with_account(CODEX_PROVIDER_NAME)?
        .map(|(id, _)| id)
        .unwrap_or_else(|| "codex-default".to_string()),
    )
  }

  pub async fn ensure_fresh_api_key(&self, account_id: Option<String>) -> Result<()> {
    let loaded = self.config.get();
    let resolved = if let Some(ref account) = account_id {
      loaded
        .credentials
        .resolve_runtime_credential_for_account_with_account(CODEX_PROVIDER_NAME, account)?
    } else {
      loaded
        .credentials
        .resolve_runtime_credential_with_account(CODEX_PROVIDER_NAME)?
    };
    let Some((_, credential)) = resolved else {
      return Ok(());
    };
    let account_meta = loaded
      .credentials
      .providers
      .get(CODEX_PROVIDER_NAME)
      .and_then(|cfg| {
        account_id
          .as_deref()
          .and_then(|id| cfg.accounts.iter().find(|a| a.id == id))
          .or_else(|| cfg.accounts.iter().find(|a| a.is_default))
      })
      .map(|a| a.meta.clone())
      .unwrap_or_default();
    let Some(raw_expires_at) = account_meta
      .get("oauth_expires_at")
      .or_else(|| credential.extra.get("oauth_expires_at"))
    else {
      return Ok(());
    };
    let expires_at = raw_expires_at.parse::<i64>().unwrap_or_default();
    if expires_at > Utc::now().timestamp_millis() + 30_000 {
      return Ok(());
    }
    tracing::info!(
      target: "auth",
      provider = CODEX_PROVIDER_NAME,
      requested_account_id = ?account_id,
      "Codex token expired or near expiry, triggering refresh"
    );
    self.refresh_api_key(account_id).await.map(|_| ())
  }

  fn write_state(&self, state: &CodexAuthState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    std::fs::write(&self.state_file, bytes).with_context(|| format!("failed writing {}", self.state_file.display()))
  }

  fn read_state(&self) -> Result<Option<CodexAuthState>> {
    if !self.state_file.exists() {
      return Ok(None);
    }
    let content = std::fs::read_to_string(&self.state_file)
      .with_context(|| format!("failed reading {}", self.state_file.display()))?;
    let state = serde_json::from_str(&content)?;
    Ok(Some(state))
  }

  async fn upsert_account_information(
    &self,
    observed_at: DateTime<Utc>,
    account_id: &str,
    access_token: &str,
    account_id_hint: Option<String>,
  ) -> Result<()> {
    let snapshot = self
      .fetch_quota_snapshot(access_token, account_id_hint.as_deref())
      .await?;
    let existing = self
      .requests
      .list_account_information(Some(CODEX_PROVIDER_NAME), Some(account_id))
      .ok()
      .and_then(|rows| rows.into_iter().next());
    let mut metadata = snapshot.metadata;
    metadata.insert("quota_source".to_string(), Value::String(CODEX_USAGE_URL.to_string()));
    self.requests.upsert_account_information(AccountInformationRecord {
      observed_at,
      provider: CODEX_PROVIDER_NAME.to_string(),
      account_id: account_id.to_string(),
      user_id: snapshot
        .user_id
        .or(account_id_hint)
        .or_else(|| existing.as_ref().and_then(|v| v.user_id.clone())),
      name: snapshot.name.or_else(|| existing.as_ref().and_then(|v| v.name.clone())),
      email: snapshot
        .email
        .or_else(|| existing.as_ref().and_then(|v| v.email.clone())),
      plan: snapshot.plan_type,
      quota: snapshot.quota_json,
      reset_date: snapshot.reset_date,
      status: "connected".to_string(),
      metadata,
    })?;
    Ok(())
  }

  async fn fetch_quota_snapshot(
    &self,
    access_token: &str,
    account_id_hint: Option<&str>,
  ) -> Result<CodexQuotaSnapshot> {
    let mut request = self
      .client
      .get(CODEX_USAGE_URL)
      .header(USER_AGENT, APP_USER_AGENT)
      .header("accept", "application/json")
      .bearer_auth(access_token);
    if let Some(account_id) = account_id_hint {
      if !account_id.trim().is_empty() {
        request = request.header("ChatGPT-Account-Id", account_id);
      }
    }
    let response = request.send().await.context("failed to fetch Codex usage")?;
    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      anyhow::bail!("codex usage endpoint returned {status}: {body}");
    }
    let raw_usage: Value = response.json().await.context("failed to parse Codex usage response")?;
    let usage: CodexUsageResponse =
      serde_json::from_value(raw_usage.clone()).context("failed to decode Codex usage schema")?;
    Ok(quota_snapshot_from_usage(usage, raw_usage))
  }
}

fn quota_snapshot_from_usage(usage: CodexUsageResponse, raw_usage: Value) -> CodexQuotaSnapshot {
  let primary = usage.rate_limit.as_ref().and_then(|v| v.primary_window.as_ref());
  let secondary = usage.rate_limit.as_ref().and_then(|v| v.secondary_window.as_ref());
  let user_id = extract_string_from_paths(
    &raw_usage,
    &[
      "user_id",
      "account_id",
      "chatgpt_account_id",
      "user.id",
      "account.id",
      "viewer.id",
    ],
  );
  let email = extract_string_from_paths(&raw_usage, &["email", "user.email", "account.email", "viewer.email"]);
  let name = extract_string_from_paths(&raw_usage, &["name", "user.name", "account.name", "viewer.name"]);

  let entries = [("primary_window", primary), ("secondary_window", secondary)]
    .into_iter()
    .filter_map(|(name, win)| {
      let window = win?;
      let used = window.used_percent.unwrap_or(0).clamp(0, 100) as f64;
      let remaining = (100.0 - used).clamp(0.0, 100.0);
      Some(CodexQuotaEntry {
        name: name.to_string(),
        total: Some(100.0),
        percent: Some(remaining),
        remaining: Some(remaining),
        expires: window
          .reset_at
          .or_else(|| {
            window
              .reset_after_seconds
              .map(|seconds| Utc::now().timestamp() + seconds.max(0))
          })
          .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0))
          .map(|dt| dt.to_rfc3339()),
      })
    })
    .collect::<Vec<_>>();

  let quota_json = if entries.is_empty() {
    None
  } else {
    serde_json::to_string(&entries).ok()
  };
  let reset_date = entries.iter().filter_map(|row| row.expires.clone()).min().or_else(|| {
    primary
      .and_then(|w| w.reset_at)
      .and_then(|ts| DateTime::<Utc>::from_timestamp(ts, 0))
      .map(|dt| dt.to_rfc3339())
  });

  let mut metadata = HashMap::new();
  metadata.insert("codex_usage_raw".to_string(), raw_usage);
  metadata.insert(
    "codex_usage_field_sources".to_string(),
    Value::Object(
      [
        ("plan_type".to_string(), Value::String("plan_type".to_string())),
        (
          "primary_window".to_string(),
          Value::String("rate_limit.primary_window".to_string()),
        ),
        (
          "secondary_window".to_string(),
          Value::String("rate_limit.secondary_window".to_string()),
        ),
        (
          "used_percent".to_string(),
          Value::String("rate_limit.*_window.used_percent".to_string()),
        ),
        (
          "limit_window_seconds".to_string(),
          Value::String("rate_limit.*_window.limit_window_seconds".to_string()),
        ),
        (
          "reset_after_seconds".to_string(),
          Value::String("rate_limit.*_window.reset_after_seconds".to_string()),
        ),
        (
          "reset_at".to_string(),
          Value::String("rate_limit.*_window.reset_at".to_string()),
        ),
        (
          "user_id".to_string(),
          Value::String("user_id|account_id|chatgpt_account_id|user.id|account.id|viewer.id".to_string()),
        ),
        (
          "email".to_string(),
          Value::String("email|user.email|account.email|viewer.email".to_string()),
        ),
        (
          "name".to_string(),
          Value::String("name|user.name|account.name|viewer.name".to_string()),
        ),
      ]
      .into_iter()
      .collect(),
    ),
  );
  metadata.insert(
    "quota_windows_present".to_string(),
    Value::Object(
      [
        ("primary".to_string(), Value::Bool(primary.is_some())),
        ("secondary".to_string(), Value::Bool(secondary.is_some())),
      ]
      .into_iter()
      .collect(),
    ),
  );
  if let Some(window) = primary {
    if let Some(used_percent) = window.used_percent {
      metadata.insert("primary_used_percent".to_string(), Value::Number(used_percent.into()));
    }
    if let Some(limit_window_seconds) = window.limit_window_seconds {
      metadata.insert(
        "primary_window_seconds".to_string(),
        Value::Number(limit_window_seconds.into()),
      );
    }
  }
  if let Some(window) = secondary {
    if let Some(used_percent) = window.used_percent {
      metadata.insert("secondary_used_percent".to_string(), Value::Number(used_percent.into()));
    }
    if let Some(limit_window_seconds) = window.limit_window_seconds {
      metadata.insert(
        "secondary_window_seconds".to_string(),
        Value::Number(limit_window_seconds.into()),
      );
    }
  }

  CodexQuotaSnapshot {
    plan_type: usage.plan_type,
    quota_json,
    reset_date,
    user_id,
    email,
    name,
    metadata,
  }
}

fn extract_string_from_paths(value: &Value, paths: &[&str]) -> Option<String> {
  for path in paths {
    if let Some(found) = extract_string_from_path(value, path) {
      return Some(found);
    }
  }
  None
}

fn extract_string_from_path(value: &Value, path: &str) -> Option<String> {
  let mut current = value;
  for seg in path.split('.') {
    let obj = current.as_object()?;
    current = obj.get(seg)?;
  }
  current.as_str().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn extract_account_id(token: Option<&str>) -> Option<String> {
  let token = token?;
  let payload = token.split('.').nth(1)?;
  let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload).ok()?;
  let claims: Value = serde_json::from_slice(&decoded).ok()?;
  claims
    .get("chatgpt_account_id")
    .and_then(Value::as_str)
    .map(ToString::to_string)
    .or_else(|| {
      claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
    })
    .or_else(|| {
      claims
        .get("organizations")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|org| org.get("id"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
    })
}

fn token_preview(token: &str) -> String {
  let len = token.chars().count();
  if len <= 8 {
    return "********".to_string();
  }
  let start: String = token.chars().take(4).collect();
  let end: String = token.chars().rev().take(4).collect::<String>().chars().rev().collect();
  format!("{start}...{end}")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn quota_snapshot_parses_windows() {
    let usage = CodexUsageResponse {
      plan_type: Some("plus".to_string()),
      rate_limit: Some(CodexRateLimitInfo {
        primary_window: Some(CodexWindowInfo {
          used_percent: Some(25),
          limit_window_seconds: Some(18_000),
          reset_after_seconds: Some(300),
          reset_at: None,
        }),
        secondary_window: Some(CodexWindowInfo {
          used_percent: Some(80),
          limit_window_seconds: Some(604_800),
          reset_after_seconds: None,
          reset_at: Some(Utc::now().timestamp() + 3600),
        }),
      }),
    };

    let raw_usage = serde_json::json!({
      "plan_type": "plus",
      "user": {
        "id": "user_123",
        "email": "dev@example.com",
        "name": "Dev User"
      },
      "rate_limit": {
        "primary_window": {
          "used_percent": 25,
          "limit_window_seconds": 18000,
          "reset_after_seconds": 300
        },
        "secondary_window": {
          "used_percent": 80,
          "limit_window_seconds": 604800,
          "reset_at": Utc::now().timestamp() + 3600
        }
      }
    });
    let snapshot = quota_snapshot_from_usage(usage, raw_usage);
    assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));
    let raw = snapshot.quota_json.expect("quota json");
    assert!(raw.contains("primary_window"));
    assert!(raw.contains("secondary_window"));
    assert!(snapshot.reset_date.is_some());
    assert_eq!(snapshot.user_id.as_deref(), Some("user_123"));
    assert_eq!(snapshot.email.as_deref(), Some("dev@example.com"));
    assert_eq!(snapshot.name.as_deref(), Some("Dev User"));
    assert!(snapshot.metadata.contains_key("codex_usage_raw"));
    assert_eq!(
      snapshot
        .metadata
        .get("quota_windows_present")
        .and_then(|v| v.get("primary"))
        .and_then(Value::as_bool),
      Some(true)
    );
  }
}
