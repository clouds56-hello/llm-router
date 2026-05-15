//! GitHub Copilot transport overlay.
//!
//! Headers required by the Copilot proxy regardless of which CLI persona
//! originated the request.
//!
//! SCOPE: this overlay models **outbound** headers the router injects when
//! forwarding to `api.githubcopilot.com`. The mined inbound matrix never
//! shows `Editor-Version`, `Editor-Plugin-Version`, `Copilot-Integration-Id`,
//! or `Copilot-Vision-Request` because those are added downstream of the
//! gateway. Inbound-only Copilot signals (e.g. `X-Initiator`,
//! `OpenAI-Intent`) are observed from CLI clients targeting the gateway.

use crate::error::Error;
use crate::keys;
use crate::map::HeaderMap;
use crate::name::HeaderName;
use crate::schema::{optional, put, put_opt, required, HeaderSchema};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopilotOverlay {
  #[serde(rename = "Editor-Version")]
  pub editor_version: SmolStr,
  #[serde(rename = "Editor-Plugin-Version")]
  pub editor_plugin_version: SmolStr,
  #[serde(rename = "Copilot-Integration-Id")]
  pub integration_id: SmolStr,
  #[serde(rename = "Copilot-Vision-Request")]
  pub vision_request: Option<SmolStr>,
  #[serde(rename = "X-Initiator")]
  pub initiator: Option<SmolStr>,
}

impl HeaderSchema for CopilotOverlay {
  fn parse(map: &HeaderMap) -> Result<Self, Error> {
    Ok(Self {
      editor_version: required(map, &keys::EDITOR_VERSION)?,
      editor_plugin_version: required(map, &keys::EDITOR_PLUGIN_VERSION)?,
      integration_id: required(map, &keys::COPILOT_INTEGRATION_ID)?,
      vision_request: optional(map, &keys::COPILOT_VISION_REQUEST),
      initiator: optional(map, &keys::X_INITIATOR),
    })
  }
  fn build(&self) -> HeaderMap {
    let mut m = HeaderMap::new();
    put(&mut m, &keys::EDITOR_VERSION, &self.editor_version);
    put(&mut m, &keys::EDITOR_PLUGIN_VERSION, &self.editor_plugin_version);
    put(&mut m, &keys::COPILOT_INTEGRATION_ID, &self.integration_id);
    put_opt(&mut m, &keys::COPILOT_VISION_REQUEST, &self.vision_request);
    put_opt(&mut m, &keys::X_INITIATOR, &self.initiator);
    m
  }
  fn known_names() -> &'static [&'static HeaderName] {
    static NAMES: [&HeaderName; 5] = [
      &keys::EDITOR_VERSION,
      &keys::EDITOR_PLUGIN_VERSION,
      &keys::COPILOT_INTEGRATION_ID,
      &keys::COPILOT_VISION_REQUEST,
      &keys::X_INITIATOR,
    ];
    &NAMES
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip() {
    let h = CopilotOverlay {
      editor_version: "vscode/1.95.0".into(),
      editor_plugin_version: "copilot-chat/0.23.0".into(),
      integration_id: "vscode-chat".into(),
      vision_request: Some("true".into()),
      initiator: Some("agent".into()),
    };
    assert_eq!(CopilotOverlay::parse(&h.build()).unwrap(), h);
  }

  #[test]
  fn missing_required_errors() {
    let m = HeaderMap::new();
    assert!(matches!(CopilotOverlay::parse(&m), Err(Error::MissingHeader { .. })));
  }
}
