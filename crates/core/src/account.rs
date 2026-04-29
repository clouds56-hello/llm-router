pub use crate::util::secret::Secret;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountConfig {
  pub id: String,
  #[serde(default = "default_provider")]
  pub provider: String,
  #[serde(default = "default_true")]
  pub enabled: bool,
  #[serde(default)]
  pub tags: Vec<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub label: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub base_url: Option<String>,
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub headers: BTreeMap<String, String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub auth_type: Option<AuthType>,
  #[serde(default)]
  pub username: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key: Option<Secret<String>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key_expires_at: Option<i64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub access_token: Option<Secret<String>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub access_token_expires_at: Option<i64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub id_token: Option<Secret<String>>,
  #[serde(default)]
  pub refresh_token: Option<Secret<String>>,
  #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
  pub extra: BTreeMap<String, Secret<String>>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub refresh_url: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub last_refresh: Option<i64>,
  #[serde(default)]
  pub settings: toml::Table,
}

pub type Account = AccountConfig;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuthType {
  Bearer,
  XApiKey,
  Header(String),
}

fn default_provider() -> String {
  crate::provider::ID_GITHUB_COPILOT.to_string()
}

fn default_true() -> bool {
  true
}
