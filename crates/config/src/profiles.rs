//! Persona profile loader and resolver.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

const EMBEDDED: &str = include_str!("profiles.toml");

const PERSONA_KEYS: &[&str] = &["verified", "forward", "deny"];

pub const ROUTER_CONTROLLED_HEADERS: &[&str] = &[
  "accept",
  "accept-encoding",
  "authorization",
  "connection",
  "content-length",
  "content-type",
  "host",
  "te",
  "transfer-encoding",
];

#[derive(Debug, Clone, Default)]
pub struct ProfileSection {
  pub verified: bool,
  pub headers: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct PersonaProfile {
  pub verified: bool,
  pub forward: BTreeSet<String>,
  pub deny: BTreeSet<String>,
  pub scopes: BTreeMap<String, ProfileSection>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedProfile {
  pub verified: bool,
  pub headers: BTreeMap<String, String>,
  pub forward: BTreeSet<String>,
  pub deny: BTreeSet<String>,
  pub scopes_used: Vec<String>,
}

#[derive(Debug, Default)]
pub struct Profiles {
  table: BTreeMap<String, PersonaProfile>,
}

pub use tokn_headers::TemplateVars;

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
    for (persona, persona_value) in map {
      let persona_tbl = persona_value
        .as_table()
        .ok_or_else(|| format!("[{persona}] must be a table"))?;
      let mut profile = PersonaProfile {
        verified: persona_tbl.get("verified").and_then(|v| v.as_bool()).unwrap_or(true),
        forward: parse_header_list(persona_tbl.get("forward"), &format!("[{persona}].forward"))?,
        deny: default_denied_headers(),
        scopes: BTreeMap::new(),
      };
      profile.deny.extend(parse_header_list(
        persona_tbl.get("deny"),
        &format!("[{persona}].deny"),
      )?);

      for (scope, section_value) in persona_tbl {
        if PERSONA_KEYS.contains(&scope.as_str()) {
          continue;
        }
        let section_tbl = section_value
          .as_table()
          .ok_or_else(|| format!("[{persona}.{scope}] must be a table"))?;
        let mut section = ProfileSection {
          verified: section_tbl.get("verified").and_then(|v| v.as_bool()).unwrap_or(true),
          headers: BTreeMap::new(),
        };
        for (key, value) in section_tbl {
          if key == "verified" {
            continue;
          }
          let key = normalize_header_name(key);
          if is_router_controlled(&key) {
            tracing::warn!(persona, scope, header = %key, "router-controlled persona header ignored");
            continue;
          }
          let val = value
            .as_str()
            .ok_or_else(|| format!("[{persona}.{scope}].{key} must be a string"))?;
          validate_template(val, &format!("[{persona}.{scope}].{key}"))?;
          section.headers.insert(key, resolve_static_template(val)?);
        }
        profile.verified = profile.verified && section.verified;
        profile.scopes.insert(scope.clone(), section);
      }
      out.table.insert(persona.clone(), profile);
    }
    Ok(out)
  }

