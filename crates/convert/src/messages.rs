use super::chat::args_to_string;
use super::error::{ConvertError, Result};
use super::ir::*;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const REQUEST_KEYS: &[&str] = &[
  "model",
  "system",
  "messages",
  "tools",
  "tool_choice",
  "temperature",
  "top_p",
  "max_tokens",
  "stop_sequences",
  "stream",
  "thinking",
  "metadata",
];

pub fn request_from_value(v: &Value) -> Result<IrRequest> {
  let obj = v
    .as_object()
    .ok_or_else(|| ConvertError::bad_shape("body", "expected object"))?;
  let messages = obj
    .get("messages")
    .and_then(Value::as_array)
    .ok_or(ConvertError::MissingField { field: "messages" })?
    .iter()
    .map(message_from_messages)
    .collect::<Result<Vec<_>>>()?;
  Ok(IrRequest {
    model: obj
      .get("model")
      .and_then(Value::as_str)
      .unwrap_or("unknown")
      .to_string(),
    system: system_to_string(obj.get("system")),
    messages,
    tools: obj.get("tools").and_then(Value::as_array).cloned().unwrap_or_default(),
    tool_choice: obj.get("tool_choice").cloned(),
    sampling: Sampling {
      temperature: obj.get("temperature").and_then(Value::as_f64),
      top_p: obj.get("top_p").and_then(Value::as_f64),
      max_output_tokens: obj.get("max_tokens").and_then(Value::as_u64),
      stop: obj.get("stop_sequences").cloned(),
      n: None,
      seed: None,
    },
    reasoning: obj.get("thinking").cloned(),
    stream: obj.get("stream").and_then(Value::as_bool).unwrap_or(false),
    extras: extras_from_object(obj, REQUEST_KEYS),
  })
}

pub fn request_to_value(req: &IrRequest) -> Result<Value> {
  let mut out = Map::new();
  out.insert("model".into(), Value::String(req.model.clone()));
  if let Some(system) = &req.system {
    out.insert("system".into(), Value::String(system.clone()));
  }
  out.insert(
    "messages".into(),
    Value::Array(req.messages.iter().map(message_to_messages).collect()),
  );
  if !req.tools.is_empty() {
    out.insert("tools".into(), Value::Array(req.tools.clone()));
  }
  if let Some(v) = &req.tool_choice {
    out.insert("tool_choice".into(), v.clone());
  }
  insert_opt_f64(&mut out, "temperature", req.sampling.temperature);
  insert_opt_f64(&mut out, "top_p", req.sampling.top_p);
  insert_opt_u64(&mut out, "max_tokens", req.sampling.max_output_tokens);
  if let Some(v) = &req.sampling.stop {
    out.insert("stop_sequences".into(), v.clone());
  }
  if let Some(v) = &req.reasoning {
    out.insert("thinking".into(), v.clone());
  }
  if req.stream {
    out.insert("stream".into(), Value::Bool(true));
  }
  for (k, v) in &req.extras {
    out.entry(k.clone()).or_insert_with(|| v.clone());
  }
  Ok(Value::Object(out))
}

