//! Logical identifier for the *client* whose behavior an upstream call should
//! impersonate. Carried end-to-end through the pipeline so downstream stages
//! (header shaping, provider selection) can branch on it without reparsing
//! inbound traffic.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientId {
  #[serde(rename = "opencode")]
  Opencode,
  #[serde(rename = "codex-cli")]
  CodexCli,
  #[serde(rename = "claude-code")]
  ClaudeCode,
  #[serde(rename = "cline")]
  Cline,
  #[serde(rename = "copilot-cli")]
  CopilotCli,
}

impl ClientId {
  pub fn as_str(self) -> &'static str {
    match self {
      Self::Opencode => "opencode",
      Self::CodexCli => "codex-cli",
      Self::ClaudeCode => "claude-code",
      Self::Cline => "cline",
      Self::CopilotCli => "copilot-cli",
    }
  }

  pub fn from_slug(slug: &str) -> Option<Self> {
    match slug {
      "opencode" => Some(Self::Opencode),
      "codex_exec" | "codex-tui" | "codex" | "codex-cli" => Some(Self::CodexCli),
      "claude-cli" | "claude-code" => Some(Self::ClaudeCode),
      "cline" => Some(Self::Cline),
      "copilot" | "copilot-cli" => Some(Self::CopilotCli),
      _ => None,
    }
  }

  pub fn provider_default(provider_id: &str) -> Option<Self> {
    match provider_id {
      "openai" | "deepseek" | "zai" | "zai-coding-plan" | "zhipuai" | "zhipuai-coding-plan" => Some(Self::Opencode),
      "codex" => Some(Self::CodexCli),
      "copilot" | "github-copilot" => Some(Self::CopilotCli),
      _ => None,
    }
  }
}

impl From<&str> for ClientId {
  fn from(s: &str) -> Self {
    Self::from_slug(s).unwrap_or_else(|| panic!("unknown client_id slug: {s}"))
  }
}

impl From<String> for ClientId {
  fn from(s: String) -> Self {
    Self::from(s.as_str())
  }
}

impl From<SmolStr> for ClientId {
  fn from(s: SmolStr) -> Self {
    Self::from(s.as_str())
  }
}

impl From<ClientId> for SmolStr {
  fn from(value: ClientId) -> Self {
    SmolStr::new(value.as_str())
  }
}

impl FromStr for ClientId {
  type Err = String;

  fn from_str(s: &str) -> Result<Self, Self::Err> {
    Self::from_slug(s).ok_or_else(|| format!("unknown client_id slug: {s}"))
  }
}

impl fmt::Display for ClientId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str((*self).as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn known_ids_round_trip() {
    for (slug, expected) in [
      ("opencode", ClientId::Opencode),
      ("codex-cli", ClientId::CodexCli),
      ("claude-code", ClientId::ClaudeCode),
      ("cline", ClientId::Cline),
      ("copilot-cli", ClientId::CopilotCli),
    ] {
      assert_eq!(ClientId::from_slug(slug), Some(expected));
      assert_eq!(expected.as_str(), slug);
      assert_eq!(expected.to_string(), slug);
    }
  }

  #[test]
  fn aliases_normalize() {
    assert_eq!(ClientId::from_slug("codex"), Some(ClientId::CodexCli));
    assert_eq!(ClientId::from_slug("codex_exec"), Some(ClientId::CodexCli));
    assert_eq!(ClientId::from_slug("claude-cli"), Some(ClientId::ClaudeCode));
    assert_eq!(ClientId::from_slug("copilot"), Some(ClientId::CopilotCli));
  }

  #[test]
  fn provider_defaults_cover_known_providers() {
    assert_eq!(ClientId::provider_default("openai"), Some(ClientId::Opencode));
    assert_eq!(ClientId::provider_default("codex"), Some(ClientId::CodexCli));
    assert_eq!(ClientId::provider_default("github-copilot"), Some(ClientId::CopilotCli));
    assert_eq!(ClientId::provider_default("nonesuch"), None);
  }
}
