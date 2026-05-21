//! Logical identifier for the *client* whose behavior an upstream call should
//! impersonate. Carried end-to-end through the pipeline so downstream stages
//! (header shaping, provider selection) can branch on it without reparsing
//! inbound traffic.
//!
//! The newtype wraps [`SmolStr`] so common values (e.g. `"codex"`, `"copilot"`)
//! stay inline and cloning stays cheap.

use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ClientId(pub SmolStr);

impl ClientId {
  pub fn new(s: impl Into<SmolStr>) -> Self {
    Self(s.into())
  }

  pub fn as_str(&self) -> &str {
    self.0.as_str()
  }
}

impl From<&str> for ClientId {
  fn from(s: &str) -> Self {
    Self(SmolStr::new(s))
  }
}

impl From<String> for ClientId {
  fn from(s: String) -> Self {
    Self(SmolStr::new(s))
  }
}

impl From<SmolStr> for ClientId {
  fn from(s: SmolStr) -> Self {
    Self(s)
  }
}

impl std::fmt::Display for ClientId {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.0.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn new_and_as_str_roundtrip() {
    let id = ClientId::new("codex");
    assert_eq!(id.as_str(), "codex");
    assert_eq!(id.to_string(), "codex");
  }

  #[test]
  fn equality_independent_of_construction_path() {
    assert_eq!(ClientId::from("opencode"), ClientId::new(SmolStr::new("opencode")));
    assert_eq!(ClientId::from(String::from("copilot")), ClientId::from("copilot"));
  }
}