pub fn response_from_value(v: &Value) -> Result<IrResponse> {
  let mut content = Vec::new();
  let mut tool_calls = Vec::new();
  if let Some(parts) = v.get("content").and_then(Value::as_array) {
    for part in parts {
      match part.get("type").and_then(Value::as_str) {
        Some("text") => content.push(ContentPart::Text {
          text: part.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
        }),
        Some("thinking") | Some("redacted_thinking") => content.push(ContentPart::Reasoning {
          text: part
            .get("thinking")
            .or_else(|| part.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        }),
        Some("tool_use") => tool_calls.push(ToolCall {
          id: part.get("id").and_then(Value::as_str).map(str::to_string),
          name: part.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
          arguments: part.get("input").cloned().unwrap_or(Value::Null),
        }),
        _ => content.push(ContentPart::Raw { value: part.clone() }),
      }
    }
  }
  Ok(IrResponse {
    id: v.get("id").and_then(Value::as_str).map(str::to_string),
    model: v.get("model").and_then(Value::as_str).map(str::to_string),
    role: v.get("role").and_then(Value::as_str).map(Role::from_str),
    content,
    tool_calls,
    usage: v.get("usage").map(|u| Usage {
      input_tokens: u.get("input_tokens").and_then(Value::as_u64),
      output_tokens: u.get("output_tokens").and_then(Value::as_u64),
      total_tokens: None,
    }),
    finish_reason: v.get("stop_reason").and_then(Value::as_str).map(str::to_string),
    extras: BTreeMap::new(),
  })
}

pub fn response_to_value(resp: &IrResponse) -> Result<Value> {
  let mut content = Vec::new();
  let text = text_from_parts(&resp.content);
  if !text.is_empty() {
    content.push(json!({ "type": "text", "text": text }));
  }
  if let Some(reasoning) = reasoning_from_parts(&resp.content) {
    content.push(json!({ "type": "thinking", "thinking": reasoning }));
  }
  for call in &resp.tool_calls {
    content.push(json!({
      "type": "tool_use",
      "id": call.id.clone().unwrap_or_else(|| "toolu_converted".into()),
      "name": call.name,
      "input": call.arguments,
    }));
  }
  let mut out = json!({
    "id": resp.id.clone().unwrap_or_else(|| "msg_converted".into()),
    "type": "message",
    "role": "assistant",
    "model": resp.model.clone().unwrap_or_default(),
    "content": content,
    "stop_reason": resp.finish_reason.clone().unwrap_or_else(|| "end_turn".into()),
    "stop_sequence": null,
  });
  if let Some(usage) = &resp.usage {
    out["usage"] = json!({
      "input_tokens": usage.input_tokens.unwrap_or(0),
      "output_tokens": usage.output_tokens.unwrap_or(0),
    });
  }
  Ok(out)
}

pub fn delta_from_messages_event(v: &Value) -> Vec<IrDelta> {
  let mut out = Vec::new();
  match v.get("type").and_then(Value::as_str) {
    Some("content_block_delta") => {
      if let Some(delta) = v.get("delta") {
        match delta.get("type").and_then(Value::as_str) {
          Some("text_delta") => {
            if let Some(text) = delta.get("text").and_then(Value::as_str) {
              out.push(IrDelta::Text(text.to_string()));
            }
          }
          Some("thinking_delta") => {
            if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
              out.push(IrDelta::Reasoning(text.to_string()));
            }
          }
          Some("input_json_delta") => out.push(IrDelta::ToolCall {
            index: v.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
            id: None,
            name: None,
            arguments_delta: delta
              .get("partial_json")
              .and_then(Value::as_str)
              .unwrap_or_default()
              .to_string(),
          }),
          _ => {}
        }
      }
    }
    Some("message_delta") => {
      if let Some(stop) = v
        .get("delta")
        .and_then(|d| d.get("stop_reason"))
        .and_then(Value::as_str)
      {
        out.push(IrDelta::Finish(Some(stop.to_string())));
      }
      if let Some(u) = v.get("usage") {
        out.push(IrDelta::Usage(Usage {
          input_tokens: None,
          output_tokens: u.get("output_tokens").and_then(Value::as_u64),
          total_tokens: None,
        }));
      }
    }
    Some("message_start") => {
      if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
        out.push(IrDelta::Usage(Usage {
          input_tokens: u.get("input_tokens").and_then(Value::as_u64),
          output_tokens: u.get("output_tokens").and_then(Value::as_u64),
          total_tokens: None,
        }));
      }
    }
    _ => {}
  }
  out
}

