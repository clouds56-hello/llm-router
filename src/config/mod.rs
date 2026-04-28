use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use snafu::ResultExt;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod error;
pub mod paths;

pub use error::{Error, Result};

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
  github_token: Option<crate::util::secret::Secret<String>>,
  #[serde(default)]
  api_token: Option<crate::util::secret::Secret<String>>,
  #[serde(default)]
  api_token_expires_at: Option<i64>,
  #[serde(default)]
  api_key: Option<crate::util::secret::Secret<String>>,
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
  1800 // 30 min
}
fn default_session_tombstone() -> u64 {
  7200 // 2 h
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
///
/// Resolution rules:
/// - If `url` is set, every outbound request goes through it (subject to `no_proxy`).
/// - Else if `system` is true, defer to reqwest's env-var auto-detection
///   (`HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY`).
/// - Else: explicitly disable any ambient env-var proxy (predictable default).
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
}

/// Logging / tracing settings.
///
/// Resolution precedence (high → low): CLI flag > env var > this config > defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
  /// `tracing-subscriber::EnvFilter` directive (e.g. `info,llm_router=debug`).
  /// Overridden by `RUST_LOG` if set.
  #[serde(default = "default_log_level")]
  pub level: String,
  /// Output format for log records.
  #[serde(default)]
  pub format: LogFormat,
  /// Where to write logs.
  #[serde(default)]
  pub target: LogTarget,
  /// Override directory for the rotating file sink.
  /// Defaults to `<state-dir>/logs`.
  #[serde(default)]
  pub dir: Option<PathBuf>,
  /// Use ANSI colors on terminal output.
  #[serde(default = "default_true")]
  pub ansi: bool,
  /// Emit span open/close events (verbose).
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
  /// Both stderr and a rotating file (recommended default).
  #[default]
  Both,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum InitiatorMode {
  /// Auto-classify by inspecting the chat messages array.
  #[default]
  Auto,
  /// Always send X-Initiator: user.
  AlwaysUser,
  /// Always send X-Initiator: agent.
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
  /// Optional persona name (e.g. "copilot", "opencode", "codex", "openclaw").
  /// Header values from the matching profile in `profiles.toml` are merged
  /// in before any explicit fields above. Inbound `X-Behave-As` overrides.
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
          initiator_mode: o.initiator_mode,
          behave_as: o.behave_as.clone().or_else(|| self.behave_as.clone()),
          extra_headers: extra,
        }
      }
    }
  }

  pub fn validate(&self) -> Result<()> {
    for name in self.extra_headers.keys() {
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
    for (label, val) in [
      ("editor_version", &self.editor_version),
      ("editor_plugin_version", &self.editor_plugin_version),
      ("user_agent", &self.user_agent),
      ("copilot_integration_id", &self.copilot_integration_id),
      ("openai_intent", &self.openai_intent),
    ] {
      if val.trim().is_empty() {
        return error::EmptyFieldSnafu { field: label }.fail();
      }
    }
    Ok(())
  }
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
  pub id: String,
  /// Provider id, e.g. "github-copilot". Defaults to "github-copilot" for backward compat.
  #[serde(default = "default_provider")]
  pub provider: String,
  /// GitHub OAuth token (github-copilot provider only).
  #[serde(default)]
  pub github_token: Option<crate::util::secret::Secret<String>>,
  /// Cached short-lived Copilot API token; written back by the daemon.
  #[serde(default)]
  pub api_token: Option<crate::util::secret::Secret<String>>,
  /// Unix seconds when `api_token` expires.
  #[serde(default)]
  pub api_token_expires_at: Option<i64>,
  /// Static long-lived API key (zai/zhipuai providers).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub api_key: Option<crate::util::secret::Secret<String>>,
  /// Optional per-account header overrides (github-copilot provider only).
  #[serde(default)]
  pub copilot: Option<CopilotHeaders>,
  /// Optional Z.ai-specific account config.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub zai: Option<ZaiAccountConfig>,
  /// Per-account persona override. Wins over `[copilot] behave_as`. Inbound
  /// `X-Behave-As` still overrides this.
  #[serde(default)]
  pub behave_as: Option<String>,
}

/// Z.ai-specific per-account knobs.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ZaiAccountConfig {
  /// Override the upstream base URL. Defaults to
  /// `https://api.z.ai/api/coding/paas/v4`. Use
  /// `https://open.bigmodel.cn/api/paas/v4` for the China-mainland endpoint.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub base_url: Option<String>,
}

fn default_provider() -> String {
  DEFAULT_PROVIDER.to_string()
}

