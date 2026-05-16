use serde::{Deserialize, Serialize};
use serde_json::Value;

use llm_endpoint_core::Extras;
pub use llm_endpoint_core::ToolChoice as ResponsesToolChoice;

use crate::item::InputItem;
use crate::parameters::{ResponsesExtraParameters, ResponsesRequestParameters};

/// `input` field accepts either a plain string or a list of items.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ResponsesInput {
  Text(String),
  Items(Vec<InputItem>),
}

impl Default for ResponsesInput {
  fn default() -> Self {
    Self::Items(Vec::new())
  }
}

/// Request body for `POST /v1/responses`.
///
/// Behavior knobs (temperature, top_p, max_*_tokens, tool_choice,
/// reasoning, etc.) live on the embedded
/// [`ResponsesRequestParameters`]; this struct keeps content,
/// streaming controls and structured payloads at the top level.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponsesRequest {
  pub model: String,
  pub input: ResponsesInput,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub instructions: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stream: Option<bool>,
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub tools: Vec<ResponsesToolDef>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub stop: Option<Value>,
  /// Optional list of additional fields to include in the response
  /// (e.g. `"reasoning.encrypted_content"`, `"file_search_call.results"`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub include: Option<Vec<String>>,
  /// Free-form per-request metadata echoed back by some providers.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub client_metadata: Option<Value>,
  #[serde(default, flatten)]
  pub params: ResponsesRequestParameters,
  #[serde(default, flatten)]
  pub extra_params: ResponsesExtraParameters,
  #[serde(default, flatten)]
  pub extras: Extras,
}

/// `tools[]` entry. The Responses API permits multiple tool kinds
/// (function, web_search, file_search, custom, etc.). For function tools
/// the standard fields are typed directly; non-function tools leave
/// those fields as `None` and use `extras` for kind-specific data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ResponsesToolDef {
  #[serde(rename = "type")]
  pub kind: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub name: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub description: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parameters: Option<Value>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub strict: Option<bool>,
  #[serde(default, flatten)]
  pub extras: Extras,
}
