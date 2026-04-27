use anyhow::{anyhow, Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub mod paths;

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PROVIDER: &str = "github-copilot";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub pool: PoolConfig,
    #[serde(default)]
    pub usage: UsageConfig,
    #[serde(default)]
    pub proxy: ProxyConfig,
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
    github_token: Option<String>,
    #[serde(default)]
    api_token: Option<String>,
    #[serde(default)]
    api_token_expires_at: Option<i64>,
    #[serde(default)]
    copilot: Option<CopilotHeadersRaw>,
    #[serde(default)]
    behave_as: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ConfigRaw {
    #[serde(default)]
    server: ServerConfig,
    #[serde(default)]
    pool: PoolConfig,
    #[serde(default)]
    usage: UsageConfig,
    #[serde(default)]
    proxy: ProxyConfig,
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
    fn default() -> Self { Self { host: default_host(), port: default_port() } }
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
            let parsed = reqwest::Url::parse(u)
                .with_context(|| format!("[proxy].url is not a valid URL: {u}"))?;
            match parsed.scheme() {
                "http" | "https" | "socks5" | "socks5h" => {}
                other => return Err(anyhow!("[proxy].url has unsupported scheme: {other}")),
            }
        }
        Ok(())
    }
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
                    initiator_mode: o.initiator_mode,
                    behave_as: o.behave_as.clone().or_else(|| self.behave_as.clone()),
                    extra_headers: extra,
                }
            }
        }
    }

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
    /// Provider id, e.g. "github-copilot". Defaults to "github-copilot" for backward compat.
    #[serde(default = "default_provider")]
    pub provider: String,
    /// GitHub OAuth token (github-copilot provider only).
    #[serde(default)]
    pub github_token: Option<String>,
    /// Cached short-lived Copilot API token; written back by the daemon.
    #[serde(default)]
    pub api_token: Option<String>,
    /// Unix seconds when `api_token` expires.
    #[serde(default)]
    pub api_token_expires_at: Option<i64>,
    /// Optional per-account header overrides (github-copilot provider only).
    #[serde(default)]
    pub copilot: Option<CopilotHeaders>,
    /// Per-account persona override. Wins over `[copilot] behave_as`. Inbound
    /// `X-Behave-As` still overrides this.
    #[serde(default)]
    pub behave_as: Option<String>,
}

fn default_provider() -> String { DEFAULT_PROVIDER.to_string() }

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
    if let Some(m) = raw.initiator_mode { out.initiator_mode = m; }
    out.behave_as = raw.behave_as.clone().or_else(|| inherited_persona.map(str::to_string));

    let persona = out.behave_as.clone();
    if let Some(persona_name) = persona.as_deref() {
        if let Some(resolved) = profiles.resolve(persona_name, upstream) {
            crate::provider::profiles::warn_if_unverified(persona_name, upstream, &resolved);
            for (name, val) in &resolved.headers {
                match name.as_str() {
                    "editor-version" => {
                        if raw.editor_version.is_none() { out.editor_version = val.clone(); }
                    }
                    "editor-plugin-version" => {
                        if raw.editor_plugin_version.is_none() {
                            out.editor_plugin_version = val.clone();
                        }
                    }
                    "user-agent" => {
                        if raw.user_agent.is_none() { out.user_agent = val.clone(); }
                    }
                    "copilot-integration-id" => {
                        if raw.copilot_integration_id.is_none() {
                            out.copilot_integration_id = val.clone();
                        }
                    }
                    "openai-intent" => {
                        if raw.openai_intent.is_none() { out.openai_intent = val.clone(); }
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
    if let Some(v) = &raw.editor_version { out.editor_version = v.clone(); }
    if let Some(v) = &raw.editor_plugin_version { out.editor_plugin_version = v.clone(); }
    if let Some(v) = &raw.user_agent { out.user_agent = v.clone(); }
    if let Some(v) = &raw.copilot_integration_id { out.copilot_integration_id = v.clone(); }
    if let Some(v) = &raw.openai_intent { out.openai_intent = v.clone(); }
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
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("read config {}", path.display()))?;
        let raw_cfg: ConfigRaw = toml::from_str(&raw)
            .with_context(|| format!("parse config {}", path.display()))?;

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
                let acct_copilot = a.copilot.as_ref().map(|h| {
                    resolve_copilot(h, profiles, upstream, acct_persona)
                });
                Account {
                    id: a.id,
                    provider: a.provider,
                    github_token: a.github_token,
                    api_token: a.api_token,
                    api_token_expires_at: a.api_token_expires_at,
                    copilot: acct_copilot,
                    behave_as: a.behave_as,
                }
            })
            .collect();

        let cfg = Config {
            server: raw_cfg.server,
            pool: raw_cfg.pool,
            usage: raw_cfg.usage,
            proxy: raw_cfg.proxy,
            copilot,
            accounts,
        };

        cfg.proxy.validate()?;
        cfg.copilot.validate()?;
        for a in &cfg.accounts {
            if let Some(h) = &a.copilot {
                h.validate()
                    .with_context(|| format!("account {}: invalid [copilot] override", a.id))?;
            }
            if a.provider == DEFAULT_PROVIDER && a.github_token.is_none() {
                return Err(anyhow!(
                    "account '{}': provider 'github-copilot' requires `github_token`",
                    a.id
                ));
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
        write_atomic(path, &toml)
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
            std::fs::read_to_string(path)
                .with_context(|| format!("read config {}", path.display()))?
        } else {
            String::new()
        };
        let mut doc: toml_edit::DocumentMut = raw
            .parse()
            .with_context(|| format!("parse config {}", path.display()))?;
        f(&mut doc)?;
        let serialised = doc.to_string();
        // Validate by round-tripping through serde + our validators.
        let cfg: Config = toml::from_str(&serialised)
            .context("validation failed: edited config no longer parses")?;
        cfg.proxy.validate().context("validation failed: [proxy]")?;
        cfg.copilot.validate().context("validation failed: [copilot]")?;
        for a in &cfg.accounts {
            if let Some(h) = &a.copilot {
                h.validate()
                    .with_context(|| format!("validation failed: account '{}'", a.id))?;
            }
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
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
    ProjectDirs::from("dev", "llm-router", "llm-router")
        .ok_or_else(|| anyhow!("could not resolve XDG project dirs"))
}

fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)
        .with_context(|| format!("write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(&tmp, perm)?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}
