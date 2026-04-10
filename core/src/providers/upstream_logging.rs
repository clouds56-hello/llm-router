use std::time::Instant;

use serde_json::Value;

use super::UpstreamLogContext;

const SNIPPET_MAX_CHARS: usize = 240;
const SUMMARY_MAX_CHARS: usize = 240;

pub(crate) fn log_started(ctx: &UpstreamLogContext, body: &Value) -> Instant {
  let started = Instant::now();
  tracing::info!(
    target: "upstream",
    provider = %ctx.provider,
    adapter = %ctx.adapter,
    upstream_path = %ctx.upstream_path,
    method = ctx.method,
    model = %ctx.model.clone().unwrap_or_default(),
    stream = ctx.stream,
    request_summary = %summarize_request_body(body),
    "upstream request started"
  );
  started
}

pub(crate) fn log_completed(ctx: &UpstreamLogContext, started: Instant, status: u16) {
  tracing::info!(
    target: "upstream",
    provider = %ctx.provider,
    adapter = %ctx.adapter,
    upstream_path = %ctx.upstream_path,
    method = ctx.method,
    model = %ctx.model.clone().unwrap_or_default(),
    stream = ctx.stream,
    status = status,
    latency_ms = started.elapsed().as_millis() as u64,
    "upstream request completed"
  );
}

pub(crate) fn log_failed(ctx: &UpstreamLogContext, started: Instant, status: Option<u16>, snippet: Option<&str>) {
  tracing::warn!(
    target: "upstream",
    provider = %ctx.provider,
    adapter = %ctx.adapter,
    upstream_path = %ctx.upstream_path,
    method = ctx.method,
    model = %ctx.model.clone().unwrap_or_default(),
    stream = ctx.stream,
    status = status.unwrap_or_default(),
    latency_ms = started.elapsed().as_millis() as u64,
    upstream_error_snippet = %sanitize_snippet(snippet.unwrap_or_default()),
    "upstream request failed"
  );
}

fn summarize_request_body(body: &Value) -> String {
  let model = body.get("model").and_then(Value::as_str).unwrap_or_default();
  let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
  let temperature = body.get("temperature").and_then(Value::as_f64);
  let max_tokens = body
    .get("max_tokens")
    .or_else(|| body.get("max_output_tokens"))
    .and_then(Value::as_u64);
  let top_p = body.get("top_p").and_then(Value::as_f64);
  let messages_count = body.get("messages").and_then(Value::as_array).map(|v| v.len());
  let input_chars = body.get("input").map(value_char_len);
  let stop_count = body.get("stop").and_then(|v| v.as_array().map(|a| a.len()));

  let mut parts = Vec::new();
  if !model.is_empty() {
    parts.push(format!("model={model}"));
  }
  parts.push(format!("stream={stream}"));
  if let Some(v) = temperature {
    parts.push(format!("temperature={v}"));
  }
  if let Some(v) = max_tokens {
    parts.push(format!("max_tokens={v}"));
  }
  if let Some(v) = top_p {
    parts.push(format!("top_p={v}"));
  }
  if let Some(v) = stop_count {
    parts.push(format!("stop_count={v}"));
  }
  if let Some(v) = messages_count {
    parts.push(format!("messages_count={v}"));
  }
  if let Some(v) = input_chars {
    parts.push(format!("input_chars={v}"));
  }

  let summary = parts.join(" ");
  if summary.chars().count() > SUMMARY_MAX_CHARS {
    summary.chars().take(SUMMARY_MAX_CHARS).collect()
  } else {
    summary
  }
}

fn value_char_len(value: &Value) -> usize {
  match value {
    Value::String(s) => s.chars().count(),
    Value::Array(arr) => arr.iter().map(value_char_len).sum(),
    Value::Object(map) => map.values().map(value_char_len).sum(),
    _ => value.to_string().chars().count(),
  }
}

fn sanitize_snippet(raw: &str) -> String {
  if raw.is_empty() {
    return String::new();
  }
  let compact = raw
    .replace('\n', " ")
    .replace('\r', " ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ");
  if compact.chars().count() > SNIPPET_MAX_CHARS {
    compact.chars().take(SNIPPET_MAX_CHARS).collect()
  } else {
    compact
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn summary_omits_sensitive_fields() {
    let body = serde_json::json!({
      "model":"gpt-5-mini",
      "stream":true,
      "messages":[{"role":"user","content":"top secret prompt"}],
      "api_key":"sk-live-secret",
      "access_token":"oauth-secret"
    });
    let summary = summarize_request_body(&body);
    assert!(summary.contains("model=gpt-5-mini"));
    assert!(summary.contains("messages_count=1"));
    assert!(!summary.contains("sk-live-secret"));
    assert!(!summary.contains("oauth-secret"));
    assert!(!summary.contains("top secret prompt"));
  }

  #[test]
  fn snippet_is_compacted_and_truncated() {
    let raw = format!("{}\n\n{}", "x".repeat(400), "tail");
    let out = sanitize_snippet(&raw);
    assert!(!out.contains('\n'));
    assert!(out.chars().count() <= SNIPPET_MAX_CHARS);
  }
}
