//! Headers emitted by the OpenCode CLI client.
//!
//! Field set derived from the inbound real-world matrix (see
//! `tests/fixtures/inbound_real_world.json`). Required fields are present in
//! ≥99% of captured requests; optional fields are observed but inconsistent.
//!
//! `Authorization` is modelled as required even though its value may be the
//! literal `"<redacted>"` in fixtures: the *header* is universally present,
//! and downstream layers replace its value before transmission.

use crate::error::Error;
use crate::keys;
use crate::map::HeaderMap;
use crate::name::HeaderName;
use crate::schema::{optional, put, put_opt, required, HeaderSchema};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

/// Inbound headers consistently emitted by the OpenCode CLI persona.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpencodeHeaders {
  // Transport (always present)
  #[serde(rename = "User-Agent")]
  pub user_agent: SmolStr,
  #[serde(rename = "Authorization")]
  pub authorization: SmolStr,
  #[serde(rename = "Host")]
  pub host: SmolStr,
  #[serde(rename = "Accept")]
  pub accept: SmolStr,
  #[serde(rename = "Accept-Encoding")]
  pub accept_encoding: SmolStr,
  #[serde(rename = "Connection")]
  pub connection: SmolStr,
  #[serde(rename = "Content-Type")]
  pub content_type: SmolStr,

  // Body framing (present on every POST; absent on GET /models)
  #[serde(rename = "Content-Length", skip_serializing_if = "Option::is_none")]
  pub content_length: Option<SmolStr>,

  // Session correlation (X-Session-Affinity is the inbound name; X-Session-Id
  // is router-injected and never appears in opencode-emitted captures).
  #[serde(rename = "X-Session-Affinity", skip_serializing_if = "Option::is_none")]
  pub session_affinity: Option<SmolStr>,
  #[serde(rename = "X-Parent-Session-Id", skip_serializing_if = "Option::is_none")]
  pub parent_session_id: Option<SmolStr>,
}

impl HeaderSchema for OpencodeHeaders {
  fn parse(map: &HeaderMap) -> Result<Self, Error> {
    Ok(Self {
      user_agent: required(map, &keys::USER_AGENT)?,
      authorization: required(map, &keys::AUTHORIZATION)?,
      host: required(map, &keys::HOST)?,
      accept: required(map, &keys::ACCEPT)?,
      accept_encoding: required(map, &keys::ACCEPT_ENCODING)?,
      connection: required(map, &keys::CONNECTION)?,
      content_type: required(map, &keys::CONTENT_TYPE)?,
      content_length: optional(map, &keys::CONTENT_LENGTH),
      session_affinity: optional(map, &keys::X_SESSION_AFFINITY),
      parent_session_id: optional(map, &keys::X_PARENT_SESSION_ID),
    })
  }
  fn build(&self) -> HeaderMap {
    let mut m = HeaderMap::new();
    put(&mut m, &keys::USER_AGENT, &self.user_agent);
    put(&mut m, &keys::AUTHORIZATION, &self.authorization);
    put(&mut m, &keys::HOST, &self.host);
    put(&mut m, &keys::ACCEPT, &self.accept);
    put(&mut m, &keys::ACCEPT_ENCODING, &self.accept_encoding);
    put(&mut m, &keys::CONNECTION, &self.connection);
    put(&mut m, &keys::CONTENT_TYPE, &self.content_type);
    put_opt(&mut m, &keys::CONTENT_LENGTH, &self.content_length);
    put_opt(&mut m, &keys::X_SESSION_AFFINITY, &self.session_affinity);
    put_opt(&mut m, &keys::X_PARENT_SESSION_ID, &self.parent_session_id);
    m
  }
  fn known_names() -> &'static [&'static HeaderName] {
    static NAMES: [&HeaderName; 10] = [
      &keys::USER_AGENT,
      &keys::AUTHORIZATION,
      &keys::HOST,
      &keys::ACCEPT,
      &keys::ACCEPT_ENCODING,
      &keys::CONNECTION,
      &keys::CONTENT_TYPE,
      &keys::CONTENT_LENGTH,
      &keys::X_SESSION_AFFINITY,
      &keys::X_PARENT_SESSION_ID,
    ];
    &NAMES
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn sample() -> OpencodeHeaders {
    OpencodeHeaders {
      user_agent: "opencode/1.14.28 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.13".into(),
      authorization: "<redacted>".into(),
      host: "api.deepseek.com".into(),
      accept: "*/*".into(),
      accept_encoding: "gzip, deflate, br, zstd".into(),
      connection: "keep-alive".into(),
      content_type: "application/json".into(),
      content_length: Some("4429".into()),
      session_affinity: Some("ses_1dddd2016ffed1A1u3yj5LmNWC".into()),
      parent_session_id: None,
    }
  }

  #[test]
  fn opencode_round_trip() {
    let h = sample();
    let parsed = OpencodeHeaders::parse(&h.build()).unwrap();
    assert_eq!(parsed, h);
  }

  #[test]
  fn opencode_optional_fields_omitted_when_none() {
    let mut h = sample();
    h.content_length = None;
    h.session_affinity = None;
    h.parent_session_id = None;
    let m = h.build();
    // 7 required fields, 0 optional written.
    assert_eq!(m.len(), 7);
    assert!(!m.contains_key(&keys::CONTENT_LENGTH));
    assert!(!m.contains_key(&keys::X_SESSION_AFFINITY));
  }

  #[test]
  fn opencode_missing_required_returns_error() {
    let m = HeaderMap::new();
    assert!(matches!(OpencodeHeaders::parse(&m), Err(Error::MissingHeader { .. })));
  }
}
