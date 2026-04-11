use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::{ConfigManager, ConnectAccountInput, UpdateAccountInput};
use crate::db::{AccountInformationRecord, RequestStore};

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_FLOW_SCOPE: &str = "read:user copilot";
const COPILOT_PROVIDER_NAME: &str = "github_copilot";
const GITHUB_USER_ENDPOINT: &str = "https://api.github.com/user";
const GITHUB_USER_EMAILS_ENDPOINT: &str = "https://api.github.com/user/emails";
const GITHUB_COPILOT_TOKEN_ENDPOINT: &str = "https://api.github.com/copilot_internal/v2/token";
const GITHUB_COPILOT_USER_INFO_ENDPOINT: &str = "https://api.github.com/copilot_internal/user";
const APP_USER_AGENT: &str = "llm-router";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum CopilotDeployment {
  GitHubCom,
  Enterprise { domain: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotAuthState {
  pub logged_in: bool,
  pub deployment: CopilotDeployment,
  pub access_token_preview: Option<String>,
  pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthStartRequest {
  pub deployment_type: String,
  pub enterprise_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthStartResponse {
  pub session_id: String,
  pub verification_uri: String,
  pub user_code: String,
  pub expires_in: u64,
  pub interval: u64,
  pub deployment: CopilotDeployment,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthCompleteRequest {
  pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceAuthCompleteResponse {
  pub status: String,
  pub auth_state: Option<CopilotAuthState>,
}

#[derive(Debug, Clone)]
struct PendingDeviceFlow {
  deployment: CopilotDeployment,
  device_code: String,
}

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
  device_code: String,
  user_code: String,
  verification_uri: String,
  expires_in: u64,
  interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
  access_token: Option<String>,
  refresh_token: Option<String>,
  token_type: Option<String>,
  scope: Option<String>,
  error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubUser {
  id: u64,
  login: String,
  name: Option<String>,
  email: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubEmail {
  email: String,
  primary: Option<bool>,
  verified: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct CopilotTokenResponse {
  token: Option<String>,
  expires_at: Option<u64>,
  refresh_in: Option<u64>,
  sku: Option<String>,
  chat_enabled: Option<bool>,
  limited_user_quotas: Option<Value>,
  limited_user_reset_date: Option<String>,
  message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct SimplifiedQuota {
  name: String,
  total: Option<f64>,
  percent: Option<f64>,
  remaining: Option<f64>,
  expires: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct CopilotUserInfoResponse {
  copilot_plan: Option<String>,
  quota_snapshots: Option<Value>,
  quota_reset_date: Option<String>,
  limited_user_quotas: Option<Value>,
  limited_user_reset_date: Option<String>,
  chat_enabled: Option<bool>,
  sku: Option<String>,
  #[serde(flatten)]
  extra: HashMap<String, Value>,
}

#[derive(Clone)]
pub struct CopilotAuthManager {
  state_file: PathBuf,
  sessions: Arc<RwLock<HashMap<String, PendingDeviceFlow>>>,
  config: ConfigManager,
  requests: RequestStore,
  client: reqwest::Client,
}

impl CopilotAuthManager {
  pub fn new(config_dir: PathBuf, config: ConfigManager, requests: RequestStore) -> Self {
    Self {
      state_file: config_dir.join("copilot_auth_state.json"),
      sessions: Arc::new(RwLock::new(HashMap::new())),
      config,
      requests,
      client: reqwest::Client::new(),
    }
  }

  pub async fn start_device_authorization(&self, request: DeviceAuthStartRequest) -> Result<DeviceAuthStartResponse> {
    let deployment = parse_deployment(request)?;
    let auth_base = deployment_auth_base(&deployment);
    tracing::info!(
      target: "auth",
      deployment = %deployment_label(&deployment),
      auth_base = %auth_base,
      "starting Copilot OAuth device authorization"
    );

    let res = self
      .client
      .post(format!("{auth_base}/login/device/code"))
      .header(ACCEPT, "application/json")
      .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
      .form(&[("client_id", GITHUB_CLIENT_ID), ("scope", DEVICE_FLOW_SCOPE)])
      .send()
      .await
      .context("failed to request device code")?;

    if !res.status().is_success() {
      anyhow::bail!("device authorization failed with {}", res.status());
    }

    let body: DeviceCodeResponse = res.json().await.context("failed to parse device code response")?;

    let session_id = uuid::Uuid::new_v4().to_string();
    self.sessions.write().insert(
      session_id.clone(),
      PendingDeviceFlow {
        deployment: deployment.clone(),
        device_code: body.device_code,
      },
    );

    Ok(DeviceAuthStartResponse {
      session_id,
      verification_uri: body.verification_uri,
      user_code: body.user_code,
      expires_in: body.expires_in,
      interval: body.interval.unwrap_or(5),
      deployment,
    })
  }

  pub async fn complete_device_authorization(
    &self,
    request: DeviceAuthCompleteRequest,
  ) -> Result<DeviceAuthCompleteResponse> {
    let pending = self
      .sessions
      .read()
      .get(&request.session_id)
      .cloned()
      .ok_or_else(|| anyhow::anyhow!("unknown session_id"))?;
    tracing::info!(
      target: "auth",
      session_id = %request.session_id,
      deployment = %deployment_label(&pending.deployment),
      "completing Copilot OAuth device authorization"
    );

    let auth_base = deployment_auth_base(&pending.deployment);
    let res = self
      .client
      .post(format!("{auth_base}/login/oauth/access_token"))
      .header(ACCEPT, "application/json")
      .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
      .form(&[
        ("client_id", GITHUB_CLIENT_ID),
        ("device_code", pending.device_code.as_str()),
        ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
      ])
      .send()
      .await
      .context("failed to request device access token")?;

    if !res.status().is_success() {
      anyhow::bail!("token endpoint failed with status {}", res.status());
    }

    let body: TokenResponse = res.json().await.context("failed to parse token response")?;

    if let Some(github_access_token) = body.access_token.clone() {
      let account_id = account_id_for_deployment(&pending.deployment);
      let copilot_token = self.fetch_copilot_token(&github_access_token).await?;
      let mut meta = HashMap::new();
      meta.insert("oauth".to_string(), "true".to_string());
      meta.insert("deployment".to_string(), deployment_label(&pending.deployment));
      if let CopilotDeployment::Enterprise { domain } = &pending.deployment {
        meta.insert("enterprise_domain".to_string(), domain.clone());
      }

      let secrets = HashMap::from_iter(vec![
        ("api_key".to_string(), copilot_token.clone()),
        ("oauth_access_token".to_string(), github_access_token.clone()),
        ("oauth_auth_base".to_string(), auth_base.clone()),
      ]);

      self.config.connect_account(ConnectAccountInput {
        provider: COPILOT_PROVIDER_NAME.to_string(),
        account_id: Some(account_id.clone()),
        label: Some(match &pending.deployment {
          CopilotDeployment::GitHubCom => "GitHub.com".to_string(),
          CopilotDeployment::Enterprise { domain } => format!("Enterprise ({domain})"),
        }),
        auth_type: Some("bearer".to_string()),
        secrets,
        meta,
        set_default: Some(true),
        enabled: Some(true),
      })?;
      tracing::info!(
        target: "auth",
        provider = COPILOT_PROVIDER_NAME,
        account_id = %account_id,
        deployment = %deployment_label(&pending.deployment),
        has_refresh_token = body.refresh_token.is_some(),
        "Copilot OAuth token exchange succeeded and account connected"
      );

      if let Err(err) = self
        .upsert_account_information(
          Utc::now(),
          &account_id,
          &github_access_token,
          body.token_type.clone(),
          body.scope.clone(),
        )
        .await
      {
        tracing::warn!(
          target: "auth",
          error = %err,
          "failed to upsert account_information for Copilot OAuth completion"
        );
        if let Err(fallback_err) = self
          .requests
          .touch_account_information_connected(COPILOT_PROVIDER_NAME, &account_id)
        {
          tracing::warn!(
            target: "auth",
            error = %fallback_err,
            "failed to touch account_information after OAuth completion"
          );
        }
      }

      let auth_state = CopilotAuthState {
        logged_in: true,
        deployment: pending.deployment.clone(),
        access_token_preview: Some(token_preview(&copilot_token)),
        updated_at: Utc::now(),
      };
      self.write_state(&auth_state)?;
      self.sessions.write().remove(&request.session_id);
      tracing::info!(
        target: "auth",
        provider = COPILOT_PROVIDER_NAME,
        account_id = %account_id,
        "Copilot OAuth login completed"
      );

      return Ok(DeviceAuthCompleteResponse {
        status: "ok".to_string(),
        auth_state: Some(auth_state),
      });
    }

    Ok(DeviceAuthCompleteResponse {
      status: body.error.unwrap_or_else(|| "authorization_pending".to_string()),
      auth_state: None,
    })
  }

  pub fn status(&self) -> Result<Option<CopilotAuthState>> {
    let Some(state) = self.read_state()? else {
      return Ok(None);
    };

    let token = self
      .config
      .get()
      .credentials
      .resolve_runtime_credential(COPILOT_PROVIDER_NAME)?
      .and_then(|c| c.api_key);

    if token.is_none() {
      return Ok(None);
    }

    Ok(Some(state))
  }

  pub async fn refresh_api_key(&self, account_id: Option<String>) -> Result<CopilotAuthState> {
    let loaded = self.config.get();
    let resolved = if let Some(ref account) = account_id {
      loaded
        .credentials
        .resolve_runtime_credential_for_account_with_account(COPILOT_PROVIDER_NAME, account)?
    } else {
      loaded
        .credentials
        .resolve_runtime_credential_with_account(COPILOT_PROVIDER_NAME)?
    };
    let (resolved_account_id, credential) = resolved.ok_or_else(|| {
      anyhow::anyhow!(
        "no credential account configured for provider '{}'",
        COPILOT_PROVIDER_NAME
      )
    })?;
    tracing::info!(
      target: "auth",
      provider = COPILOT_PROVIDER_NAME,
      account_id = %resolved_account_id,
      "refreshing Copilot API key from GitHub OAuth access token"
    );

    let github_access_token = credential
      .extra
      .get("oauth_access_token")
      .clone()
      .ok_or_else(|| anyhow::anyhow!("oauth_access_token not found for account '{}'", resolved_account_id))?;
    let copilot_token = self.fetch_copilot_token(&github_access_token).await?;

    let mut set_secrets = HashMap::from_iter(vec![
      ("api_key".to_string(), copilot_token.clone()),
      ("oauth_access_token".to_string(), github_access_token.clone()),
    ]);
    if let Some(auth_base) = credential.extra.get("oauth_auth_base").cloned() {
      set_secrets.insert("oauth_auth_base".to_string(), auth_base);
    }

    self.config.update_account(UpdateAccountInput {
      provider: COPILOT_PROVIDER_NAME.to_string(),
      account_id: resolved_account_id.clone(),
      set_secrets: Some(set_secrets),
      ..Default::default()
    })?;
    tracing::info!(
      target: "auth",
      provider = COPILOT_PROVIDER_NAME,
      account_id = %resolved_account_id,
      "Copilot API key refreshed from GitHub OAuth access token"
    );

    if let Err(err) = self
      .upsert_account_information(Utc::now(), &resolved_account_id, &github_access_token, None, None)
      .await
    {
      tracing::warn!(
        target: "auth",
        error = %err,
        "failed to upsert account_information for Copilot token refresh"
      );
      if let Err(fallback_err) = self
        .requests
        .touch_account_information_connected(COPILOT_PROVIDER_NAME, &resolved_account_id)
      {
        tracing::warn!(
          target: "auth",
          error = %fallback_err,
          "failed to touch account_information after Copilot token refresh"
        );
      }
    }

    let mut state = self.read_state()?.unwrap_or(CopilotAuthState {
      logged_in: true,
      deployment: deployment_from_auth_base(
        credential
          .extra
          .get("oauth_auth_base")
          .map(String::as_str)
          .unwrap_or("https://github.com"),
      ),
      access_token_preview: None,
      updated_at: Utc::now(),
    });
    state.logged_in = true;
    state.access_token_preview = Some(token_preview(&copilot_token));
    state.updated_at = Utc::now();
    self.write_state(&state)?;

    Ok(state)
  }

  pub fn logout(&self) -> Result<()> {
    if let Some(state) = self.read_state()? {
      let account_id = account_id_for_deployment(&state.deployment);
      tracing::info!(
        target: "auth",
        provider = COPILOT_PROVIDER_NAME,
        account_id = %account_id,
        deployment = %deployment_label(&state.deployment),
        "logging out Copilot OAuth account"
      );
      self.config.disconnect_account(COPILOT_PROVIDER_NAME, &account_id)?;
      self
        .requests
        .mark_account_information_disconnected(COPILOT_PROVIDER_NAME, &account_id)?;
      tracing::info!(
        target: "auth",
        provider = COPILOT_PROVIDER_NAME,
        account_id = %account_id,
        "Copilot OAuth account logged out"
      );
    }
    if self.state_file.exists() {
      std::fs::remove_file(&self.state_file).context("failed to remove persisted state")?;
    }
    Ok(())
  }

  pub fn copilot_api_base(deployment: &CopilotDeployment) -> String {
    match deployment {
      CopilotDeployment::GitHubCom => "https://api.githubcopilot.com".to_string(),
      CopilotDeployment::Enterprise { domain } => {
        // TODO: verify enterprise Copilot API derivation for all enterprise setups.
        format!("https://api.{domain}/copilot")
      }
    }
  }

  fn write_state(&self, state: &CopilotAuthState) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(state)?;
    std::fs::write(&self.state_file, bytes).with_context(|| format!("failed writing {}", self.state_file.display()))
  }

  fn read_state(&self) -> Result<Option<CopilotAuthState>> {
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
    oauth_token_type: Option<String>,
    oauth_scope: Option<String>,
  ) -> Result<()> {
    let github_user = self.fetch_github_user(access_token).await?;
    let github_email = if let Some(email) = github_user.email.clone() {
      Some(email)
    } else {
      match self.fetch_primary_github_email(access_token).await {
        Ok(email) => email,
        Err(err) => {
          tracing::warn!(
            target: "auth",
            provider = COPILOT_PROVIDER_NAME,
            account_id = account_id,
            error = %err,
            "failed to fetch GitHub email, skipping email field"
          );
          None
        }
      }
    };
    let copilot_user_info = self.fetch_copilot_user_info(access_token).await.ok();

    let mut metadata = HashMap::new();
    metadata.insert("github_login".to_string(), Value::String(github_user.login.clone()));
    if let Some(token_type) = oauth_token_type {
      metadata.insert("oauth_token_type".to_string(), Value::String(token_type));
    }
    if let Some(scope) = oauth_scope {
      metadata.insert("oauth_scope".to_string(), Value::String(scope));
    }
    if let Some(info) = &copilot_user_info {
      metadata.insert(
        "chat_enabled".to_string(),
        info.chat_enabled.map(Value::Bool).unwrap_or(Value::Null),
      );
      metadata.insert("copilot_sku".to_string(), to_json_string_value(info.sku.clone()));
      metadata.insert(
        "quota_snapshots".to_string(),
        info.quota_snapshots.clone().unwrap_or(Value::Null),
      );
      metadata.insert(
        "limited_user_quotas".to_string(),
        info.limited_user_quotas.clone().unwrap_or(Value::Null),
      );
      metadata.insert(
        "limited_user_reset_date".to_string(),
        to_json_string_value(info.limited_user_reset_date.clone()),
      );
      metadata.insert(
        "copilot_user_info_extra".to_string(),
        serde_json::to_value(&info.extra).unwrap_or(Value::Object(Default::default())),
      );
    }

    let reset_date = copilot_user_info
      .as_ref()
      .and_then(|info| info.quota_reset_date.clone())
      .or_else(|| {
        copilot_user_info
          .as_ref()
          .and_then(|info| info.limited_user_reset_date.clone())
      });

    let plan = copilot_user_info
      .as_ref()
      .and_then(|info| info.copilot_plan.clone().or(info.sku.clone()));
    let simplified_quotas = copilot_user_info.as_ref().map(simplify_quotas).unwrap_or_default();
    let quota = if simplified_quotas.is_empty() {
      None
    } else {
      Some(serde_json::to_string(&simplified_quotas).context("failed to serialize simplified quotas")?)
    };
    let display_name = github_user
      .name
      .clone()
      .filter(|v| !v.trim().is_empty())
      .unwrap_or_else(|| github_user.login.clone());
    let has_name = !display_name.trim().is_empty();
    let has_email = github_email.is_some();
    let has_plan = plan.is_some();
    let has_quota = quota.is_some();
    let has_reset_date = reset_date.is_some();
    let metadata_keys = metadata.len();

    self.requests.upsert_account_information(AccountInformationRecord {
      observed_at,
      provider: COPILOT_PROVIDER_NAME.to_string(),
      account_id: account_id.to_string(),
      user_id: Some(github_user.id.to_string()),
      name: Some(display_name),
      email: github_email,
      plan,
      quota,
      reset_date,
      status: "connected".to_string(),
      metadata,
    })?;
    tracing::info!(
      target: "account",
      provider = COPILOT_PROVIDER_NAME,
      account_id = account_id,
      has_user_id = true,
      has_name = has_name,
      has_email = has_email,
      has_plan = has_plan,
      has_quota = has_quota,
      has_reset_date = has_reset_date,
      metadata_keys = metadata_keys,
      "account information upserted from Copilot OAuth snapshot"
    );

    Ok(())
  }

  async fn fetch_github_user(&self, github_access_token: &str) -> Result<GitHubUser> {
    let response = self
      .client
      .get(GITHUB_USER_ENDPOINT)
      .header(USER_AGENT, APP_USER_AGENT)
      .header(ACCEPT, "application/vnd.github+json")
      .header(AUTHORIZATION, format!("Bearer {github_access_token}"))
      .send()
      .await
      .context("failed to request GitHub user profile")?;

    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      anyhow::bail!("GitHub user profile request failed: status={status}, body={body}");
    }

    let uesr = response
      .json::<GitHubUser>()
      .await
      .context("failed to parse GitHub user profile response")?;

    tracing::debug!(
      target: "auth",
      provider = COPILOT_PROVIDER_NAME,
      github_user_id = uesr.id,
      github_login = %uesr.login,
      has_name = uesr.name.is_some(),
      has_email = uesr.email.is_some(),
      "fetched GitHub user profile"
    );

    Ok(uesr)
  }

  async fn fetch_primary_github_email(&self, github_access_token: &str) -> Result<Option<String>> {
    let response = self
      .client
      .get(GITHUB_USER_EMAILS_ENDPOINT)
      .header(USER_AGENT, APP_USER_AGENT)
      .header(ACCEPT, "application/vnd.github+json")
      .header(AUTHORIZATION, format!("Bearer {github_access_token}"))
      .send()
      .await
      .context("failed to request GitHub email list")?;

    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      anyhow::bail!("GitHub email list request failed: status={status}, body={body}");
    }

    let emails = response
      .json::<Vec<GitHubEmail>>()
      .await
      .context("failed to parse GitHub email list response")?;

    let primary = emails
      .iter()
      .find(|item| item.primary.unwrap_or(false) && item.verified.unwrap_or(false))
      .map(|item| item.email.clone());
    if primary.is_some() {
      return Ok(primary);
    }

    Ok(
      emails
        .iter()
        .find(|item| item.verified.unwrap_or(false))
        .map(|item| item.email.clone()),
    )
  }

  async fn fetch_copilot_user_info(&self, github_access_token: &str) -> Result<CopilotUserInfoResponse> {
    let response = self
      .client
      .get(GITHUB_COPILOT_USER_INFO_ENDPOINT)
      .header(USER_AGENT, APP_USER_AGENT)
      .header(ACCEPT, "application/json")
      .header("X-GitHub-Api-Version", "2025-04-01")
      .header(AUTHORIZATION, format!("token {github_access_token}"))
      .send()
      .await
      .context("failed to request Copilot user info")?;

    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      anyhow::bail!("Copilot user info request failed: status={status}, body={body}");
    }

    response
      .json::<CopilotUserInfoResponse>()
      .await
      .context("failed to parse Copilot user info response")
  }

  async fn fetch_copilot_token(&self, github_access_token: &str) -> Result<String> {
    let response = self
      .client
      .get(GITHUB_COPILOT_TOKEN_ENDPOINT)
      .header(USER_AGENT, APP_USER_AGENT)
      .header(ACCEPT, "application/json")
      .header("X-GitHub-Api-Version", "2025-04-01")
      .header(AUTHORIZATION, format!("token {github_access_token}"))
      .send()
      .await
      .context("failed to request Copilot API token")?;

    if !response.status().is_success() {
      let status = response.status();
      let body = response.text().await.unwrap_or_default();
      anyhow::bail!("Copilot token request failed: status={status}, body={body}");
    }

    let payload = response
      .json::<CopilotTokenResponse>()
      .await
      .context("failed to parse Copilot token response")?;

    let token = payload
      .token
      .ok_or_else(|| anyhow::anyhow!(payload.message.unwrap_or_else(|| "copilot token missing".to_string())))?;

    tracing::info!(
      target: "auth",
      provider = COPILOT_PROVIDER_NAME,
      chat_enabled = payload.chat_enabled.unwrap_or(false),
      has_expires_at = payload.expires_at.is_some(),
      has_refresh_in = payload.refresh_in.is_some(),
      has_limited_user_quotas = payload.limited_user_quotas.is_some(),
      has_limited_user_reset_date = payload.limited_user_reset_date.is_some(),
      has_sku = payload.sku.is_some(),
      "fetched Copilot API token snapshot"
    );

    Ok(token)
  }
}

fn to_json_string_value(v: Option<String>) -> Value {
  v.map(Value::String).unwrap_or(Value::Null)
}

fn simplify_quotas(info: &CopilotUserInfoResponse) -> Vec<SimplifiedQuota> {
  let mut out = Vec::new();
  if let Some(v) = &info.quota_snapshots {
    collect_simplified_quotas(v, info.quota_reset_date.as_deref(), &mut out);
  }
  if let Some(v) = &info.limited_user_quotas {
    collect_simplified_quotas(v, info.limited_user_reset_date.as_deref(), &mut out);
  }
  out
}

fn collect_simplified_quotas(source: &Value, default_expires: Option<&str>, out: &mut Vec<SimplifiedQuota>) {
  match source {
    Value::Object(map) => {
      for (name, value) in map {
        out.push(build_simplified_quota(name.to_string(), value, default_expires));
      }
    }
    Value::Array(items) => {
      for (idx, item) in items.iter().enumerate() {
        let name = item
          .get("name")
          .and_then(Value::as_str)
          .map(ToString::to_string)
          .unwrap_or_else(|| format!("quota_{idx}"));
        out.push(build_simplified_quota(name, item, default_expires));
      }
    }
    _ => {}
  }
}

fn build_simplified_quota(name: String, value: &Value, default_expires: Option<&str>) -> SimplifiedQuota {
  let total = value_number_by_keys(value, &["entitlement", "total", "limit", "quota", "monthly_limit"]);
  let remaining = value_number_by_keys(value, &["remaining", "available", "remaining_count", "count"]);
  let percent = value_number_by_keys(
    value,
    &[
      "percent",
      "percent_remaining",
      "percentage_remaining",
      "remaining_percent",
    ],
  )
  .or_else(|| {
    if let (Some(t), Some(r)) = (total, remaining) {
      if t > 0.0 {
        Some((r / t) * 100.0)
      } else {
        None
      }
    } else {
      None
    }
  });
  let expires = value_string_by_keys(value, &["expires", "expires_at", "reset_date", "reset_at"])
    .or_else(|| default_expires.map(ToString::to_string));

  SimplifiedQuota {
    name,
    total,
    percent,
    remaining,
    expires,
  }
}

fn value_number_by_keys(value: &Value, keys: &[&str]) -> Option<f64> {
  for key in keys {
    let Some(v) = value.get(*key) else {
      continue;
    };
    if let Some(num) = v.as_f64() {
      return Some(num);
    }
    if let Some(text) = v.as_str() {
      if let Ok(parsed) = text.parse::<f64>() {
        return Some(parsed);
      }
    }
  }
  None
}

fn value_string_by_keys(value: &Value, keys: &[&str]) -> Option<String> {
  for key in keys {
    let Some(v) = value.get(*key) else {
      continue;
    };
    if let Some(text) = v.as_str() {
      return Some(text.to_string());
    }
  }
  None
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

fn account_id_for_deployment(deployment: &CopilotDeployment) -> String {
  match deployment {
    CopilotDeployment::GitHubCom => "copilot-github-com".to_string(),
    CopilotDeployment::Enterprise { domain } => format!("copilot-enterprise-{}", domain.replace('.', "-")),
  }
}

fn deployment_label(deployment: &CopilotDeployment) -> String {
  match deployment {
    CopilotDeployment::GitHubCom => "github.com".to_string(),
    CopilotDeployment::Enterprise { domain } => format!("enterprise:{domain}"),
  }
}

fn parse_deployment(req: DeviceAuthStartRequest) -> Result<CopilotDeployment> {
  match req.deployment_type.as_str() {
    "github.com" => Ok(CopilotDeployment::GitHubCom),
    "enterprise" => {
      let Some(url_or_domain) = req.enterprise_url else {
        anyhow::bail!("enterprise_url is required for enterprise deployment")
      };
      let domain = normalize_enterprise_domain(&url_or_domain)?;
      Ok(CopilotDeployment::Enterprise { domain })
    }
    _ => anyhow::bail!("unknown deployment_type: {}", req.deployment_type),
  }
}

fn deployment_from_auth_base(auth_base: &str) -> CopilotDeployment {
  let normalized = auth_base.trim_end_matches('/').to_lowercase();
  if normalized == "https://github.com" {
    CopilotDeployment::GitHubCom
  } else {
    let domain = normalized
      .strip_prefix("https://")
      .unwrap_or(normalized.as_str())
      .to_string();
    CopilotDeployment::Enterprise { domain }
  }
}

fn deployment_auth_base(deployment: &CopilotDeployment) -> String {
  match deployment {
    CopilotDeployment::GitHubCom => "https://github.com".to_string(),
    CopilotDeployment::Enterprise { domain } => format!("https://{domain}"),
  }
}

pub fn normalize_enterprise_domain(input: &str) -> Result<String> {
  let trimmed = input.trim().trim_end_matches('/');
  if trimmed.is_empty() {
    anyhow::bail!("enterprise domain/url cannot be empty");
  }

  let candidate = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
    trimmed.to_string()
  } else {
    format!("https://{trimmed}")
  };

  let parsed = url::Url::parse(&candidate).context("invalid enterprise URL")?;
  let host = parsed
    .host_str()
    .ok_or_else(|| anyhow::anyhow!("enterprise URL is missing host"))?;

  if host.eq_ignore_ascii_case("github.com") {
    anyhow::bail!("use deployment_type=github.com for public GitHub");
  }

  Ok(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalizes_enterprise_domain() {
    let got = normalize_enterprise_domain("https://GitHub.company.local/").unwrap();
    assert_eq!(got, "github.company.local");
  }

  #[test]
  fn rejects_github_public_for_enterprise() {
    let err = normalize_enterprise_domain("github.com").unwrap_err();
    assert!(err.to_string().contains("github.com"));
  }

  #[test]
  fn derives_enterprise_copilot_api_base() {
    let deployment = CopilotDeployment::Enterprise {
      domain: "git.corp.example".to_string(),
    };
    let base = CopilotAuthManager::copilot_api_base(&deployment);
    assert_eq!(base, "https://api.git.corp.example/copilot");
  }
}
