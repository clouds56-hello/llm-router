use crate::config::CopilotHeaders;
use crate::provider::{error, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde_json::Value;
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
    HeaderValue::from_str(&format!("Bearer {api_token}"))
      .context(error::HeaderValueSnafu { name: "authorization" })?,
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

/// Classify a chat completion request as a fresh user turn ("user") or as a
/// continuation of an in-flight tool-use loop ("agent").
///
/// Heuristic, walking from the end of `messages` and skipping system msgs:
/// - last non-system role is `tool`           -> "agent" (sending a tool result)
/// - last non-system role is `assistant`      -> "agent" (assistant about to be
///   re-prompted, e.g. continuation of a forced response — billed as one turn)
/// - last non-system role is `user`           -> "user"
/// - empty / unknown                          -> "user"
///
/// Mirrors what VS Code Copilot Chat sends: a single `X-Initiator: user` per
/// human turn, then `X-Initiator: agent` for every follow-up tool round-trip.
pub fn classify_initiator(body: &Value) -> &'static str {
  let Some(msgs) = body.get("messages").and_then(|v| v.as_array()) else {
    return "user";
  };
  for m in msgs.iter().rev() {
    match m.get("role").and_then(|r| r.as_str()) {
      Some("system") => continue,
      Some("tool") => return "agent",
      Some("assistant") => return "agent",
      Some("user") => return "user",
      _ => return "agent",
    }
  }
  "user"
}

/// Responses-API variant of [`classify_initiator`].
///
/// The OpenAI Responses request body uses `input` (a string OR an array of
/// input items: `{type, role, content}`) rather than `messages`. We walk the
/// array tail-first with the same rules:
/// - last non-system role is `tool`/`function_call_output` → "agent"
/// - last non-system role is `assistant`                   → "agent"
/// - last non-system role is `user`                        → "user"
/// - bare-string input or empty array                      → "user"
/// - unknown shape                                         → "agent"
pub fn classify_initiator_responses(body: &Value) -> &'static str {
  let Some(input) = body.get("input") else {
    return "user";
  };
  // String form is by definition a fresh user prompt.
  if input.is_string() {
    return "user";
  }
  let Some(items) = input.as_array() else {
    return "agent";
  };
  for it in items.iter().rev() {
    // Responses items can be plain `{role, content}` messages or typed
    // items (`{type:"function_call_output", ...}`). Treat both.
    let typ = it.get("type").and_then(|t| t.as_str());
    if let Some(t) = typ {
      match t {
        "function_call_output" | "tool_result" | "computer_call_output" => return "agent",
        "function_call" | "tool_call" | "reasoning" => return "agent",
        "message" => {} // fall through to role check below
        _ => return "agent",
      }
    }
    match it.get("role").and_then(|r| r.as_str()) {
      Some("system") | Some("developer") => continue,
      Some("tool") => return "agent",
      Some("assistant") => return "agent",
      Some("user") => return "user",
      _ => return "agent",
    }
  }
  "user"
}

#[cfg(test)]
mod responses_tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn bare_string_input_is_user() {
    let b = json!({ "input": "hello" });
    assert_eq!(classify_initiator_responses(&b), "user");
  }

  #[test]
  fn missing_input_defaults_to_user() {
    assert_eq!(classify_initiator_responses(&json!({})), "user");
  }

  #[test]
  fn user_message_array_is_user() {
    let b = json!({ "input": [
        { "role": "system", "content": "x" },
        { "role": "user", "content": "hi" }
    ]});
    assert_eq!(classify_initiator_responses(&b), "user");
  }

  #[test]
  fn tool_followup_is_agent() {
    let b = json!({ "input": [
        { "role": "user", "content": "x" },
        { "type": "function_call", "name": "f" },
        { "type": "function_call_output", "output": "42" }
    ]});
    assert_eq!(classify_initiator_responses(&b), "agent");
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn user_turn() {
    let b = json!({"messages":[
        {"role":"system","content":"x"},
        {"role":"user","content":"hi"}
    ]});
    assert_eq!(classify_initiator(&b), "user");
  }

  #[test]
  fn tool_followup_is_agent() {
    let b = json!({"messages":[
        {"role":"user","content":"do x"},
        {"role":"assistant","tool_calls":[{"id":"1"}]},
        {"role":"tool","tool_call_id":"1","content":"42"}
    ]});
    assert_eq!(classify_initiator(&b), "agent");
  }

  #[test]
  fn after_assistant_is_agent() {
    // model finished but caller is asking for a continuation
    let b = json!({"messages":[
        {"role":"user","content":"hi"},
        {"role":"assistant","content":"ok"}
    ]});
    assert_eq!(classify_initiator(&b), "agent");
  }

  #[test]
  fn empty_defaults_to_user() {
    let b = json!({});
    assert_eq!(classify_initiator(&b), "user");
  }
}
