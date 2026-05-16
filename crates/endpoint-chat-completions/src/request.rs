use serde::{Deserialize, Serialize};
use serde_json::Value;

use llm_endpoint_core::Extras;
pub use llm_endpoint_core::ToolChoice as ChatToolChoice;

use crate::message::ChatMessage;
use crate::parameters::{ChatExtraParameters, ChatRequestParameters};

/// Request body for `POST /v1/chat/completions`.
///
/// Behavior knobs (temperature, top_p, max_*_tokens, tool_choice,
/// reasoning, thinking, etc.) live on the embedded
/// [`ChatRequestParameters`]; structured payloads (messages, tools,
/// stop) and streaming controls stay at the top level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatRequest {
  pub model: String,
  pub messages: Vec<ChatMessage>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stream: Option<bool>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tools: Vec<ChatToolDef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stop: Option<Value>,
  #[serde(default, flatten)]
  pub params: ChatRequestParameters,
  #[serde(default, flatten)]
  pub extra_params: ChatExtraParameters,
  #[serde(default, flatten)]
  pub extras: Extras,
}

/// A `tools[]` entry. Chat Completions only defines function tools.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatToolDef {
  #[serde(rename = "type", default = "default_function_type")]
  pub kind: String,
  pub function: Value,
  #[serde(default, flatten)]
  pub extras: Extras,
}

fn default_function_type() -> String {
  "function".into()
}