pub fn events_from_deltas(resp_id: &str, model: &str, deltas: &[IrDelta], finish: bool) -> Vec<(String, Value)> {
  let mut events = Vec::new();
  for delta in deltas {
    match delta {
      IrDelta::Text(text) => events.push((
        "content_block_delta".into(),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
      )),
      IrDelta::Reasoning(text) => events.push((
        "content_block_delta".into(),
        json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": text } }),
      )),
      IrDelta::ToolCall { index, arguments_delta, .. } => events.push((
        "content_block_delta".into(),
        json!({ "type": "content_block_delta", "index": index, "delta": { "type": "input_json_delta", "partial_json": arguments_delta } }),
      )),
      IrDelta::Usage(usage) => events.push((
        "message_delta".into(),
        json!({ "type": "message_delta", "delta": {}, "usage": { "output_tokens": usage.output_tokens.unwrap_or(0) } }),
      )),
      IrDelta::Finish(reason) => events.push((
        "message_delta".into(),
        json!({ "type": "message_delta", "delta": { "stop_reason": reason.clone().unwrap_or_else(|| "end_turn".into()), "stop_sequence": null } }),
      )),
    }
  }
  if finish {
    events.insert(
      0,
      (
        "message_start".into(),
        json!({
          "type": "message_start",
          "message": { "id": resp_id, "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": null, "stop_sequence": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }
        }),
      ),
    );
    events.push((
      "content_block_stop".into(),
      json!({ "type": "content_block_stop", "index": 0 }),
    ));
    events.push(("message_stop".into(), json!({ "type": "message_stop" })));
  }
  events
}

fn message_from_messages(v: &Value) -> Result<IrMessage> {
  let role = Role::from_str(v.get("role").and_then(Value::as_str).unwrap_or("user"));
  Ok(IrMessage {
    role,
    content: content_from_messages(v.get("content")),
    tool_call_id: None,
    name: None,
    raw: None,
  })
}

fn content_from_messages(content: Option<&Value>) -> Vec<ContentPart> {
  match content {
    Some(Value::String(s)) => vec![ContentPart::Text { text: s.clone() }],
    Some(Value::Array(parts)) => parts.iter().map(part_from_messages).collect(),
    Some(v) => vec![ContentPart::Raw { value: v.clone() }],
    None => Vec::new(),
  }
}

fn part_from_messages(v: &Value) -> ContentPart {
  match v.get("type").and_then(Value::as_str) {
    Some("text") => ContentPart::Text {
      text: v.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
    },
    Some("thinking") | Some("redacted_thinking") => ContentPart::Reasoning {
      text: v
        .get("thinking")
        .or_else(|| v.get("text"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string(),
    },
    Some("tool_use") => ContentPart::ToolCall {
      call: ToolCall {
        id: v.get("id").and_then(Value::as_str).map(str::to_string),
        name: v.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
        arguments: v.get("input").cloned().unwrap_or(Value::Null),
      },
    },
    Some("tool_result") => ContentPart::ToolResult {
      id: v.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
      content: v.get("content").cloned().unwrap_or(Value::Null),
    },
    _ => ContentPart::Raw { value: v.clone() },
  }
}

fn message_to_messages(msg: &IrMessage) -> Value {
  let content: Vec<_> = msg.content.iter().map(part_to_messages).collect();
  json!({ "role": msg.role.as_str(), "content": content })
}

fn part_to_messages(part: &ContentPart) -> Value {
  match part {
    ContentPart::Text { text } => json!({ "type": "text", "text": text }),
    ContentPart::Reasoning { text } => json!({ "type": "thinking", "thinking": text }),
    ContentPart::ToolCall { call } => json!({
      "type": "tool_use",
      "id": call.id.clone().unwrap_or_else(|| "toolu_converted".into()),
      "name": call.name,
      "input": call.arguments,
    }),
    ContentPart::ToolResult { id, content } => json!({
      "type": "tool_result",
      "tool_use_id": id,
      "content": content,
    }),
    ContentPart::Raw { value } => value.clone(),
  }
}

fn system_to_string(system: Option<&Value>) -> Option<String> {
  match system {
    Some(Value::String(s)) => Some(s.clone()),
    Some(Value::Array(parts)) => {
      let text = parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n\n");
      (!text.is_empty()).then_some(text)
    }
    _ => None,
  }
}

#[allow(dead_code)]
fn _tool_args_string(call: &ToolCall) -> String {
  args_to_string(&call.arguments)
}
