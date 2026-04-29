use serde_json::Value;

/// Classify a chat-like request as a fresh user turn ("user") or as a
/// continuation of an in-flight tool-use loop ("agent").
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
pub fn classify_initiator_responses(body: &Value) -> &'static str {
  let Some(input) = body.get("input") else {
    return "user";
  };
  if input.is_string() {
    return "user";
  }
  let Some(items) = input.as_array() else {
    return "agent";
  };
  for it in items.iter().rev() {
    let typ = it.get("type").and_then(|t| t.as_str());
    if let Some(t) = typ {
      match t {
        "function_call_output" | "tool_result" | "computer_call_output" => return "agent",
        "function_call" | "tool_call" | "reasoning" => return "agent",
        "message" => {}
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
