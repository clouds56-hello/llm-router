//! Headers emitted by the Cline CLI client.
//!
//! NOTE: not yet verified against real-world inbound captures — no `cline`
//! traffic was observed in the mined request logs. Field set is a
//! best-effort outbound model and may need refinement once captures
//! become available.

use crate::error::Error;
use crate::keys;
use crate::map::HeaderMap;
use crate::name::HeaderName;
use crate::schema::{optional, put, put_opt, required, HeaderSchema};
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClineHeaders {
  #[serde(rename = "User-Agent")]
  pub user_agent: SmolStr,
  #[serde(rename = "X-Session-Id")]
  pub session_id: Option<SmolStr>,
  #[serde(rename = "X-Behave-As")]
  pub behave_as: Option<SmolStr>,
}

impl HeaderSchema for ClineHeaders {
  fn parse(map: &HeaderMap) -> Result<Self, Error> {
    Ok(Self {
      user_agent: required(map, &keys::USER_AGENT)?,
      session_id: optional(map, &keys::X_SESSION_ID),
      behave_as: optional(map, &keys::X_BEHAVE_AS),
    })
  }
  fn build(&self) -> HeaderMap {
    let mut m = HeaderMap::new();
    put(&mut m, &keys::USER_AGENT, &self.user_agent);
    put_opt(&mut m, &keys::X_SESSION_ID, &self.session_id);
    put_opt(&mut m, &keys::X_BEHAVE_AS, &self.behave_as);
    m
  }
  fn known_names() -> &'static [&'static HeaderName] {
    static NAMES: [&HeaderName; 3] = [&keys::USER_AGENT, &keys::X_SESSION_ID, &keys::X_BEHAVE_AS];
    &NAMES
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn round_trip() {
    let h = ClineHeaders {
      user_agent: "cline/3.0.0".into(),
      session_id: Some("ses_cli".into()),
      behave_as: Some("agent".into()),
    };
    assert_eq!(ClineHeaders::parse(&h.build()).unwrap(), h);
  }
}