  fn merge(&mut self, other: Profiles) {
    for (persona, profile) in other.table {
      let dst = self.table.entry(persona).or_insert_with(|| PersonaProfile {
        verified: true,
        deny: default_denied_headers(),
        ..Default::default()
      });
      dst.verified = dst.verified && profile.verified;
      dst.forward.extend(profile.forward);
      dst.deny.extend(profile.deny);
      for (scope, sec) in profile.scopes {
        let entry = dst.scopes.entry(scope).or_default();
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
      .map(|(name, profile)| (name.as_str(), profile.verified))
      .collect()
  }

  pub fn resolve(&self, persona: &str, upstream: &str) -> Option<ResolvedProfile> {
    let profile = self.table.get(persona)?;
    let mut out = ResolvedProfile {
      verified: profile.verified,
      forward: profile.forward.clone(),
      deny: profile.deny.clone(),
      ..Default::default()
    };
    if let Some(sec) = profile.scopes.get("default") {
      apply(&mut out, "default", sec);
    }
    if let Some(sec) = profile.scopes.get(upstream) {
      apply(&mut out, upstream, sec);
    } else if let Some(sec) = profile.scopes.get("general") {
      apply(&mut out, "general", sec);
    }
    Some(out)
  }
}

impl ResolvedProfile {
  pub fn render_headers(&self, vars: &TemplateVars) -> BTreeMap<String, String> {
    self
      .headers
      .iter()
      .filter_map(|(k, v)| render_request_template(v, vars).map(|v| (k.clone(), v)))
      .collect()
  }
}

fn apply(out: &mut ResolvedProfile, scope: &str, sec: &ProfileSection) {
  out.verified = out.verified && sec.verified;
  out.scopes_used.push(scope.to_string());
  for (k, v) in &sec.headers {
    out.headers.insert(k.clone(), v.clone());
  }
}

fn parse_header_list(value: Option<&toml::Value>, what: &str) -> Result<BTreeSet<String>, String> {
  let Some(value) = value else {
    return Ok(BTreeSet::new());
  };
  let arr = value
    .as_array()
    .ok_or_else(|| format!("{what} must be an array of strings"))?;
  let mut out = BTreeSet::new();
  for item in arr {
    let s = item.as_str().ok_or_else(|| format!("{what} entries must be strings"))?;
    out.insert(normalize_header_name(s));
  }
  Ok(out)
}

pub fn normalize_header_name(name: &str) -> String {
  name.trim().to_ascii_lowercase()
}

pub fn is_router_controlled(name: &str) -> bool {
  let n = normalize_header_name(name);
  ROUTER_CONTROLLED_HEADERS.contains(&n.as_str())
}

fn default_denied_headers() -> BTreeSet<String> {
  ROUTER_CONTROLLED_HEADERS.iter().map(|s| s.to_string()).collect()
}

fn validate_template(value: &str, what: &str) -> Result<(), String> {
  for name in template_names(value) {
    if !is_known_template(&name) {
      return Err(format!("{what} references unknown template <{name}>"));
    }
  }
  Ok(())
}

fn resolve_static_template(value: &str) -> Result<String, String> {
  render_template(value, &TemplateVars::default(), true).ok_or_else(|| "static template render failed".to_string())
}

fn render_request_template(value: &str, vars: &TemplateVars) -> Option<String> {
  render_template(value, vars, false)
}

fn render_template(value: &str, vars: &TemplateVars, static_only: bool) -> Option<String> {
  let mut out = String::with_capacity(value.len());
  let mut rest = value;
  while let Some(start) = rest.find('<') {
    out.push_str(&rest[..start]);
    let after = &rest[start + 1..];
    if let Some(stripped) = after.strip_prefix('<') {
      out.push('<');
      rest = stripped;
      continue;
    }
    let end = after.find('>')?;
    let name = &after[..end];
    let replacement = static_template_value(name).or_else(|| {
      if static_only {
        Some(format!("<{name}>"))
      } else {
        request_template_value(name, vars)
      }
    })?;
    out.push_str(&replacement);
    rest = &after[end + 1..];
  }
  out.push_str(rest);
  Some(out)
}

fn template_names(value: &str) -> Vec<String> {
  let mut names = Vec::new();
  let mut rest = value;
  while let Some(start) = rest.find('<') {
    let after = &rest[start + 1..];
    if let Some(stripped) = after.strip_prefix('<') {
      rest = stripped;
      continue;
    }
    let Some(end) = after.find('>') else {
      break;
    };
    names.push(after[..end].to_string());
    rest = &after[end + 1..];
  }
  names
}

fn is_known_template(name: &str) -> bool {
  matches!(
    name,
    "os"
      | "arch"
      | "os-pretty"
      | "hostname"
      | "tokn-router-version"
      | "session_id"
      | "request_id"
      | "project_cwd"
      | "interaction_id"
      | "account_id"
  )
}

fn static_template_value(name: &str) -> Option<String> {
  match name {
    "os" => Some(match std::env::consts::OS {
      "macos" => "macos".into(),
      "windows" => "windows".into(),
      "linux" => "linux".into(),
      other => other.into(),
    }),
    "arch" => Some(match std::env::consts::ARCH {
      "x86_64" => "x64".into(),
      "aarch64" => "arm64".into(),
      other => other.into(),
    }),
    "os-pretty" => Some(os_pretty()),
    "hostname" => std::env::var("HOSTNAME").ok().filter(|s| !s.trim().is_empty()),
    "tokn-router-version" => Some(tokn_core::util::version::full().to_string()),
    _ => None,
  }
}

fn request_template_value(name: &str, vars: &TemplateVars) -> Option<String> {
  match name {
    "session_id" => vars.session_id.as_ref().map(|s| s.to_string()),
    "request_id" => vars.request_id.as_ref().map(|s| s.to_string()),
    "project_cwd" => vars.project_cwd.as_ref().map(|s| s.to_string()),
    "interaction_id" => vars.interaction_id.as_ref().map(|s| s.to_string()),
    "account_id" => vars.account_id.as_ref().map(|s| s.to_string()),
    _ => None,
  }
  .filter(|s| !s.is_empty())
}

fn os_pretty() -> String {
  if cfg!(target_os = "linux") {
    if let Ok(s) = std::fs::read_to_string("/etc/os-release") {
      for line in s.lines() {
        if let Some(value) = line.strip_prefix("PRETTY_NAME=") {
          return value.trim_matches('"').to_string();
        }
      }
    }
  }
  match std::env::consts::OS {
    "macos" => "macOS".into(),
    "windows" => "Windows".into(),
    other => other.into(),
  }
}

pub fn user_profiles_path() -> Option<PathBuf> {
  tokn_core::util::paths::config_dir().map(|dir| dir.join("profiles.toml"))
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

  #[test]
  fn resolves_forward_deny_and_scopes() {
    let p = Profiles::parse(
      r#"
        [opencode]
        verified = true
        forward = ["X-Session-Affinity", "Content-Type"]
        deny = ["Cookie"]

        [opencode.default]
        "user-agent" = "opencode/<arch>"

        [opencode.github-copilot]
        "x-session-affinity" = "<session_id>"
      "#,
    )
    .unwrap();
    let r = p.resolve("opencode", "github-copilot").unwrap();
    assert!(r.forward.contains("x-session-affinity"));
    assert!(r.forward.contains("content-type"));
    assert!(r.deny.contains("cookie"));
    assert!(r.headers.get("user-agent").unwrap().starts_with("opencode/"));
    assert_eq!(r.headers.get("x-session-affinity").unwrap(), "<session_id>");
  }

  #[test]
  fn drops_header_when_request_template_missing() {
    let p = Profiles::parse(
      r#"
        [opencode.default]
        "x-session-affinity" = "<session_id>"
        "x-static" = "ok"
      "#,
    )
    .unwrap();
    let r = p.resolve("opencode", "github-copilot").unwrap();
    let rendered = r.render_headers(&TemplateVars::default());
    assert!(!rendered.contains_key("x-session-affinity"));
    assert_eq!(rendered.get("x-static").map(String::as_str), Some("ok"));
  }

  #[test]
  fn renders_request_template_when_present() {
    let p = Profiles::parse(
      r#"
        [opencode.default]
        "x-session-affinity" = "<session_id>"
      "#,
    )
    .unwrap();
    let r = p.resolve("opencode", "github-copilot").unwrap();
    let rendered = r.render_headers(&TemplateVars {
      session_id: Some("ses_123".into()),
      ..Default::default()
    });
    assert_eq!(rendered.get("x-session-affinity").map(String::as_str), Some("ses_123"));
  }

  #[test]
  fn ignores_router_controlled_headers() {
    let p = Profiles::parse(
      r#"
        [opencode.default]
        accept = "*/*"
        "content-type" = "application/json"
        "user-agent" = "opencode"
      "#,
    )
    .unwrap();
    let r = p.resolve("opencode", "github-copilot").unwrap();
    assert!(!r.headers.contains_key("accept"));
    assert!(!r.headers.contains_key("content-type"));
    assert_eq!(r.headers.get("user-agent").map(String::as_str), Some("opencode"));
  }
}
