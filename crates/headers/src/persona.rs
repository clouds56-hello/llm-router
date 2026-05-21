//! Internal header-building identity used to synthesize client-specific
//! outbound headers.
//!
//! This remains an open enum because the header registry still uses a
//! fallback `Custom` variant internally, but router-facing code should use
//! [`tokn_core::ClientId`] rather than inferring identity from inbound
//! headers.

use crate::map::HeaderMap;
use crate::schema::HeaderSchema;
use crate::schemas::{ClaudeCodeHeaders, ClineHeaders, CodexCliHeaders, CopilotCliHeaders, OpencodeHeaders};
use crate::vars::TemplateVars;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

/// A named originator of an inbound request. Use [`Persona::from_str_lossy`]
/// or the [`FromStr`] impl to parse a string into this enum without ever
/// failing — unknown tool names fall through to [`Persona::Custom`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "SmolStr", into = "SmolStr")]
pub enum Persona {
  Opencode,
  CodexCli,
  ClaudeCode,
  Cline,
  CopilotCli,
  Custom(SmolStr),
}

impl Persona {
  /// Parse from any string. Never fails — falls back to [`Persona::Custom`].
  pub fn from_str_lossy(s: &str) -> Self {
    match s {
      "opencode" => Self::Opencode,
      "codex" | "codex-cli" => Self::CodexCli,
      "claude-code" => Self::ClaudeCode,
      "cline" => Self::Cline,
      "copilot" | "copilot-cli" => Self::CopilotCli,
      other => Self::Custom(SmolStr::new(other)),
    }
  }

  /// String form, suitable for use as a profile key.
  pub fn as_str(&self) -> &str {
    match self {
      Self::Opencode => "opencode",
      Self::CodexCli => "codex-cli",
      Self::ClaudeCode => "claude-code",
      Self::Cline => "cline",
      Self::CopilotCli => "copilot-cli",
      Self::Custom(s) => s.as_str(),
    }
  }

  /// Build the outbound HeaderMap for this persona from `vars` and `inbound`.
  ///
  /// Dispatches to the concrete [`HeaderSchema`] struct's `build` constructor
  /// then `dump`s the result. For [`Persona::Custom`], returns an empty
  /// `HeaderMap` — callers (typically the router) should resolve `Custom` to
  /// a configured fallback variant **before** invoking this method.
  pub fn build_outbound(&self, vars: &TemplateVars, inbound: &HeaderMap) -> HeaderMap {
    match self {
      Self::Opencode => OpencodeHeaders::build(vars, inbound).dump(),
      Self::CodexCli => CodexCliHeaders::build(vars, inbound).dump(),
      Self::ClaudeCode => ClaudeCodeHeaders::build(vars, inbound).dump(),
      Self::Cline => ClineHeaders::build(vars, inbound).dump(),
      Self::CopilotCli => CopilotCliHeaders::build(vars, inbound).dump(),
      Self::Custom(_) => HeaderMap::new(),
    }
  }
}

impl fmt::Display for Persona {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl FromStr for Persona {
  type Err = Infallible;
  fn from_str(s: &str) -> Result<Self, Infallible> {
    Ok(Self::from_str_lossy(s))
  }
}

impl From<SmolStr> for Persona {
  fn from(s: SmolStr) -> Self {
    Self::from_str_lossy(&s)
  }
}

impl From<Persona> for SmolStr {
  fn from(p: Persona) -> SmolStr {
    SmolStr::new(p.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_personas_round_trip() {
    for s in ["opencode", "codex-cli", "claude-code", "cline", "copilot-cli"] {
      let p = Persona::from_str_lossy(s);
      assert_eq!(p.as_str(), s);
      assert_eq!(p.to_string(), s);
    }
  }

  #[test]
  fn codex_alias_normalizes() {
    assert_eq!(Persona::from_str_lossy("codex"), Persona::CodexCli);
  }

  #[test]
  fn copilot_alias_normalizes() {
    assert_eq!(Persona::from_str_lossy("copilot"), Persona::CopilotCli);
  }

  #[test]
  fn unknown_persona_falls_back_to_custom() {
    let p: Persona = "my-bespoke-tool".parse().unwrap();
    assert_eq!(p, Persona::Custom(SmolStr::new("my-bespoke-tool")));
    assert_eq!(p.as_str(), "my-bespoke-tool");
  }

  #[test]
  fn serde_round_trip_known() {
    let p = Persona::Opencode;
    let s = serde_json::to_string(&p).unwrap();
    assert_eq!(s, "\"opencode\"");
    let back: Persona = serde_json::from_str(&s).unwrap();
    assert_eq!(back, p);
  }

  #[test]
  fn serde_round_trip_custom() {
    let p = Persona::Custom(SmolStr::new("foo"));
    let s = serde_json::to_string(&p).unwrap();
    assert_eq!(s, "\"foo\"");
    let back: Persona = serde_json::from_str(&s).unwrap();
    assert_eq!(back, p);
  }

  #[test]
  fn build_outbound_for_known_persona_returns_nonempty_map() {
    let vars = TemplateVars::default();
    let inbound = HeaderMap::new();
    for p in [
      Persona::Opencode,
      Persona::CodexCli,
      Persona::ClaudeCode,
      Persona::Cline,
      Persona::CopilotCli,
    ] {
      let out = p.build_outbound(&vars, &inbound);
      assert!(!out.is_empty(), "{p:?} should build a non-empty HeaderMap");
    }
  }

  #[test]
  fn build_outbound_for_custom_returns_empty_map() {
    let out = Persona::Custom(SmolStr::new("anything")).build_outbound(&TemplateVars::default(), &HeaderMap::new());
    assert_eq!(out.len(), 0);
  }
}
