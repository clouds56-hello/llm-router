pub use crate::util::secret::Secret;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PROVIDER: &str = "github-copilot";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
  #[serde(default)]
  pub server: ServerConfig,
  #[serde(default)]
  pub pool: PoolConfig,
  #[serde(default, alias = "usage")]
  pub db: DbConfig,
  #[serde(default)]
  pub proxy: ProxyConfig,
  #[serde(default)]
  pub logging: LoggingConfig,
  #[serde(default)]
  pub copilot: CopilotHeaders,
  #[serde(default)]
  pub accounts: Vec<Account>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
  #[serde(default = "default_host")]
  pub host: String,
  #[serde(default = "default_port")]
  pub port: u16,
}

impl Default for ServerConfig {
  fn default() -> Self {
    Self {
      host: default_host(),
      port: default_port(),
    }
  }
}

fn default_host() -> String {
  DEFAULT_HOST.to_string()
}

fn default_port() -> u16 {
  DEFAULT_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
  #[serde(default = "default_strategy")]
  pub strategy: String,
  #[serde(default = "default_cooldown")]
  pub failure_cooldown_secs: u64,
  #[serde(default = "default_session_ttl")]
  pub session_ttl_secs: u64,
  #[serde(default = "default_session_tombstone")]
  pub session_tombstone_secs: u64,
}

impl Default for PoolConfig {
  fn default() -> Self {
    Self {
      strategy: default_strategy(),
      failure_cooldown_secs: default_cooldown(),
      session_ttl_secs: default_session_ttl(),
      session_tombstone_secs: default_session_tombstone(),
    }
  }
}

fn default_strategy() -> String {
  "round_robin".into()
}

fn default_cooldown() -> u64 {
  60
}

fn default_session_ttl() -> u64 {
  1800
}

fn default_session_tombstone() -> u64 {
  7200
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbConfig {
  #[serde(default = "default_true")]
  pub enabled: bool,
  #[serde(default, alias = "db_path")]
  pub usage_db_path: Option<PathBuf>,
  #[serde(default)]
  pub sessions_db_path: Option<PathBuf>,
  #[serde(default)]
  pub requests_dir: Option<PathBuf>,
  #[serde(default = "default_true")]
  pub record_sessions: bool,
  #[serde(default = "default_true")]
  pub record_request_bodies: bool,
  #[serde(default = "default_body_max_bytes")]
  pub body_max_bytes: usize,
  #[serde(default = "default_write_queue_capacity")]
  pub write_queue_capacity: usize,
}

impl Default for DbConfig {
  fn default() -> Self {
    Self {
      enabled: true,
      usage_db_path: None,
      sessions_db_path: None,
      requests_dir: None,
      record_sessions: true,
      record_request_bodies: true,
      body_max_bytes: default_body_max_bytes(),
      write_queue_capacity: default_write_queue_capacity(),
    }
  }
}

fn default_true() -> bool {
  true
}

fn default_body_max_bytes() -> usize {
  10 * 1024 * 1024
}

fn default_write_queue_capacity() -> usize {
  4096
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyConfig {
  #[serde(default)]
  pub url: Option<String>,
  #[serde(default)]
  pub no_proxy: Vec<String>,
  #[serde(default)]
  pub system: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
  #[serde(default = "default_log_level")]
  pub level: String,
  #[serde(default)]
  pub format: LogFormat,
  #[serde(default)]
  pub target: LogTarget,
  #[serde(default)]
  pub dir: Option<PathBuf>,
  #[serde(default = "default_true")]
  pub ansi: bool,
  #[serde(default)]
  pub include_spans: bool,
}

impl Default for LoggingConfig {
  fn default() -> Self {
    Self {
      level: default_log_level(),
      format: LogFormat::default(),
      target: LogTarget::default(),
      dir: None,
      ansi: true,
      include_spans: false,
    }
  }
}

fn default_log_level() -> String {
  "info,llm_router=info".into()
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogFormat {
  Pretty,
  #[default]
  Compact,
  Json,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum LogTarget {
  Stderr,
  File,
  #[default]
  Both,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InitiatorMode {
  #[default]
  Auto,
  AlwaysUser,
  AlwaysAgent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CopilotHeaders {
  #[serde(default = "default_editor_version")]
  pub editor_version: String,
  #[serde(default = "default_editor_plugin_version")]
  pub editor_plugin_version: String,
  #[serde(default = "default_user_agent")]
  pub user_agent: String,
  #[serde(default = "default_integration_id")]
  pub copilot_integration_id: String,
  #[serde(default = "default_openai_intent")]
  pub openai_intent: String,
  #[serde(default)]
  pub initiator_mode: InitiatorMode,
  #[serde(default)]
  pub behave_as: Option<String>,
  #[serde(default)]
  pub extra_headers: BTreeMap<String, String>,
}

impl Default for CopilotHeaders {
  fn default() -> Self {
    Self {
      editor_version: default_editor_version(),
      editor_plugin_version: default_editor_plugin_version(),
      user_agent: default_user_agent(),
      copilot_integration_id: default_integration_id(),
      openai_intent: default_openai_intent(),
      initiator_mode: InitiatorMode::default(),
      behave_as: None,
      extra_headers: BTreeMap::new(),
    }
  }
}

fn default_editor_version() -> String {
  "vscode/1.95.0".into()
}

fn default_editor_plugin_version() -> String {
  "copilot-chat/0.20.0".into()
}

fn default_user_agent() -> String {
  "GitHubCopilotChat/0.20.0".into()
}

fn default_integration_id() -> String {
  "vscode-chat".into()
}

fn default_openai_intent() -> String {
  "conversation-panel".into()
}

impl CopilotHeaders {
  pub fn merged(&self, override_: Option<&CopilotHeaders>) -> CopilotHeaders {
    match override_ {
      None => self.clone(),
      Some(o) => {
        let mut extra = self.extra_headers.clone();
        for (k, v) in &o.extra_headers {
          extra.insert(k.clone(), v.clone());
        }
        CopilotHeaders {
          editor_version: o.editor_version.clone(),
          editor_plugin_version: o.editor_plugin_version.clone(),
          user_agent: o.user_agent.clone(),
          copilot_integration_id: o.copilot_integration_id.clone(),
          openai_intent: o.openai_intent.clone(),
          initiator_mode: o.initiator_mode,
          behave_as: o.behave_as.clone().or_else(|| self.behave_as.clone()),
          extra_headers: extra,
        }
      }
    }
  }
}

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
  pub copilot: Option<CopilotHeaders>,
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
  DEFAULT_PROVIDER.to_string()
}
