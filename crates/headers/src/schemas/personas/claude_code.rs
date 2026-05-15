//! Headers emitted by the Claude Code CLI client.
//!
//! NOTE: not yet verified against real-world inbound captures — no
//! `claude-cli` traffic was observed in the mined request logs. Field set is
//! a best-effort outbound model and may need refinement once captures
//! become available.

use crate::error::Error;
use crate::keys;
use crate::map::HeaderMap;
use crate::name::HeaderName;
use crate::schema::{optional, put, put_opt, required, HeaderSchema};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaudeCodeHeaders {
  #[serde(rename = "User-Agent")]
  pub user_agent: SmolStr,
  #[serde(rename = "Anthropic-Version")]
  pub anthropic_version: Option<SmolStr>,
  #[serde(rename = "Anthropic-Beta")]
  pub anthropic_beta: Option<SmolStr>,
  #[serde(rename = "X-Session-Id")]
  pub session_id: Option<SmolStr>,
  #[serde(rename = "X-Interaction-Id")]
  pub interaction_id: Option<SmolStr>,
}

impl HeaderSchema for ClaudeCodeHeaders {
  fn parse(map: &HeaderMap) -> Result<Self, Error> {
    Ok(Self {
      user_agent: required(map, &keys::USER_AGENT)?,
      anthropic_version: optional(map, &keys::ANTHROPIC_VERSION),
      anthropic_beta: optional(map, &keys::ANTHROPIC_BETA),
      session_id: optional(map, &keys::X_SESSION_ID),
      interaction_id: optional(map, &keys::X_INTERACTION_ID),
    })
  }
  fn build(&self) -> HeaderMap {
    let mut m = HeaderMap::new();
    put(&mut m, &keys::USER_AGENT, &self.user_agent);
    put_opt(&mut m, &keys::ANTHROPIC_VERSION, &self.anthropic_version);
    put_opt(&mut m, &keys::ANTHROPIC_BETA, &self.anthropic_beta);
    put_opt(&mut m, &keys::X_SESSION_ID, &self.session_id);
    put_opt(&mut m, &keys::X_INTERACTION_ID, &self.interaction_id);
    m
  }
  fn known_names() -> &'static [&'static HeaderName] {
    static NAMES: [&HeaderName; 5] = [
      &keys::USER_AGENT,
      &keys::ANTHROPIC_VERSION,
      &keys::ANTHROPIC_BETA,
      &keys::X_SESSION_ID,
      &keys::X_INTERACTION_ID,
    ];
    &NAMES
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip() {
    let h = ClaudeCodeHeaders {
      user_agent: "claude-code/1.2.3".into(),
      anthropic_version: Some("2023-06-01".into()),
      anthropic_beta: Some("messages-2023-12-15".into()),
      session_id: Some("ses_cc".into()),
      interaction_id: Some("int_99".into()),
    };
    assert_eq!(ClaudeCodeHeaders::parse(&h.build()).unwrap(), h);
  }
}
