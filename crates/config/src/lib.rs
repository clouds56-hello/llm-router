pub mod error;
pub mod paths;
pub mod profiles;

pub use error::{Error, Result};
pub use llm_core::account::{Account, ZaiAccountConfig};

use directories::ProjectDirs;
use llm_core::provider::{ID_GITHUB_COPILOT, ZAI_ALIASES};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use snafu::ResultExt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_HOST: &str = "127.0.0.1";
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
  pub logging: LoggingConfig,
  #[serde(default)]
  pub copilot: Value,
  #[serde(default)]
  pub accounts: Vec<Account>,
}

/// Sparse view of `[copilot]` used during loading to distinguish
/// user-explicit fields from hardcoded defaults. Any field left as `None` here
/// is eligible to be filled in by a persona profile overlay.
#[derive(Debug, Clone, Default, Deserialize)]
struct CopilotHeadersRaw {
  #[serde(default)]
  editor_version: Option<String>,
  #[serde(default)]
  editor_plugin_version: Option<String>,
  #[serde(default)]
  user_agent: Option<String>,
  #[serde(default)]
  copilot_integration_id: Option<String>,
  #[serde(default)]
  openai_intent: Option<String>,
  #[serde(default)]
  initiator_mode: Option<InitiatorMode>,
  #[serde(default)]
  behave_as: Option<String>,
  #[serde(default)]
  extra_headers: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct AccountRaw {
  id: String,
  #[serde(default = "default_provider")]
  provider: String,
  #[serde(default)]
  github_token: Option<llm_core::util::secret::Secret<String>>,
  #[serde(default)]
  api_token: Option<llm_core::util::secret::Secret<String>>,
  #[serde(default)]
  api_token_expires_at: Option<i64>,
  #[serde(default)]
  api_key: Option<llm_core::util::secret::Secret<String>>,
  #[serde(default)]
  copilot: Option<CopilotHeadersRaw>,
  #[serde(default)]
  zai: Option<ZaiAccountConfig>,
  #[serde(default)]
  behave_as: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigRaw {
  #[serde(default)]
  server: ServerConfig,
  #[serde(default)]
  pool: PoolConfig,
  #[serde(default, alias = "usage")]
  db: DbConfig,
  #[serde(default)]
  proxy: ProxyConfig,
  #[serde(default)]
  logging: LoggingConfig,
  #[serde(default)]
  copilot: CopilotHeadersRaw,
  #[serde(default)]
  accounts: Vec<AccountRaw>,
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

fn default_provider() -> String {
  DEFAULT_PROVIDER.to_string()
}

fn resolve_copilot(
  raw: &CopilotHeadersRaw,
  profiles: &profiles::Profiles,
  upstream: &str,
  inherited_persona: Option<&str>,
) -> Value {
  let mut headers = default_copilot_map();

  if let Some(m) = raw.initiator_mode {
    headers.insert(
      "initiator_mode".into(),
      serde_json::to_value(m).unwrap_or(Value::String("auto".into())),
    );
  }
  let behave_as = raw.behave_as.clone().or_else(|| inherited_persona.map(str::to_string));
  if let Some(persona_name) = behave_as.as_deref() {
    headers.insert("behave_as".into(), Value::String(persona_name.to_string()));
    if let Some(resolved) = profiles.resolve(persona_name, upstream) {
      profiles::warn_if_unverified(persona_name, upstream, &resolved);
      let mut extra = BTreeMap::new();
      for (name, val) in &resolved.headers {
        match name.as_str() {
          "editor-version" if raw.editor_version.is_none() => insert_str(&mut headers, "editor_version", val),
          "editor-plugin-version" if raw.editor_plugin_version.is_none() => {
            insert_str(&mut headers, "editor_plugin_version", val)
          }
          "user-agent" if raw.user_agent.is_none() => insert_str(&mut headers, "user_agent", val),
          "copilot-integration-id" if raw.copilot_integration_id.is_none() => {
            insert_str(&mut headers, "copilot_integration_id", val)
          }
          "openai-intent" if raw.openai_intent.is_none() => insert_str(&mut headers, "openai_intent", val),
          other => {
            extra.insert(other.to_string(), val.clone());
          }
        }
      }
      if !extra.is_empty() {
        headers.insert(
          "extra_headers".into(),
          serde_json::to_value(extra).unwrap_or(Value::Object(Map::new())),
        );
      }
    } else {
      tracing::warn!(persona = %persona_name, "unknown persona; ignoring behave_as");
    }
  }

  if let Some(v) = &raw.editor_version {
    insert_str(&mut headers, "editor_version", v);
  }
  if let Some(v) = &raw.editor_plugin_version {
    insert_str(&mut headers, "editor_plugin_version", v);
  }
  if let Some(v) = &raw.user_agent {
    insert_str(&mut headers, "user_agent", v);
  }
  if let Some(v) = &raw.copilot_integration_id {
    insert_str(&mut headers, "copilot_integration_id", v);
  }
  if let Some(v) = &raw.openai_intent {
    insert_str(&mut headers, "openai_intent", v);
  }
  if let Some(extra) = &raw.extra_headers {
    headers.insert(
      "extra_headers".into(),
      serde_json::to_value(extra).unwrap_or(Value::Object(Map::new())),
    );
  }
  Value::Object(headers)
}

fn default_copilot_map() -> Map<String, Value> {
  let mut out = Map::new();
  insert_str(&mut out, "editor_version", "vscode/1.95.0");
  insert_str(&mut out, "editor_plugin_version", "copilot-chat/0.20.0");
  insert_str(&mut out, "user_agent", "GitHubCopilotChat/0.20.0");
  insert_str(&mut out, "copilot_integration_id", "vscode-chat");
  insert_str(&mut out, "openai_intent", "conversation-panel");
  out.insert("initiator_mode".into(), Value::String("auto".into()));
  out
}

fn insert_str(headers: &mut Map<String, Value>, key: &str, value: &str) {
  headers.insert(key.to_string(), Value::String(value.to_string()));
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
    let upstream = ID_GITHUB_COPILOT;
    let profiles = profiles::Profiles::global();
    let copilot = resolve_copilot(&raw_cfg.copilot, profiles, upstream, None);
    let accounts: Vec<Account> = raw_cfg
      .accounts
      .into_iter()
      .map(|a| {
        let parent_persona = raw_cfg.copilot.behave_as.as_deref();
        let acct_persona = a.behave_as.as_deref().or(parent_persona);
        let acct_copilot = a
          .copilot
          .as_ref()
          .map(|h| resolve_copilot(h, profiles, upstream, acct_persona));
        Account {
          id: a.id,
          provider: a.provider,
          github_token: a.github_token,
          api_token: a.api_token,
          api_token_expires_at: a.api_token_expires_at,
          api_key: a.api_key,
          copilot: acct_copilot,
          zai: a.zai,
          behave_as: a.behave_as,
        }
      })
      .collect();
    let cfg = Config {
      server: raw_cfg.server,
      pool: raw_cfg.pool,
      db: raw_cfg.db,
      proxy: raw_cfg.proxy,
      logging: raw_cfg.logging,
      copilot,
      accounts,
    };
    cfg.validate()?;
    tracing::debug!(path = %path.display(), accounts = cfg.accounts.len(), "config loaded");
    Ok((cfg, path))
  }

  pub fn validate(&self) -> Result<()> {
    self.proxy.validate()?;
    validate_copilot_value(&self.copilot)?;
    for a in &self.accounts {
      if let Some(h) = &a.copilot {
        validate_copilot_value(h).map_err(|e| Error::AccountOverride {
          id: a.id.clone(),
          source: Box::new(e),
        })?;
      }
      if a.provider == DEFAULT_PROVIDER && a.github_token.is_none() {
        return error::MissingGithubTokenSnafu { id: a.id.clone() }.fail();
      }
      if ZAI_ALIASES.contains(&a.provider.as_str())
        && a.api_key.as_ref().map(|s| s.expose().trim()).unwrap_or("").is_empty()
      {
        return error::MissingApiKeySnafu {
          id: a.id.clone(),
          provider: a.provider.clone(),
        }
        .fail();
      }
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
    validate_copilot_value(&cfg.copilot).map_err(|e| Error::EditValidateSection {
      section: "[copilot]",
      source: Box::new(e),
    })?;
    for a in &cfg.accounts {
      if let Some(h) = &a.copilot {
        validate_copilot_value(h).map_err(|e| Error::AccountOverride {
          id: a.id.clone(),
          source: Box::new(e),
        })?;
      }
    }
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent).context(error::CreateDirSnafu {
        path: parent.to_path_buf(),
      })?;
    }
    write_atomic(path, &serialised)
  }

  pub fn upsert_account(&mut self, a: Account) {
    if let Some(slot) = self.accounts.iter_mut().find(|x| x.id == a.id) {
      *slot = a;
    } else {
      self.accounts.push(a);
    }
  }
}

fn validate_copilot_value(value: &Value) -> Result<()> {
  if value.is_null() {
    return Ok(());
  }
  let Some(obj) = value.as_object() else {
    return Ok(());
  };
  if let Some(extra) = obj.get("extra_headers").and_then(Value::as_object) {
    for name in extra.keys() {
      if !is_token(name) {
        return error::InvalidHeaderNameSnafu { name: name.clone() }.fail();
      }
      let lower = name.to_ascii_lowercase();
      if matches!(
        lower.as_str(),
        "authorization" | "host" | "content-length" | "content-type"
      ) {
        return error::ReservedHeaderSnafu { name: name.clone() }.fail();
      }
    }
  }
  for field in [
    "editor_version",
    "editor_plugin_version",
    "user_agent",
    "copilot_integration_id",
    "openai_intent",
  ] {
    if obj
      .get(field)
      .and_then(Value::as_str)
      .map(|s| s.trim().is_empty())
      .unwrap_or(false)
    {
      return error::EmptyFieldSnafu { field }.fail();
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
