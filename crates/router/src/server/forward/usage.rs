use llm_core::db::{Usage, UsageDetails};
use serde_json::Value;

/// Extract `Usage` from an upstream response body. Handles three shapes:
///
/// - **OpenAI** (chat completions): `usage.{prompt_tokens, completion_tokens,
///   prompt_tokens_details.cached_tokens, completion_tokens_details.reasoning_tokens}`
/// - **Anthropic** (messages): `message.usage.{input_tokens, output_tokens,
///   cache_creation_input_tokens, cache_read_input_tokens}`. The Anthropic
///   `input_tokens` field excludes cached portions, so we normalize
///   `Usage.input_tokens` to the total (input + cache_creation + cache_read).
/// - **OpenAI Responses API**: `response.usage.{input_tokens, output_tokens, ...}`
///
/// Returns an empty `Usage` (all `None`) when no recognizable shape is found.
pub(crate) fn parse_usage_any_value(v: &Value) -> Usage {
  // Try OpenAI shape: top-level "usage"
  if let Some(u) = v.get("usage") {
    if let Some(usage) = parse_openai_usage(u) {
      return usage;
    }
    // Could be Anthropic-style at top-level "usage" too
    if let Some(usage) = parse_anthropic_usage(u) {
      return usage;
    }
  }
  // Anthropic streaming shape: message.usage
  if let Some(u) = v.get("message").and_then(|m| m.get("usage")) {
    if let Some(usage) = parse_anthropic_usage(u) {
      return usage;
    }
  }
  // OpenAI Responses API: response.usage
  if let Some(u) = v.get("response").and_then(|r| r.get("usage")) {
    if let Some(usage) = parse_openai_usage(u) {
      return usage;
    }
  }
  Usage::default()
}

/// Parse OpenAI-style usage block. Recognized by `prompt_tokens` or
/// `input_tokens` (Responses API uses input_tokens at this level).
fn parse_openai_usage(u: &Value) -> Option<Usage> {
  let input_tokens = u
    .get("prompt_tokens")
    .or_else(|| u.get("input_tokens"))
    .and_then(|x| x.as_u64());
  let output_tokens = u
    .get("completion_tokens")
    .or_else(|| u.get("output_tokens"))
    .and_then(|x| x.as_u64());
  if input_tokens.is_none() && output_tokens.is_none() {
    return None;
  }
  // Reject Anthropic-shaped blocks (no prompt_tokens AND has cache_creation_input_tokens)
  if u.get("prompt_tokens").is_none() && u.get("cache_creation_input_tokens").is_some() {
    return None;
  }
  let cache_read = u
    .get("prompt_tokens_details")
    .and_then(|d| d.get("cached_tokens"))
    .and_then(|x| x.as_u64());
  let reasoning = u
    .get("completion_tokens_details")
    .and_then(|d| d.get("reasoning_tokens"))
    .and_then(|x| x.as_u64());
  Some(Usage {
    input_tokens,
    output_tokens,
    details: UsageDetails {
      cache_read,
      reasoning,
    },
  })
}

/// Parse Anthropic-style usage block. Recognized by `input_tokens` AND
/// presence of any `cache_*_input_tokens` field. Normalizes
/// `Usage.input_tokens` to the total (input + cache_creation + cache_read)
/// since Anthropic's `input_tokens` excludes cached content.
fn parse_anthropic_usage(u: &Value) -> Option<Usage> {
  let raw_input = u.get("input_tokens").and_then(|x| x.as_u64());
  let cache_creation = u
    .get("cache_creation_input_tokens")
    .and_then(|x| x.as_u64());
  let cache_read = u.get("cache_read_input_tokens").and_then(|x| x.as_u64());
  // Require at least one Anthropic-specific marker.
  if cache_creation.is_none() && cache_read.is_none() {
    return None;
  }
  let output_tokens = u.get("output_tokens").and_then(|x| x.as_u64());
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
