pub use crate::util::secret::Secret;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
  pub id: String,
  #[serde(default = "default_provider")]
  pub provider: String,
  #[serde(default)]
  pub github_token: Option<Secret<String>>,
  #[serde(default)]
  pub api_token: Option<Secret<String>>,
  #[serde(default)]
  pub api_token_expires_at: Option<i64>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key: Option<Secret<String>>,
  #[serde(default)]
  pub copilot: Option<serde_json::Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub zai: Option<ZaiAccountConfig>,
  #[serde(default)]
  pub behave_as: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ZaiAccountConfig {
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub base_url: Option<String>,
}

fn default_provider() -> String {
  crate::provider::ID_GITHUB_COPILOT.to_string()
}
