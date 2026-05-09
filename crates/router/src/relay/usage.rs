use llm_core::db::{Usage, UsageDetails};
use serde_json::Value;

/// Extract `Usage` from an upstream response body. Handles three shapes:
///
/// - **OpenAI chat completions**:
///   `usage.{prompt_tokens, completion_tokens, prompt_tokens_details.cached_tokens,
///   completion_tokens_details.reasoning_tokens}`
/// - **OpenAI responses / Codex**:
///   `usage.{input_tokens, output_tokens, input_tokens_details.cached_tokens,
///   output_tokens_details.reasoning_tokens}`
/// - **Anthropic** (messages): `message.usage.{input_tokens, output_tokens,
///   cache_creation_input_tokens, cache_read_input_tokens}`. The Anthropic
///   `input_tokens` field excludes cached portions, so we normalize
///   `Usage.input_tokens` to the total (input + cache_creation + cache_read).
/// - **OpenAI Responses API**: `response.usage.{input_tokens, output_tokens, ...}`
///
/// Returns an empty `Usage` (all `None`) when no recognizable shape is found.
pub(crate) fn parse_usage_any_value(v: &Value) -> Usage {
  // Anthropic streaming shape: message.usage
  if let Some(usage) = parse_anthropic_usage(v.pointer("/message/usage")) {
    return usage;
  }
  // OpenAI Responses API: response.usage
  if let Some(usage) = parse_openai_responses_usage(v.pointer("/response/usage")) {
    return usage;
  }
  // Try OpenAI shape: top-level "usage"
  if let Some(usage) = parse_openai_chat_usage(v.pointer("/usage")) {
    return usage;
  }
  if let Some(usage) = parse_openai_responses_usage(v.pointer("/usage")) {
    return usage;
  }
  // Could be Anthropic-style at top-level "usage" too
  if let Some(usage) = parse_anthropic_usage(v.pointer("/usage")) {
    return usage;
  }
  Usage::default()
}

fn ptr_u64(v: Option<&Value>, path: &str) -> Option<u64> {
  v.and_then(|value| value.pointer(path)).and_then(Value::as_u64)
}

/// Parse OpenAI chat-completions usage block.
fn parse_openai_chat_usage(u: Option<&Value>) -> Option<Usage> {
  let input_tokens = ptr_u64(u, "/prompt_tokens");
  let output_tokens = ptr_u64(u, "/completion_tokens");
  if input_tokens.is_none() && output_tokens.is_none() {
    return None;
  }
  let cache_read = ptr_u64(u, "/prompt_tokens_details/cached_tokens");
  let reasoning = ptr_u64(u, "/completion_tokens_details/reasoning_tokens");
  Some(Usage {
    input_tokens,
    output_tokens,
    details: UsageDetails { cache_read, reasoning },
  })
}

/// Parse OpenAI responses / Codex usage block.
fn parse_openai_responses_usage(u: Option<&Value>) -> Option<Usage> {
  let input_tokens = ptr_u64(u, "/input_tokens");
  let output_tokens = ptr_u64(u, "/output_tokens");
  if input_tokens.is_none() && output_tokens.is_none() {
    return None;
  }
  let cache_read = ptr_u64(u, "/input_tokens_details/cached_tokens");
  let reasoning = ptr_u64(u, "/output_tokens_details/reasoning_tokens");
  Some(Usage {
    input_tokens,
    output_tokens,
    details: UsageDetails { cache_read, reasoning },
  })
}

/// Parse Anthropic-style usage block. Recognized by `input_tokens` AND
/// presence of any `cache_*_input_tokens` field. Normalizes
/// `Usage.input_tokens` to the total (input + cache_creation + cache_read)
/// since Anthropic's `input_tokens` excludes cached content.
fn parse_anthropic_usage(u: Option<&Value>) -> Option<Usage> {
  let raw_input = ptr_u64(u, "/input_tokens");
  let cache_creation = ptr_u64(u, "/cache_creation_input_tokens");
  let cache_read = ptr_u64(u, "/cache_read_input_tokens");
  // Require at least one Anthropic-specific marker.
  if cache_creation.is_none() && cache_read.is_none() {
    return None;
  }
  let output_tokens = ptr_u64(u, "/output_tokens");
  let total_input = match (raw_input, cache_creation, cache_read) {
    (None, None, None) => None,
    (a, b, c) => Some(a.unwrap_or(0) + b.unwrap_or(0) + c.unwrap_or(0)),
  };
  Some(Usage {
    input_tokens: total_input,
    output_tokens,
    details: UsageDetails {
      cache_read,
      reasoning: None,
    },
  })
}

pub(super) fn parse_usage_any_json(bytes: &[u8]) -> Usage {
  let v: Value = match serde_json::from_slice(bytes) {
    Ok(v) => v,
    Err(_) => return Usage::default(),
  };
  parse_usage_any_value(&v)
}
