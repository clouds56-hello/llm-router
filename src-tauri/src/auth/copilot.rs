use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use reqwest::header::{ACCEPT, CONTENT_TYPE};
use serde::{Deserialize, Serialize};

const GITHUB_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
const DEVICE_FLOW_SCOPE: &str = "read:user copilot";

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
  error: Option<String>,
}

trait TokenStore: Send + Sync {
  fn get(&self, key: &str) -> Result<Option<String>>;
  fn set(&self, key: &str, value: &str) -> Result<()>;
  fn delete(&self, key: &str) -> Result<()>;
}

#[derive(Default)]
struct KeyringTokenStore;

impl TokenStore for KeyringTokenStore {
  fn get(&self, key: &str) -> Result<Option<String>> {
    let entry = keyring::Entry::new("llm-router", key)?;
    match entry.get_password() {
      Ok(v) => Ok(Some(v)),
      Err(keyring::Error::NoEntry) => Ok(None),
      Err(err) => Err(err.into()),
    }
  }

  fn set(&self, key: &str, value: &str) -> Result<()> {
    let entry = keyring::Entry::new("llm-router", key)?;
    entry.set_password(value)?;
    Ok(())
  }

  fn delete(&self, key: &str) -> Result<()> {
    let entry = keyring::Entry::new("llm-router", key)?;
    match entry.delete_credential() {
      Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
      Err(err) => Err(err.into()),
    }
  }
}

#[derive(Clone)]
pub struct CopilotAuthManager {
  state_file: PathBuf,
  sessions: Arc<RwLock<HashMap<String, PendingDeviceFlow>>>,
  token_store: Arc<dyn TokenStore>,
  client: reqwest::Client,
}

impl CopilotAuthManager {
  pub fn new(config_dir: PathBuf) -> Self {
    Self {
      state_file: config_dir.join("copilot_auth_state.json"),
      sessions: Arc::new(RwLock::new(HashMap::new())),
      token_store: Arc::new(KeyringTokenStore),
      client: reqwest::Client::new(),
    }
  }

  pub async fn start_device_authorization(&self, request: DeviceAuthStartRequest) -> Result<DeviceAuthStartResponse> {
    let deployment = parse_deployment(request)?;
    let auth_base = deployment_auth_base(&deployment);

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

    if let Some(token) = body.access_token {
      let key = token_key(&pending.deployment);
      self
        .token_store
        .set(&key, &token)
        .context("failed to persist token in secure store")?;

      let auth_state = CopilotAuthState {
        logged_in: true,
        deployment: pending.deployment.clone(),
        access_token_preview: Some(token_preview(&token)),
        updated_at: Utc::now(),
      };
      self.write_state(&auth_state)?;
      self.sessions.write().remove(&request.session_id);

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

    let key = token_key(&state.deployment);
    let has_token = self.token_store.get(&key)?.is_some();
    if !has_token {
      return Ok(None);
    }

    Ok(Some(state))
  }

  pub fn logout(&self) -> Result<()> {
    if let Some(state) = self.read_state()? {
      self.token_store.delete(&token_key(&state.deployment))?;
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

fn token_key(deployment: &CopilotDeployment) -> String {
  match deployment {
    CopilotDeployment::GitHubCom => "copilot-github-com".to_string(),
    CopilotDeployment::Enterprise { domain } => format!("copilot-enterprise-{domain}"),
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
