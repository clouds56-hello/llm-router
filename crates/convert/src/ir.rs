use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IrRequest {
  pub model: String,
  pub system: Option<String>,
  pub messages: Vec<IrMessage>,
  pub tools: Vec<Value>,
  pub tool_choice: Option<Value>,
  pub sampling: Sampling,
  pub reasoning: Option<Value>,
  pub stream: bool,
  pub extras: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IrMessage {
  pub role: Role,
  pub content: Vec<ContentPart>,
  pub tool_call_id: Option<String>,
  pub name: Option<String>,
  pub raw: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
  System,
  User,
  Assistant,
  Tool,
  Other(String),
}

impl Role {
  pub fn from_str(s: &str) -> Self {
    match s {
      "system" => Self::System,
      "user" => Self::User,
      "assistant" => Self::Assistant,
      "tool" => Self::Tool,
      other => Self::Other(other.to_string()),
    }
  }

  pub fn as_str(&self) -> &str {
    match self {
      Self::System => "system",
      Self::User => "user",
      Self::Assistant => "assistant",
      Self::Tool => "tool",
      Self::Other(s) => s.as_str(),
    }
  }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContentPart {
  Text { text: String },
  Reasoning { text: String },
  ToolCall { call: ToolCall },
  ToolResult { id: Option<String>, content: Value },
  Raw { value: Value },
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ToolCall {
  pub id: Option<String>,
  pub name: String,
  pub arguments: Value,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sampling {
  pub temperature: Option<f64>,
  pub top_p: Option<f64>,
  pub max_output_tokens: Option<u64>,
  pub stop: Option<Value>,
  pub n: Option<u64>,
  pub seed: Option<i64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct IrResponse {
  pub id: Option<String>,
  pub model: Option<String>,
  pub role: Option<Role>,
  pub content: Vec<ContentPart>,
  pub tool_calls: Vec<ToolCall>,
  pub usage: Option<Usage>,
  pub finish_reason: Option<String>,
  pub extras: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
  pub input_tokens: Option<u64>,
  pub output_tokens: Option<u64>,
  pub total_tokens: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum IrDelta {
  Text(String),
  Reasoning(String),
  ToolCall {
    index: usize,
    id: Option<String>,
    name: Option<String>,
    arguments_delta: String,
  },
  Usage(Usage),
  Finish(Option<String>),
}

impl IrResponse {
  pub fn push_delta(&mut self, delta: IrDelta) {
    match delta {
      IrDelta::Text(text) => push_text_part(&mut self.content, text),
      IrDelta::Reasoning(text) => push_reasoning_part(&mut self.content, text),
      IrDelta::ToolCall {
        index,
        id,
        name,
        arguments_delta,
      } => {
        while self.tool_calls.len() <= index {
          self.tool_calls.push(ToolCall::default());
        }
        let call = &mut self.tool_calls[index];
        if id.is_some() {
          call.id = id;
        }
        if let Some(name) = name {
          call.name = name;
        }
        let mut current = call.arguments.as_str().unwrap_or_default().to_string();
        current.push_str(&arguments_delta);
        call.arguments = Value::String(current);
      }
      IrDelta::Usage(usage) => self.usage = Some(usage),
      IrDelta::Finish(reason) => self.finish_reason = reason,
    }
  }
}

fn push_text_part(parts: &mut Vec<ContentPart>, text: String) {
  if text.is_empty() {
    return;
  }
  if let Some(ContentPart::Text { text: existing }) = parts.last_mut() {
    existing.push_str(&text);
  } else {
    parts.push(ContentPart::Text { text });
  }
}

fn push_reasoning_part(parts: &mut Vec<ContentPart>, text: String) {
  if text.is_empty() {
    return;
  }
  if let Some(ContentPart::Reasoning { text: existing }) = parts.last_mut() {
    existing.push_str(&text);
  } else {
    parts.push(ContentPart::Reasoning { text });
  }
}

pub fn text_from_parts(parts: &[ContentPart]) -> String {
  parts
    .iter()
    .filter_map(|p| match p {
      ContentPart::Text { text } => Some(text.as_str()),
      _ => None,
    })
    .collect::<Vec<_>>()
    .join("")
}

pub fn reasoning_from_parts(parts: &[ContentPart]) -> Option<String> {
  let text = parts
    .iter()
    .filter_map(|p| match p {
      ContentPart::Reasoning { text } => Some(text.as_str()),
      _ => None,
    })
    .collect::<Vec<_>>()
    .join("");
  (!text.is_empty()).then_some(text)
}

pub fn usage_from_openai(v: &Value) -> Option<Usage> {
  let u = v.get("usage")?;
  Some(Usage {
    input_tokens: u
      .get("prompt_tokens")
      .or_else(|| u.get("input_tokens"))
      .and_then(Value::as_u64),
    output_tokens: u
      .get("completion_tokens")
      .or_else(|| u.get("output_tokens"))
      .and_then(Value::as_u64),
    total_tokens: u.get("total_tokens").and_then(Value::as_u64),
  })
}

pub fn usage_to_chat(usage: &Usage) -> Value {
  serde_json::json!({
    "prompt_tokens": usage.input_tokens.unwrap_or(0),
    "completion_tokens": usage.output_tokens.unwrap_or(0),
    "total_tokens": usage.total_tokens.unwrap_or_else(|| usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0)),
  })
}

pub fn usage_to_io(usage: &Usage) -> Value {
  serde_json::json!({
    "input_tokens": usage.input_tokens.unwrap_or(0),
    "output_tokens": usage.output_tokens.unwrap_or(0),
    "total_tokens": usage.total_tokens.unwrap_or_else(|| usage.input_tokens.unwrap_or(0) + usage.output_tokens.unwrap_or(0)),
  })
}

pub fn extras_from_object(obj: &Map<String, Value>, known: &[&str]) -> BTreeMap<String, Value> {
  obj
    .iter()
    .filter(|(k, _)| !known.contains(&k.as_str()))
    .map(|(k, v)| (k.clone(), v.clone()))
    .collect()
}

pub fn insert_opt_f64(out: &mut Map<String, Value>, key: &str, value: Option<f64>) {
  if let Some(value) = value.and_then(serde_json::Number::from_f64) {
    out.insert(key.into(), Value::Number(value));
  }
}

pub fn insert_opt_u64(out: &mut Map<String, Value>, key: &str, value: Option<u64>) {
  if let Some(value) = value {
    out.insert(key.into(), Value::Number(value.into()));
  }
}
