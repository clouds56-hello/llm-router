//! Persona profile loader and resolver.
//!
//! A "profile" (a.k.a. persona) is a named bundle of upstream HTTP identity
//! headers. Profiles are organised as `[<persona>.<scope>]` TOML tables where
//! `<scope>` is either `default`, `general`, or an upstream provider id such
//! as `github-copilot`.
//!
//! Built-in profiles are embedded at compile time via `include_str!`. Users
//! may extend or override them by creating
//! `~/.config/llm-router/profiles.toml`. On a (persona, upstream, header) key
//! collision the user file wins.
//!
//! Use `Profiles::global()` to obtain the lazily-initialized merged registry,
//! and `Profiles::resolve(persona, upstream)` to compute the ordered list of
//! `(header_name, value)` pairs to apply for a given outbound request.

use super::{error, Result};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const EMBEDDED: &str = include_str!("profiles.toml");

/// One scoped section of a profile, e.g. `[opencode.github-copilot]`.
#[derive(Debug, Clone, Default)]
pub struct ProfileSection {
  pub verified: bool,
  /// Header name (wire-form, lowercase) -> value.
  pub headers: BTreeMap<String, String>,
}

/// Resolved overlay for a single (persona, upstream) pair.
#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
  /// `false` if any contributing section was marked `verified = false`.
  pub verified: bool,
  /// Final header set in deterministic order.
  pub headers: BTreeMap<String, String>,
  /// Which scopes were applied, for diagnostics.
  pub scopes_used: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Profiles {
  /// persona -> scope -> section
  table: BTreeMap<String, BTreeMap<String, ProfileSection>>,
}

static GLOBAL: OnceLock<Profiles> = OnceLock::new();
static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

impl Profiles {
  /// Lazily-initialised, process-wide merged registry. Built-in + user file.
  /// Errors during parsing of the user file are logged and ignored — we keep
  /// the embedded set rather than crashing the daemon.
  pub fn global() -> &'static Profiles {
    GLOBAL.get_or_init(|| match Self::load() {
      Ok(p) => p,
      Err(e) => {
        tracing::warn!(error = %e, "failed to load profiles; using embedded only");
        Self::parse(EMBEDDED).unwrap_or_default()
      }
    })
  }

  fn load() -> Result<Self> {
    let mut p = Self::parse(EMBEDDED).map_err(|e| error::Error::Profiles {
      message: format!("parse embedded profiles.toml: {e}"),
    })?;
    if let Some(path) = user_profiles_path() {
      if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|e| error::Error::Profiles {
          message: format!("read {}: {e}", path.display()),
        })?;
        let user = Self::parse(&raw).map_err(|e| error::Error::Profiles {
          message: format!("parse {}: {e}", path.display()),
        })?;
        p.merge(user);
      }
    }
    Ok(p)
  }

  pub fn parse(raw: &str) -> Result<Self> {
    let v: toml::Value = toml::from_str(raw).map_err(|e| error::Error::Profiles {
      message: format!("invalid profiles TOML: {e}"),
    })?;
    let map = v.as_table().ok_or_else(|| error::Error::Profiles {
      message: "expected top-level table".into(),
    })?;
    let mut out = Profiles::default();
    for (persona, scopes) in map {
      let scopes = scopes.as_table().ok_or_else(|| error::Error::Profiles {
        message: format!("[{persona}] must be a table"),
      })?;
      for (scope, section) in scopes {
        let tbl = section.as_table().ok_or_else(|| error::Error::Profiles {
          message: format!("[{persona}.{scope}] must be a table"),
        })?;
        let mut sec = ProfileSection::default();
        for (k, v) in tbl {
          if k == "verified" {
            sec.verified = v.as_bool().unwrap_or(false);
            continue;
          }
          let val = v.as_str().ok_or_else(|| error::Error::Profiles {
            message: format!("[{persona}.{scope}].{k} must be a string"),
          })?;
          sec.headers.insert(k.to_ascii_lowercase(), val.to_string());
        }
        out.table.entry(persona.clone()).or_default().insert(scope.clone(), sec);
      }
    }
    Ok(out)
  }

  /// Merge `other` into `self`; `other` wins on collisions.
  fn merge(&mut self, other: Profiles) {
    for (persona, scopes) in other.table {
      let dst = self.table.entry(persona).or_default();
      for (scope, sec) in scopes {
        let entry = dst.entry(scope).or_default();
        // verified flag: AND so any unverified contributor downgrades.
        entry.verified = entry.verified && sec.verified;
        for (k, v) in sec.headers {
          entry.headers.insert(k, v);
        }
      }
    }
  }

  /// Returns sorted list of known persona names.
  pub fn personas(&self) -> Vec<(&str, bool)> {
    self
      .table
      .iter()
      .map(|(name, scopes)| {
        let verified = scopes.get("default").map(|s| s.verified).unwrap_or(false);
        (name.as_str(), verified)
      })
      .collect()
  }

  /// Resolve overlay for `persona` when sending to `upstream`. Returns
  /// `None` if `persona` is unknown.
  pub fn resolve(&self, persona: &str, upstream: &str) -> Option<ResolvedProfile> {
    let scopes = self.table.get(persona)?;
    let mut out = ResolvedProfile {
      verified: true,
      ..Default::default()
    };

    if let Some(sec) = scopes.get("default") {
      apply(&mut out, "default", sec);
    }
    if let Some(sec) = scopes.get(upstream) {
      apply(&mut out, upstream, sec);
    } else if let Some(sec) = scopes.get("general") {
      apply(&mut out, "general", sec);
    }
    Some(out)
  }
}

