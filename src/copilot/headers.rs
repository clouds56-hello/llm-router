use crate::config::CopilotHeaders;
use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};

/// Headers for the Copilot API token exchange (`api.github.com`).
pub fn token_exchange_headers(github_token: &str, h: &CopilotHeaders) -> Result<HeaderMap> {
    let mut m = HeaderMap::new();
    m.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("token {github_token}"))?,
    );
    m.insert(ACCEPT, HeaderValue::from_static("application/json"));
    m.insert(USER_AGENT, HeaderValue::from_str(&h.user_agent)?);
    insert_str(&mut m, "editor-version", &h.editor_version)?;
    insert_str(&mut m, "editor-plugin-version", &h.editor_plugin_version)?;
    Ok(m)
}

/// Headers for upstream Copilot API requests (chat / models).
pub fn copilot_request_headers(
    api_token: &str,
    h: &CopilotHeaders,
    streaming: bool,
) -> Result<HeaderMap> {
    let mut m = HeaderMap::new();
    m.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {api_token}"))?,
    );
    m.insert(
        ACCEPT,
        HeaderValue::from_static(if streaming { "text/event-stream" } else { "application/json" }),
    );
    m.insert(USER_AGENT, HeaderValue::from_str(&h.user_agent)?);
    insert_str(&mut m, "editor-version", &h.editor_version)?;
    insert_str(&mut m, "editor-plugin-version", &h.editor_plugin_version)?;
    insert_str(&mut m, "copilot-integration-id", &h.copilot_integration_id)?;
    insert_str(&mut m, "openai-intent", &h.openai_intent)?;

    // Extra (free-form) headers — applied last, overriding earlier values.
    for (k, v) in &h.extra_headers {
        let name = HeaderName::from_bytes(k.as_bytes())
            .with_context(|| format!("invalid header name {k:?}"))?;
        let val = HeaderValue::from_str(v)
            .with_context(|| format!("invalid header value for {k:?}"))?;
        m.insert(name, val);
    }
    Ok(m)
}

fn insert_str(m: &mut HeaderMap, name: &'static str, value: &str) -> Result<()> {
    let n = HeaderName::from_static(name);
    let v = HeaderValue::from_str(value)
        .with_context(|| format!("invalid header value for {name}"))?;
    m.insert(n, v);
    Ok(())
}
