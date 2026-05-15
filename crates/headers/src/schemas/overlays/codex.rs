//! Codex (ChatGPT account) transport overlay.
//!
//! Headers required when targeting the ChatGPT-account Codex backend on top
//! of a base persona.
//!
//! SCOPE: this overlay models **outbound** headers the router injects /
//! validates when forwarding to `chatgpt.com`. The codex-cli-native
//! inbound headers (`originator`, `version`, `session_id`, `thread_id`,
//! `x-codex-*`) are modelled directly on `CodexCliHeaders`.

use crate::error::Error;
use crate::keys;
use crate::map::HeaderMap;
use crate::name::HeaderName;
use crate::schema::{optional, put, put_opt, required, HeaderSchema};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexOverlay {
  #[serde(rename = "OpenAI-Beta")]
  pub openai_beta: SmolStr,
  #[serde(rename = "OpenAI-Intent")]
  pub openai_intent: Option<SmolStr>,
  #[serde(rename = "chatgpt-account-id")]
  pub chatgpt_account_id: Option<SmolStr>,
  #[serde(rename = "X-Session-Id")]
  pub session_id: Option<SmolStr>,
}

impl HeaderSchema for CodexOverlay {
  fn parse(map: &HeaderMap) -> Result<Self, Error> {
    Ok(Self {
      openai_beta: required(map, &keys::OPENAI_BETA)?,
      openai_intent: optional(map, &keys::OPENAI_INTENT),
      chatgpt_account_id: optional(map, &keys::CHATGPT_ACCOUNT_ID),
      session_id: optional(map, &keys::X_SESSION_ID),
    })
  }
  fn build(&self) -> HeaderMap {
    let mut m = HeaderMap::new();
    put(&mut m, &keys::OPENAI_BETA, &self.openai_beta);
    put_opt(&mut m, &keys::OPENAI_INTENT, &self.openai_intent);
    put_opt(&mut m, &keys::CHATGPT_ACCOUNT_ID, &self.chatgpt_account_id);
    put_opt(&mut m, &keys::X_SESSION_ID, &self.session_id);
    m
  }
  fn known_names() -> &'static [&'static HeaderName] {
    static NAMES: [&HeaderName; 4] =
      [&keys::OPENAI_BETA, &keys::OPENAI_INTENT, &keys::CHATGPT_ACCOUNT_ID, &keys::X_SESSION_ID];
    &NAMES
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip() {
    let h = CodexOverlay {
      openai_beta: "responses=v1".into(),
      openai_intent: Some("assistants".into()),
      chatgpt_account_id: Some("acct_99".into()),
      session_id: Some("ses_codex".into()),
    };
    assert_eq!(CodexOverlay::parse(&h.build()).unwrap(), h);
  }
}
