use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod paths;

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_HOST: &str = "127.0.0.1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub pool: PoolConfig,
    #[serde(default)]
    pub usage: UsageConfig,
    #[serde(default)]
    pub copilot: CopilotHeaders,
    #[serde(default)]
    pub accounts: Vec<Account>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            pool: PoolConfig::default(),
            usage: UsageConfig::default(),
            copilot: CopilotHeaders::default(),
            accounts: Vec::new(),
        }
    }
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
        Self { host: default_host(), port: default_port() }
    }
}
fn default_host() -> String { DEFAULT_HOST.to_string() }
fn default_port() -> u16 { DEFAULT_PORT }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
    #[serde(default = "default_strategy")]
    pub strategy: String,
    #[serde(default = "default_cooldown")]
    pub failure_cooldown_secs: u64,
}
impl Default for PoolConfig {
    fn default() -> Self {
        Self { strategy: default_strategy(), failure_cooldown_secs: default_cooldown() }
    }
}
fn default_strategy() -> String { "round_robin".into() }
fn default_cooldown() -> u64 { 60 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub db_path: Option<PathBuf>,
}
impl Default for UsageConfig {
    fn default() -> Self { Self { enabled: true, db_path: None } }
}
fn default_true() -> bool { true }

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
            extra_headers: BTreeMap::new(),
        }
    }
}
fn default_editor_version() -> String { "vscode/1.95.0".into() }
fn default_editor_plugin_version() -> String { "copilot-chat/0.20.0".into() }
fn default_user_agent() -> String { "GitHubCopilotChat/0.20.0".into() }
fn default_integration_id() -> String { "vscode-chat".into() }
fn default_openai_intent() -> String { "conversation-panel".into() }

impl CopilotHeaders {
    /// Merge `self` (global) with `override_` (per-account). Per-account fields
    /// take precedence; `extra_headers` are merged with override winning.
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
                    extra_headers: extra,
                }
            }
        }
    }

    /// Validate header field values and extra header names.
    pub fn validate(&self) -> Result<()> {
        for (name, _) in &self.extra_headers {
            if !is_token(name) {
                return Err(anyhow!("invalid header name in [copilot.extra_headers]: {name:?}"));
            }
            let lower = name.to_ascii_lowercase();
            if matches!(lower.as_str(),
                "authorization" | "host" | "content-length" | "content-type") {
                return Err(anyhow!("header {name:?} is reserved and cannot be set via extra_headers"));
            }
        }
        for (label, val) in [
            ("editor_version", &self.editor_version),
            ("editor_plugin_version", &self.editor_plugin_version),
            ("user_agent", &self.user_agent),
            ("copilot_integration_id", &self.copilot_integration_id),
            ("openai_intent", &self.openai_intent),
        ] {
            if val.trim().is_empty() {
                return Err(anyhow!("[copilot].{label} must be non-empty"));
            }
        }
        Ok(())
    }
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| matches!(b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
            | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub github_token: String,
    /// Cached short-lived Copilot API token; written back by the daemon.
    #[serde(default)]
    pub api_token: Option<String>,
    /// Unix seconds when `api_token` expires.
    #[serde(default)]
    pub api_token_expires_at: Option<i64>,
    /// Optional per-account header overrides.
    #[serde(default)]
    pub copilot: Option<CopilotHeaders>,
}

impl Config {
    pub fn load(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => paths::config_path()?,
        };
        if !path.exists() {
            return Ok((Config::default(), path));
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        let cfg: Config = toml::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;
        cfg.copilot.validate()?;
        for a in &cfg.accounts {
            if let Some(h) = &a.copilot {
                h.validate()
                    .with_context(|| format!("account {}: invalid [copilot] override", a.id))?;
            }
        }
        Ok((cfg, path))
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let toml = toml::to_string_pretty(self)?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perm = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(&tmp, perm)?;
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn upsert_account(&mut self, a: Account) {
        if let Some(slot) = self.accounts.iter_mut().find(|x| x.id == a.id) {
            *slot = a;
        } else {
            self.accounts.push(a);
        }
    }
}

pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("dev", "llm-router", "llm-router")
        .ok_or_else(|| anyhow!("could not resolve XDG project dirs"))
}
