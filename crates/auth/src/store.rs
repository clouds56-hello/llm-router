//! `auth.yaml` storage + back-compat shim for `config.toml`'s legacy
//! `[[accounts]]` block.
//!
//! Format (`auth.yaml`):
//!
//! ```yaml
//! version: 1
//! accounts:
//!   - id: clouds56-bot
//!     provider: github-copilot
//!     enabled: true
//!     tier: active
//!     refresh_token: ghu_xxx
//!     access_token: tid_xxx
//!     access_token_expires_at: 1234567890
//!     last_refresh: 1234567890
//!     settings: {}
//! ```
//!
//! Resolution order on load:
//!  1. `auth.yaml` if present → parsed and returned.
//!  2. else if `config.toml` contains `[[accounts]]` → migrated in
//!     memory, written to `auth.yaml` immediately, deprecation warning
//!     logged.
//!  3. else → empty store.
//!
//! Saves *always* go to `auth.yaml`. The legacy block in `config.toml`
//! is left untouched so the operator can remove it manually after they
//! verify the new file. The next loader pass will ignore it (yaml wins).

use anyhow::{Context, Result};
use tokn_core::account::AccountConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const CURRENT_VERSION: u32 = 1;
const AUTH_FILE_NAME: &str = "auth.yaml";

/// On-disk schema. Future versions can introduce new top-level keys; the
/// `version` field is mandatory so we can detect format upgrades.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct AuthFile {
  #[serde(default = "default_version")]
  version: u32,
  #[serde(default)]
  accounts: Vec<AccountConfig>,
}

fn default_version() -> u32 {
  CURRENT_VERSION
}

/// In-memory account store backed by `auth.yaml`.
///
/// `path` is the absolute path the store will save to. It is captured at
/// load-time so subsequent saves don't need to re-resolve `XDG_CONFIG_HOME`.
#[derive(Debug, Clone)]
pub struct AuthStore {
  path: PathBuf,
  pub accounts: Vec<AccountConfig>,
}

impl AuthStore {
  /// Load `auth.yaml` from the given path, falling back to the legacy
  /// `accounts = [...]` table in `config.toml` if `auth.yaml` is missing.
  ///
  /// `auth_path` overrides the default `~/.config/tokn-router/auth.yaml`.
  /// `config_path` is consulted only for the migration fallback; pass the
  /// effective config.toml path. When the legacy fallback fires, this
  /// function writes the migrated `auth.yaml` to disk and emits a
  /// `tracing::warn!`.
  pub fn load(auth_path: Option<&Path>, config_path: Option<&Path>) -> Result<Self> {
    let resolved = auth_path.map(PathBuf::from).unwrap_or_else(default_auth_path);

    if resolved.exists() {
      return load_from_yaml(&resolved);
    }

    // Yaml missing — try the legacy block in config.toml.
    if let Some(cfg_path) = config_path {
      if let Some(legacy) = load_legacy_accounts(cfg_path)? {
        let store = AuthStore {
          path: resolved.clone(),
          accounts: legacy,
        };
        // Materialise immediately so subsequent runs go straight to the
        // fast path. Failures here are non-fatal; we still return the
        // in-memory store and let the user retry on the next mutation.
        if let Err(e) = store.save() {
          tracing::warn!(error = %e, "failed to write migrated auth.yaml");
        } else {
          tracing::warn!(
            count = store.accounts.len(),
            from = %cfg_path.display(),
            to = %resolved.display(),
            "migrated accounts from config.toml to auth.yaml; remove the [[accounts]] table from config.toml"
          );
        }
        return Ok(store);
      }
    }

    // Nothing to load — empty store rooted at the resolved path so a
    // future `save()` creates the file.
    Ok(AuthStore {
      path: resolved,
      accounts: Vec::new(),
    })
  }