fn apply(out: &mut ResolvedProfile, scope: &str, sec: &ProfileSection) {
  out.verified = out.verified && sec.verified;
  out.scopes_used.push(scope.to_string());
  for (k, v) in &sec.headers {
    out.headers.insert(k.clone(), v.clone());
  }
}

pub fn user_profiles_path() -> Option<PathBuf> {
  crate::config::project_dirs()
    .ok()
    .map(|d| d.config_dir().join("profiles.toml"))
}

/// Emit a `tracing::warn!` once per (persona, upstream) pair if the resolved
/// profile is unverified.
pub fn warn_if_unverified(persona: &str, upstream: &str, resolved: &ResolvedProfile) {
  if resolved.verified {
    return;
  }
  let key = format!("{persona}|{upstream}");
  let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
  let mut g = set.lock().unwrap();
  if g.insert(key) {
    tracing::warn!(
        persona,
        upstream,
        scopes = ?resolved.scopes_used,
        "using unverified persona profile; header values may be wrong"
    );
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn embedded_parses() {
    let p = Profiles::parse(EMBEDDED).unwrap();
    assert!(p.table.contains_key("copilot"));
    assert!(p.table.contains_key("opencode"));
  }

  #[test]
  fn copilot_default_verified() {
    let p = Profiles::parse(EMBEDDED).unwrap();
    let r = p.resolve("copilot", "github-copilot").unwrap();
    assert!(r.verified);
    assert_eq!(
      r.headers.get("user-agent").map(String::as_str),
      Some("GitHubCopilotChat/0.20.0")
    );
  }

  #[test]
  fn opencode_uses_upstream_section() {
    let p = Profiles::parse(EMBEDDED).unwrap();
    let r = p.resolve("opencode", "github-copilot").unwrap();
    assert!(!r.verified);
    assert!(r.scopes_used.contains(&"github-copilot".to_string()));
    assert!(r.headers.contains_key("editor-version"));
  }

  #[test]
  fn opencode_falls_back_to_general() {
    let p = Profiles::parse(EMBEDDED).unwrap();
    let r = p.resolve("opencode", "no-such-upstream").unwrap();
    assert!(r.scopes_used.contains(&"general".to_string()));
  }

  #[test]
  fn unknown_persona_is_none() {
    let p = Profiles::parse(EMBEDDED).unwrap();
    assert!(p.resolve("does-not-exist", "github-copilot").is_none());
  }

  #[test]
  fn user_overrides_embedded() {
    let mut p = Profiles::parse(EMBEDDED).unwrap();
    let user = Profiles::parse(
      r#"
[copilot.default]
verified = true
user-agent = "custom/9.9"
"#,
    )
    .unwrap();
    p.merge(user);
    let r = p.resolve("copilot", "github-copilot").unwrap();
    assert_eq!(r.headers.get("user-agent").map(String::as_str), Some("custom/9.9"));
  }
}
