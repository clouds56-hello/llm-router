use serde::{Deserialize, Serialize};
use serde_json::Value;

use llm_endpoint_core::Extras;

use crate::content::ContentBlock;
use crate::message::Message;
use crate::parameters::{MessagesExtraParameters, MessagesRequestParameters};

/// `system` accepts either a single string or an array of content
/// blocks (typically `text` blocks).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
  Text(String),
  Blocks(Vec<ContentBlock>),
}

/// Request body for `POST /v1/messages`.
///
/// Behavior knobs (temperature, top_p, top_k, service_tier,
/// tool_choice, thinking) live on the embedded
/// [`MessagesRequestParameters`]; structured payloads, content,
/// `max_tokens` (required) and streaming controls stay at the top
/// level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessagesRequest {
  pub model: String,
  pub messages: Vec<Message>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub system: Option<SystemPrompt>,
  pub max_tokens: u64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stream: Option<bool>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tools: Vec<MessagesToolDef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stop_sequences: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub metadata: Option<Value>,
  #[serde(default, flatten)]
  pub params: MessagesRequestParameters,
  #[serde(default, flatten)]
  pub extra_params: MessagesExtraParameters,
  #[serde(default, flatten)]
  pub extras: Extras,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessagesToolDef {
  pub name: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub input_schema: Option<Value>,
  #[serde(default, flatten)]
  pub extras: Extras,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessagesToolChoice {
  Mode(Value),
  Named(Value),
}