  /// Persist the current state to `auth.yaml`. Creates parent directories
  /// as needed; writes with mode 0600 on Unix to keep tokens off prying
  /// eyes.
  pub fn save(&self) -> Result<()> {
    if let Some(parent) = self.path.parent() {
      std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let file = AuthFile {
      version: CURRENT_VERSION,
      accounts: self.accounts.clone(),
    };
    let yaml = serde_yaml::to_string(&file).with_context(|| "serialising auth.yaml")?;
    write_secured(&self.path, yaml.as_bytes()).with_context(|| format!("writing {}", self.path.display()))
  }

  /// Insert or replace an account by id.
  pub fn upsert(&mut self, account: AccountConfig) {
    if let Some(slot) = self.accounts.iter_mut().find(|a| a.id == account.id) {
      *slot = account;
    } else {
      self.accounts.push(account);
    }
  }

  /// Remove the account with the given id, returning the removed value if
  /// any.
  pub fn remove(&mut self, id: &str) -> Option<AccountConfig> {
    let idx = self.accounts.iter().position(|a| a.id == id)?;
    Some(self.accounts.remove(idx))
  }

  /// Borrow the account with the given id, if any.
  pub fn get(&self, id: &str) -> Option<&AccountConfig> {
    self.accounts.iter().find(|a| a.id == id)
  }

  /// Mutably borrow the account with the given id, if any.
  pub fn get_mut(&mut self, id: &str) -> Option<&mut AccountConfig> {
    self.accounts.iter_mut().find(|a| a.id == id)
  }

  /// The path this store will save to.
  pub fn path(&self) -> &Path {
    &self.path
  }
}

fn load_from_yaml(path: &Path) -> Result<AuthStore> {
  let raw = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
  let parsed: AuthFile =
    serde_yaml::from_str(&raw).with_context(|| format!("parsing {} (expected `version: 1` schema)", path.display()))?;
  if parsed.version != CURRENT_VERSION {
    anyhow::bail!(
      "{}: unsupported version {} (this build understands {})",
      path.display(),
      parsed.version,
      CURRENT_VERSION
    );
  }
  Ok(AuthStore {
    path: path.to_path_buf(),
    accounts: parsed.accounts,
  })
}

/// Read just the `accounts = [...]` array from `config.toml` without
/// re-implementing the full schema. Returns `None` if the file is absent
/// or has no accounts; `Some(vec![])` is also `None` for our purposes.
fn load_legacy_accounts(config_path: &Path) -> Result<Option<Vec<AccountConfig>>> {
  if !config_path.exists() {
    return Ok(None);
  }
  let raw = std::fs::read_to_string(config_path).with_context(|| format!("reading {}", config_path.display()))?;
  // Minimal local schema: accounts have moved to auth.yaml, so we can no
  // longer go through `tokn_config::Config`. We only care about the legacy
  // `[[accounts]]` table here; everything else in config.toml is ignored.
  #[derive(serde::Deserialize)]
  struct LegacyAccounts {
    #[serde(default)]
    accounts: Vec<AccountConfig>,
  }
  let parsed: LegacyAccounts =
    toml::from_str(&raw).with_context(|| format!("parsing legacy {}", config_path.display()))?;
  if parsed.accounts.is_empty() {
    Ok(None)
  } else {
    Ok(Some(parsed.accounts))
  }
}

/// Default path: `~/.config/tokn-router/auth.yaml` (or the platform
/// equivalent via `directories::ProjectDirs`).
pub fn default_auth_path() -> PathBuf {
  if let Some(dirs) = directories::ProjectDirs::from("", "", "tokn-router") {
    dirs.config_dir().join(AUTH_FILE_NAME)
  } else {
    PathBuf::from(AUTH_FILE_NAME)
  }
}

#[cfg(unix)]
fn write_secured(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
  use std::io::Write;
  use std::os::unix::fs::OpenOptionsExt;
  let mut f = std::fs::OpenOptions::new()
    .create(true)
    .truncate(true)
    .write(true)
    .mode(0o600)
    .open(path)?;
  f.write_all(bytes)?;
  Ok(())
}

#[cfg(not(unix))]
fn write_secured(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
  std::fs::write(path, bytes)
}

#[cfg(test)]
mod tests {
  use super::*;
  use tokn_core::account::{AccountTier, AuthType};

  fn sample_account(id: &str) -> AccountConfig {
    AccountConfig {
      id: id.into(),
      provider: "github-copilot".into(),
      enabled: true,
      tier: AccountTier::Active,
      tags: vec![],
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: None,
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  #[test]
  fn roundtrip_yaml_preserves_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.yaml");
    let store = AuthStore {
      path: path.clone(),
      accounts: vec![sample_account("a1"), sample_account("a2")],
    };
    store.save().unwrap();
    let loaded = AuthStore::load(Some(&path), None).unwrap();
    assert_eq!(loaded.accounts.len(), 2);
    assert_eq!(loaded.accounts[0].id, "a1");
    assert_eq!(loaded.accounts[1].id, "a2");
  }

  #[test]
  fn upsert_replaces_by_id() {
    let mut store = AuthStore {
      path: PathBuf::from("/dev/null"),
      accounts: vec![sample_account("a1")],
    };
    let mut updated = sample_account("a1");
    updated.label = Some("renamed".into());
    store.upsert(updated);
    assert_eq!(store.accounts.len(), 1);
    assert_eq!(store.accounts[0].label.as_deref(), Some("renamed"));
  }

  #[test]
  fn remove_returns_extracted_account() {
    let mut store = AuthStore {
      path: PathBuf::from("/dev/null"),
      accounts: vec![sample_account("a1"), sample_account("a2")],
    };
    let popped = store.remove("a1").unwrap();
    assert_eq!(popped.id, "a1");
    assert_eq!(store.accounts.len(), 1);
    assert!(store.remove("ghost").is_none());
  }

  #[test]
  fn missing_yaml_with_legacy_config_migrates() {
    let dir = tempfile::tempdir().unwrap();
    let yaml_path = dir.path().join("auth.yaml");
    let cfg_path = dir.path().join("config.toml");
    // Minimal config.toml with one legacy account.
    std::fs::write(
      &cfg_path,
      r#"
[[accounts]]
id = "legacy"
provider = "github-copilot"
enabled = true
"#,
    )
    .unwrap();

    let store = AuthStore::load(Some(&yaml_path), Some(&cfg_path)).unwrap();
    assert_eq!(store.accounts.len(), 1);
    assert_eq!(store.accounts[0].id, "legacy");
    // Migration also wrote the yaml file.
    assert!(yaml_path.exists());
    // Subsequent load goes through the fast path.
    let store2 = AuthStore::load(Some(&yaml_path), Some(&cfg_path)).unwrap();
    assert_eq!(store2.accounts.len(), 1);
  }

  #[test]
  fn missing_both_yields_empty_store() {
    let dir = tempfile::tempdir().unwrap();
    let yaml_path = dir.path().join("auth.yaml");
    let store = AuthStore::load(Some(&yaml_path), None).unwrap();
    assert!(store.accounts.is_empty());
  }

  #[test]
  fn unknown_version_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.yaml");
    std::fs::write(&path, "version: 99\naccounts: []\n").unwrap();
    let err = AuthStore::load(Some(&path), None).unwrap_err();
    assert!(err.to_string().contains("unsupported version 99"));
  }
}
