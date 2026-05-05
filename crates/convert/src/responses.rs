use super::chat::args_to_string;
use super::error::{ConvertError, Result};
use super::ir::*;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const REQUEST_KEYS: &[&str] = &[
  "model",
  "input",
  "instructions",
  "tools",
  "tool_choice",
  "temperature",
  "top_p",
  "max_output_tokens",
  "max_tokens",
  "stop",
  "stream",
  "reasoning",
  "metadata",
];

pub fn request_from_value(v: &Value) -> Result<IrRequest> {
  let obj = v
    .as_object()
    .ok_or_else(|| ConvertError::bad_shape("body", "expected object"))?;
  let model = obj
    .get("model")
    .and_then(Value::as_str)
    .unwrap_or("unknown")
    .to_string();
  let input = obj.get("input").ok_or(ConvertError::MissingField { field: "input" })?;
  let messages = input_to_messages(input)?;
  Ok(IrRequest {
    model,
    system: obj.get("instructions").and_then(Value::as_str).map(str::to_string),
    messages,
    tools: obj.get("tools").and_then(Value::as_array).cloned().unwrap_or_default(),
    tool_choice: obj.get("tool_choice").cloned(),
    sampling: Sampling {
      temperature: obj.get("temperature").and_then(Value::as_f64),
      top_p: obj.get("top_p").and_then(Value::as_f64),
      max_output_tokens: obj
        .get("max_output_tokens")
        .or_else(|| obj.get("max_tokens"))
        .and_then(Value::as_u64),
      stop: obj.get("stop").cloned(),
      n: None,
      seed: None,
    },
    reasoning: obj.get("reasoning").cloned(),
    stream: obj.get("stream").and_then(Value::as_bool).unwrap_or(false),
    extras: extras_from_object(obj, REQUEST_KEYS),
  })
}

pub fn request_to_value(req: &IrRequest) -> Result<Value> {
  let mut out = Map::new();
  out.insert("model".into(), Value::String(req.model.clone()));
  if let Some(system) = &req.system {
    out.insert("instructions".into(), Value::String(system.clone()));
  }
  out.insert(
    "input".into(),
    Value::Array(req.messages.iter().map(message_to_responses_input).collect()),
  );
  if !req.tools.is_empty() {
    out.insert("tools".into(), Value::Array(req.tools.clone()));
  }
  if let Some(v) = &req.tool_choice {
    out.insert("tool_choice".into(), v.clone());
  }
  insert_opt_f64(&mut out, "temperature", req.sampling.temperature);
  insert_opt_f64(&mut out, "top_p", req.sampling.top_p);
  insert_opt_u64(&mut out, "max_output_tokens", req.sampling.max_output_tokens);
  if let Some(v) = &req.sampling.stop {
    out.insert("stop".into(), v.clone());
  }
  if let Some(v) = &req.reasoning {
    out.insert("reasoning".into(), v.clone());
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
  if let Some(text) = v.get("output_text").and_then(Value::as_str) {
    content.push(ContentPart::Text { text: text.to_string() });
  }
  if let Some(output) = v.get("output").and_then(Value::as_array) {
    for item in output {
      match item.get("type").and_then(Value::as_str) {
        Some("message") => {
          if let Some(parts) = item.get("content").and_then(Value::as_array) {
            for part in parts {
              match part.get("type").and_then(Value::as_str) {
                Some("output_text") | Some("text") => content.push(ContentPart::Text {
                  text: part.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
                }),
                Some("reasoning") => content.push(ContentPart::Reasoning {
                  text: part
                    .get("summary")
                    .or_else(|| part.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                }),
                _ => content.push(ContentPart::Raw { value: part.clone() }),
              }
            }
          }
        }
        Some("function_call") => tool_calls.push(ToolCall {
          id: item
            .get("call_id")
            .or_else(|| item.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string),
          name: item.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
          arguments: item
            .get("arguments")
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(|| item.get("arguments").cloned().unwrap_or(Value::Null)),
        }),
        _ => {}
      }
    }
  }
  Ok(IrResponse {
    id: v.get("id").and_then(Value::as_str).map(str::to_string),
    model: v.get("model").and_then(Value::as_str).map(str::to_string),
    role: Some(Role::Assistant),
    content,
    tool_calls,
    usage: v.get("usage").map(|u| Usage {
      input_tokens: u
        .get("input_tokens")
        .or_else(|| u.get("prompt_tokens"))
        .and_then(Value::as_u64),
      output_tokens: u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"))
        .and_then(Value::as_u64),
      total_tokens: u.get("total_tokens").and_then(Value::as_u64),
    }),
    finish_reason: v.get("status").and_then(Value::as_str).map(str::to_string),
    extras: BTreeMap::new(),
  })
}

pub fn response_to_value(resp: &IrResponse) -> Result<Value> {
  let id = resp.id.clone().unwrap_or_else(|| "resp_converted".into());
  let mut output = Vec::new();
  let mut content = Vec::new();
  let text = text_from_parts(&resp.content);
  if !text.is_empty() {
    content.push(json!({ "type": "output_text", "text": text, "annotations": [] }));
  }
  if let Some(reasoning) = reasoning_from_parts(&resp.content) {
    content.push(json!({ "type": "reasoning", "summary": reasoning }));
  }
  if !content.is_empty() {
    output.push(json!({ "type": "message", "id": format!("msg_{id}"), "status": "completed", "role": "assistant", "content": content }));
  }
  for call in &resp.tool_calls {
    output.push(json!({
      "type": "function_call",
      "id": call.id.clone().unwrap_or_else(|| "fc_converted".into()),
      "call_id": call.id.clone().unwrap_or_else(|| "call_converted".into()),
      "name": call.name,
      "arguments": args_to_string(&call.arguments),
      "status": "completed",
    }));
  }
  let mut out = json!({
    "id": id,
    "object": "response",
    "status": "completed",
    "model": resp.model.clone().unwrap_or_default(),
    "output": output,
    "output_text": text_from_parts(&resp.content),
  });
  if let Some(usage) = &resp.usage {
    out["usage"] = usage_to_io(usage);
  }
  Ok(out)
}

pub fn delta_from_responses_event(v: &Value) -> Vec<IrDelta> {
  let mut out = Vec::new();
  match v.get("type").and_then(Value::as_str) {
    Some("response.output_text.delta") => {
      if let Some(delta) = v.get("delta").and_then(Value::as_str) {
        out.push(IrDelta::Text(delta.to_string()));
      }
    }
    Some("response.reasoning_summary_text.delta") | Some("response.reasoning_text.delta") => {
      if let Some(delta) = v.get("delta").and_then(Value::as_str) {
        out.push(IrDelta::Reasoning(delta.to_string()));
      }
    }
    Some("response.function_call_arguments.delta") => out.push(IrDelta::ToolCall {
      index: v.get("output_index").and_then(Value::as_u64).unwrap_or(0) as usize,
      id: None,
      name: None,
      arguments_delta: v.get("delta").and_then(Value::as_str).unwrap_or_default().to_string(),
    }),
    Some("response.completed") => {
      if let Some(resp) = v.get("response") {
        if let Some(usage) = resp.get("usage").map(|u| Usage {
          input_tokens: u
            .get("input_tokens")
            .or_else(|| u.get("prompt_tokens"))
            .and_then(Value::as_u64),
          output_tokens: u
            .get("output_tokens")
            .or_else(|| u.get("completion_tokens"))
            .and_then(Value::as_u64),
          total_tokens: u.get("total_tokens").and_then(Value::as_u64),
        }) {
          out.push(IrDelta::Usage(usage));
        }
      }
      out.push(IrDelta::Finish(Some("stop".into())));
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
        "response.output_text.delta".into(),
        json!({ "type": "response.output_text.delta", "response_id": resp_id, "output_index": 0, "content_index": 0, "delta": text }),
      )),
      IrDelta::Reasoning(text) => events.push((
        "response.reasoning_text.delta".into(),
        json!({ "type": "response.reasoning_text.delta", "response_id": resp_id, "output_index": 0, "content_index": 0, "delta": text }),
      )),
      IrDelta::ToolCall { index, arguments_delta, .. } => events.push((
        "response.function_call_arguments.delta".into(),
        json!({ "type": "response.function_call_arguments.delta", "response_id": resp_id, "output_index": index, "delta": arguments_delta }),
      )),
      IrDelta::Usage(_) => {}
      IrDelta::Finish(_) => {}
    }
  }
  if finish {
    events.push((
      "response.completed".into(),
      json!({
        "type": "response.completed",
        "response": { "id": resp_id, "object": "response", "status": "completed", "model": model }
      }),
    ));
  }
  events
}