/// Materialise a `CopilotHeaders` from its raw (sparse) form, applying a
/// persona profile overlay for any field the user did not set explicitly.
///
/// Precedence (low -> high):
///   compile-time defaults  ->  persona overlay  ->  user explicit raw fields.
fn resolve_copilot(
  raw: &CopilotHeadersRaw,
  profiles: &crate::provider::profiles::Profiles,
  upstream: &str,
  inherited_persona: Option<&str>,
) -> CopilotHeaders {
  let mut out = CopilotHeaders::default();

  // Carry forward initiator_mode and behave_as up front (they don't come
  // from the profile registry).
  if let Some(m) = raw.initiator_mode {
    out.initiator_mode = m;
  }
  out.behave_as = raw.behave_as.clone().or_else(|| inherited_persona.map(str::to_string));

  let persona = out.behave_as.clone();
  if let Some(persona_name) = persona.as_deref() {
    if let Some(resolved) = profiles.resolve(persona_name, upstream) {
      crate::provider::profiles::warn_if_unverified(persona_name, upstream, &resolved);
      for (name, val) in &resolved.headers {
        match name.as_str() {
          "editor-version" => {
            if raw.editor_version.is_none() {
              out.editor_version = val.clone();
            }
          }
          "editor-plugin-version" => {
            if raw.editor_plugin_version.is_none() {
              out.editor_plugin_version = val.clone();
            }
          }
          "user-agent" => {
            if raw.user_agent.is_none() {
              out.user_agent = val.clone();
            }
          }
          "copilot-integration-id" => {
            if raw.copilot_integration_id.is_none() {
              out.copilot_integration_id = val.clone();
            }
          }
          "openai-intent" => {
            if raw.openai_intent.is_none() {
              out.openai_intent = val.clone();
            }
          }
          other => {
            // Unknown wire-name -> contribute as an extra header
            // (user extra_headers still win below).
            out.extra_headers.insert(other.to_string(), val.clone());
          }
        }
      }
    } else {
      tracing::warn!(persona = %persona_name, "unknown persona; ignoring behave_as");
    }
  }

  // User-explicit fields override both defaults and persona overlay.
  if let Some(v) = &raw.editor_version {
    out.editor_version = v.clone();
  }
  if let Some(v) = &raw.editor_plugin_version {
    out.editor_plugin_version = v.clone();
  }
  if let Some(v) = &raw.user_agent {
    out.user_agent = v.clone();
  }
  if let Some(v) = &raw.copilot_integration_id {
    out.copilot_integration_id = v.clone();
  }
  if let Some(v) = &raw.openai_intent {
    out.openai_intent = v.clone();
  }
  if let Some(extra) = &raw.extra_headers {
    for (k, v) in extra {
      out.extra_headers.insert(k.clone(), v.clone());
    }
  }
  out
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

    // For now we only know one upstream id; once we add more providers the
    // resolution will become per-account using `account.provider`.
    let upstream = crate::provider::ID_GITHUB_COPILOT;
    let profiles = crate::provider::profiles::Profiles::global();

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

    cfg.proxy.validate()?;
    cfg.copilot.validate()?;
    for a in &cfg.accounts {
      if let Some(h) = &a.copilot {
        h.validate().map_err(|e| Error::AccountOverride {
          id: a.id.clone(),
          source: Box::new(e),
        })?;
      }
      if a.provider == DEFAULT_PROVIDER && a.github_token.is_none() {
        return error::MissingGithubTokenSnafu { id: a.id.clone() }.fail();
      }
      if crate::provider::ZAI_ALIASES.contains(&a.provider.as_str())
        && a.api_key.as_ref().map(|s| s.expose().trim()).unwrap_or("").is_empty()
      {
        return error::MissingApiKeySnafu {
          id: a.id.clone(),
          provider: a.provider.clone(),
        }
        .fail();
      }
    }
    tracing::debug!(
      path = %path.display(),
      accounts = cfg.accounts.len(),
      "config loaded"
    );
    Ok((cfg, path))
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

  /// Apply an in-place edit to the config TOML, preserving user comments and
  /// formatting. The closure receives a mutable `toml_edit::DocumentMut`
  /// loaded from disk (or empty if the file does not exist). After the
  /// closure returns successfully the document is validated by parsing it
  /// back through `Config::load`-style validation, then written atomically.
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
    // Validate by round-tripping through serde + our validators.
    let cfg: Config = toml::from_str(&serialised).context(error::EditValidateSnafu)?;
    cfg.proxy.validate().map_err(|e| Error::EditValidateSection {
      section: "[proxy]",
      source: Box::new(e),
    })?;
    cfg.copilot.validate().map_err(|e| Error::EditValidateSection {
      section: "[copilot]",
      source: Box::new(e),
    })?;
    for a in &cfg.accounts {
      if let Some(h) = &a.copilot {
        h.validate().map_err(|e| Error::AccountOverride {
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
