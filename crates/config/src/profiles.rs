//! Persona profile loader and resolver.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const EMBEDDED: &str = include_str!("profiles.toml");

#[derive(Debug, Clone, Default)]
pub struct ProfileSection {
  pub verified: bool,
  pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
  pub verified: bool,
  pub headers: BTreeMap<String, String>,
  pub scopes_used: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Profiles {
  table: BTreeMap<String, BTreeMap<String, ProfileSection>>,
}

static GLOBAL: OnceLock<Profiles> = OnceLock::new();
static WARNED: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

impl Profiles {
  pub fn global() -> &'static Profiles {
    GLOBAL.get_or_init(|| match Self::load() {
      Ok(p) => {
        tracing::debug!(personas = p.table.len(), "profiles registry loaded");
        p
      }
      Err(e) => {
        tracing::warn!(error = %e, "failed to load profiles; using embedded only");
        Self::parse(EMBEDDED).unwrap_or_default()
      }
    })
  }

  fn load() -> Result<Self, String> {
    let mut p = Self::parse(EMBEDDED).map_err(|e| format!("parse embedded profiles.toml: {e}"))?;
    if let Some(path) = user_profiles_path() {
      if path.exists() {
        let raw = std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let user = Self::parse(&raw).map_err(|e| format!("parse {}: {e}", path.display()))?;
        p.merge(user);
      }
    }
    Ok(p)
  }

  pub fn parse(raw: &str) -> Result<Self, String> {
    let v: toml::Value = toml::from_str(raw).map_err(|e| format!("invalid profiles TOML: {e}"))?;
    let map = v.as_table().ok_or_else(|| "expected top-level table".to_string())?;
    let mut out = Profiles::default();
    for (persona, scopes) in map {
      let scopes = scopes
        .as_table()
        .ok_or_else(|| format!("[{persona}] must be a table"))?;
      for (scope, section) in scopes {
        let tbl = section
          .as_table()
          .ok_or_else(|| format!("[{persona}.{scope}] must be a table"))?;
        let mut sec = ProfileSection::default();
        for (k, v) in tbl {
          if k == "verified" {
            sec.verified = v.as_bool().unwrap_or(false);
            continue;
          }
          let val = v
            .as_str()
            .ok_or_else(|| format!("[{persona}.{scope}].{k} must be a string"))?;
          sec.headers.insert(k.to_ascii_lowercase(), val.to_string());
        }
        out.table.entry(persona.clone()).or_default().insert(scope.clone(), sec);
      }
    }
    Ok(out)
  }

  fn merge(&mut self, other: Profiles) {
    for (persona, scopes) in other.table {
      let dst = self.table.entry(persona).or_default();
      for (scope, sec) in scopes {
        let entry = dst.entry(scope).or_default();
        entry.verified = entry.verified && sec.verified;
        for (k, v) in sec.headers {
          entry.headers.insert(k, v);
        }
      }
    }
  }

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
  directories::ProjectDirs::from("dev", "llm-router", "llm-router").map(|d| d.config_dir().join("profiles.toml"))
}

pub fn warn_if_unverified(persona: &str, upstream: &str, resolved: &ResolvedProfile) {
  if resolved.verified {
    return;
  }
  let key = format!("{persona}|{upstream}");
  let set = WARNED.get_or_init(|| Mutex::new(HashSet::new()));
  let mut g = set.lock().unwrap();
  if g.insert(key) {
    tracing::warn!(persona, upstream, scopes = ?resolved.scopes_used, "using unverified persona profile; header values may be wrong");
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
}
