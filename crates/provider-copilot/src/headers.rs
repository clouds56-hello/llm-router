use crate::config::CopilotHeaders;
use crate::provider::{error, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use snafu::ResultExt;

/// Headers for the Copilot API token exchange (`api.github.com`).
pub fn token_exchange_headers(github_token: &str, h: &CopilotHeaders) -> Result<HeaderMap> {
  let mut m = HeaderMap::new();
  m.insert(
    AUTHORIZATION,
    HeaderValue::from_str(&format!("token {github_token}"))
      .context(error::HeaderValueSnafu { name: "authorization" })?,
  );
  m.insert(ACCEPT, HeaderValue::from_static("application/json"));
  m.insert(
    USER_AGENT,
    HeaderValue::from_str(&h.user_agent).context(error::HeaderValueSnafu { name: "user-agent" })?,
  );
  insert_str(&mut m, "editor-version", &h.editor_version)?;
  insert_str(&mut m, "editor-plugin-version", &h.editor_plugin_version)?;
  Ok(m)
}

/// Headers for upstream Copilot API requests (chat / models).
///
/// `initiator` must be "user" or "agent". It is sent as `X-Initiator` and is
/// what GitHub's billing pipeline uses to attribute premium-request charges to
/// a single user-initiated turn rather than to every tool-call follow-up.
pub fn copilot_request_headers(
  api_token: &str,
  h: &CopilotHeaders,
  streaming: bool,
  initiator: &str,
) -> Result<HeaderMap> {
  let mut m = HeaderMap::new();
  m.insert(
    AUTHORIZATION,
    HeaderValue::from_str(&format!("Bearer {api_token}")).context(error::HeaderValueSnafu { name: "authorization" })?,
  );
  m.insert(
    ACCEPT,
    HeaderValue::from_static(if streaming {
      "text/event-stream"
    } else {
      "application/json"
    }),
  );
  m.insert(
    USER_AGENT,
    HeaderValue::from_str(&h.user_agent).context(error::HeaderValueSnafu { name: "user-agent" })?,
  );
  insert_str(&mut m, "editor-version", &h.editor_version)?;
  insert_str(&mut m, "editor-plugin-version", &h.editor_plugin_version)?;
  insert_str(&mut m, "copilot-integration-id", &h.copilot_integration_id)?;
  insert_str(&mut m, "openai-intent", &h.openai_intent)?;
  insert_str(&mut m, "x-initiator", initiator)?;

  // Extra (free-form) headers — applied last, overriding earlier values.
  for (k, v) in &h.extra_headers {
    let name = HeaderName::from_bytes(k.as_bytes()).context(error::HeaderNameSnafu { name: k.clone() })?;
    let val = HeaderValue::from_str(v).context(error::HeaderValueSnafu { name: k.clone() })?;
    m.insert(name, val);
  }
  Ok(m)
}

fn insert_str(m: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
  let n = HeaderName::from_static(name);
  let v = HeaderValue::from_str(value).context(error::HeaderValueSnafu { name: name.to_string() })?;
  m.insert(n, v);
  Ok(())
}
