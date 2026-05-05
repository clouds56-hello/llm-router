pub mod error;
pub mod paths;
pub mod profiles;

pub use error::{Error, Result};
pub use llm_core::account::{Account, AccountConfig, AuthType};

use directories::ProjectDirs;
use llm_core::provider::ID_GITHUB_COPILOT;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PROXY_PORT: u16 = 4142;
pub const DEFAULT_PROVIDER: &str = ID_GITHUB_COPILOT;

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
  pub proxy_mode: ProxyModeConfig,
  #[serde(default)]
  pub logging: LoggingConfig,
  #[serde(default)]
  pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigRaw {
  #[serde(flatten)]
  config: Config,
  #[serde(default)]
  copilot: Option<toml::Table>,
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

fn default_proxy_port() -> u16 {
  DEFAULT_PROXY_PORT
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolConfig {
  #[serde(default = "default_strategy")]
  pub strategy: String,
  #[serde(default = "default_cooldown")]
  pub failure_cooldown_secs: u64,
  /// How long a session id stays bound to its chosen account.
  /// Sliding window: refreshed on every successful use.
  #[serde(default = "default_session_ttl")]
  pub session_ttl_secs: u64,
  /// After eviction, remember the session id as a tombstone for this long
  /// so subsequent requests get an explicit `session expired` error
  /// instead of being silently re-bound to a different account.
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

/// Outbound HTTP/HTTPS/SOCKS proxy configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProxyConfig {
  #[serde(default)]
  pub url: Option<String>,
  #[serde(default)]
  pub no_proxy: Vec<String>,
  #[serde(default)]
  pub system: bool,
}

impl ProxyConfig {
  pub fn validate(&self) -> Result<()> {
    if let Some(u) = &self.url {
      let parsed = reqwest::Url::parse(u).map_err(|e| Error::ProxyUrl {
        url: u.clone(),
        message: e.to_string(),
      })?;
      match parsed.scheme() {
        "http" | "https" | "socks5" | "socks5h" => {}
        other => {
          return error::ProxySchemeSnafu {
            scheme: other.to_string(),
          }
          .fail()
        }
      }
    }
    Ok(())
  }

  pub fn to_http_options(&self) -> llm_core::util::http::HttpClientOptions {
    llm_core::util::http::HttpClientOptions {
      url: self.url.clone(),
      no_proxy: self.no_proxy.clone(),
      system: self.system,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyModeConfig {
  #[serde(default = "default_host")]
  pub host: String,
  #[serde(default = "default_proxy_port")]
  pub port: u16,
  #[serde(default)]
  pub ca_dir: Option<PathBuf>,
  #[serde(default)]
  pub intercept_hosts: Vec<String>,
  #[serde(default)]
  pub passthrough_hosts: Vec<String>,
}

impl Default for ProxyModeConfig {
  fn default() -> Self {
    Self {
      host: default_host(),
      port: default_proxy_port(),
      ca_dir: None,
      intercept_hosts: Vec::new(),
      passthrough_hosts: Vec::new(),
    }
  }
}

impl ProxyModeConfig {
  pub fn validate(&self) -> Result<()> {
    for host in &self.intercept_hosts {
      if !is_proxy_host(host) {
        return error::ProxyInterceptHostSnafu { host: host.clone() }.fail();
      }
    }
    for host in &self.passthrough_hosts {
      if !is_proxy_host(host) {
        return error::ProxyPassthroughHostSnafu { host: host.clone() }.fail();
      }
    }
    Ok(())
  }

  pub fn resolved_ca_dir(&self) -> Result<PathBuf> {
    self.ca_dir.clone().map(Ok).unwrap_or_else(paths::default_ca_dir)
  }
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

impl Config {
  pub fn load(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
    let path = match explicit {
      Some(p) => p.to_path_buf(),
      None => paths::config_path()?,
    };
    if !path.exists() {
      return Ok((Config::default(), path));
    }
    let raw = std::fs::read_to_string(&path).context(error::ReadSnafu { path: path.clone() })?;
    let raw_cfg: ConfigRaw = toml::from_str(&raw).context(error::ParseSnafu { path: path.clone() })?;
    if raw_cfg.copilot.is_some() {
      tracing::warn!(
        "top-level [copilot] config is ignored by the new account schema; move values under [accounts.settings]"
      );
    }
    let cfg = raw_cfg.config;
    cfg.validate()?;
    tracing::debug!(path = %path.display(), accounts = cfg.accounts.len(), "config loaded");
    Ok((cfg, path))
  }

  pub fn validate(&self) -> Result<()> {
    self.proxy.validate()?;
    self.proxy_mode.validate()?;
    for a in &self.accounts {
      validate_account_common(a)?;
    }
    Ok(())
  }

  pub fn save(&self, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).context(error::CreateDirSnafu {
        path: parent.to_path_buf(),
      })?;
    }
    let toml = toml::to_string_pretty(self).context(error::SerializeSnafu)?;
    write_atomic(path, &toml)?;
    tracing::debug!(path = %path.display(), "config saved");
    Ok(())
  }

  pub fn edit_in_place<F>(path: &Path, f: F) -> Result<()>
  where
    F: FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
  {
    let raw = if path.exists() {
      std::fs::read_to_string(path).context(error::ReadSnafu {
        path: path.to_path_buf(),
      })?
    } else {
      String::new()
    };
    let mut doc: toml_edit::DocumentMut = raw.parse().context(error::ParseEditSnafu {
      path: path.to_path_buf(),
    })?;
    f(&mut doc)?;
    let serialised = doc.to_string();
    let cfg: Config = toml::from_str(&serialised).context(error::EditValidateSnafu)?;
    cfg.proxy.validate().map_err(|e| Error::EditValidateSection {
      section: "[proxy]",
      source: Box::new(e),
    })?;
    cfg.proxy_mode.validate().map_err(|e| Error::EditValidateSection {
      section: "[proxy_mode]",
      source: Box::new(e),
    })?;
    for a in &cfg.accounts {
      validate_account_common(a).map_err(|e| Error::EditValidateSection {
        section: "[[accounts]]",
        source: Box::new(e),
      })?;
    }
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).context(error::CreateDirSnafu {
        path: parent.to_path_buf(),
      })?;
    }
    write_atomic(path, &serialised)
  }

  pub fn upsert_account(&mut self, a: AccountConfig) {
    if let Some(slot) = self.accounts.iter_mut().find(|x| x.id == a.id) {
      *slot = a;
    } else {
      self.accounts.push(a);
    }
  }
}

fn validate_account_common(account: &AccountConfig) -> Result<()> {
  if account.id.trim().is_empty() {
    return error::InvalidAccountSnafu {
      id: account.id.clone(),
      message: "id must be non-empty".to_string(),
    }
    .fail();
  }
  if account.provider.trim().is_empty() {
    return error::InvalidAccountSnafu {
      id: account.id.clone(),
      message: "provider must be non-empty".to_string(),
    }
    .fail();
  }
  for name in account.headers.keys() {
    if !is_token(name) {
      return error::InvalidHeaderNameSnafu { name: name.clone() }.fail();
    }
  }
  Ok(())
}

fn is_token(s: &str) -> bool {
  !s.is_empty()
    && s.bytes().all(|b| {
      matches!(b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+'
            | b'-' | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'
            | b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z')
    })
}

fn is_proxy_host(s: &str) -> bool {
  let trimmed = s.trim();
  !trimmed.is_empty()
    && trimmed
      .bytes()
      .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'.' | b'-' | b'*'))
}

pub fn project_dirs() -> Result<ProjectDirs> {
  ProjectDirs::from("dev", "llm-router", "llm-router").ok_or(Error::NoProjectDirs)
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
  let tmp = path.with_extension("toml.tmp");
  std::fs::write(&tmp, contents).context(error::WriteSnafu { path: tmp.clone() })?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let perm = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(&tmp, perm).context(error::SetPermissionsSnafu { path: tmp.clone() })?;
  }
  std::fs::rename(&tmp, path).context(error::RenameSnafu {
    from: tmp.clone(),
    to: path.to_path_buf(),
  })?;
  Ok(())
}