fn input_to_messages(input: &Value) -> Result<Vec<IrMessage>> {
  if let Some(s) = input.as_str() {
    return Ok(vec![IrMessage {
      role: Role::User,
      content: vec![ContentPart::Text { text: s.to_string() }],
      tool_call_id: None,
      name: None,
      raw: None,
    }]);
  }
  let arr = input
    .as_array()
    .ok_or_else(|| ConvertError::bad_shape("input", "expected string or array"))?;
  arr.iter().map(input_item_to_message).collect()
}

fn input_item_to_message(item: &Value) -> Result<IrMessage> {
  let role = Role::from_str(item.get("role").and_then(Value::as_str).unwrap_or("user"));
  let content = match item.get("content") {
    Some(Value::String(s)) => vec![ContentPart::Text { text: s.clone() }],
    Some(Value::Array(parts)) => parts.iter().map(part_from_responses).collect(),
    Some(v) => vec![ContentPart::Raw { value: v.clone() }],
    None => Vec::new(),
  };
  Ok(IrMessage {
    role,
    content,
    tool_call_id: item.get("call_id").and_then(Value::as_str).map(str::to_string),
    name: item.get("name").and_then(Value::as_str).map(str::to_string),
    raw: None,
  })
}

fn part_from_responses(v: &Value) -> ContentPart {
  match v.get("type").and_then(Value::as_str) {
    Some("input_text") | Some("output_text") | Some("text") => ContentPart::Text {
      text: v.get("text").and_then(Value::as_str).unwrap_or_default().to_string(),
    },
    _ => ContentPart::Raw { value: v.clone() },
  }
}

fn message_to_responses_input(msg: &IrMessage) -> Value {
  let parts: Vec<_> = msg
    .content
    .iter()
    .filter_map(|p| match p {
      ContentPart::Text { text } => Some(json!({ "type": "input_text", "text": text })),
      ContentPart::Raw { value } => Some(value.clone()),
      _ => None,
    })
    .collect();
  json!({ "role": msg.role.as_str(), "content": parts })
}
