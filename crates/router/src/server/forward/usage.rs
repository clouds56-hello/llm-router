use serde_json::Value;

pub(crate) fn parse_usage_any_value(v: &Value) -> (Option<u64>, Option<u64>) {
  if let Some(u) = v.get("usage") {
    let pt = u
      .get("prompt_tokens")
      .or_else(|| u.get("input_tokens"))
      .and_then(|x| x.as_u64());
    let ct = u
      .get("completion_tokens")
      .or_else(|| u.get("output_tokens"))
      .and_then(|x| x.as_u64());
    if pt.is_some() || ct.is_some() {
      return (pt, ct);
    }
  }
  if let Some(m) = v.get("message").and_then(|m| m.get("usage")) {
    let pt = m.get("input_tokens").and_then(|x| x.as_u64());
    let ct = m.get("output_tokens").and_then(|x| x.as_u64());
    return (pt, ct);
  }
  if let Some(u) = v.get("response").and_then(|r| r.get("usage")) {
    let pt = u
      .get("input_tokens")
      .or_else(|| u.get("prompt_tokens"))
      .and_then(|x| x.as_u64());
    let ct = u
      .get("output_tokens")
      .or_else(|| u.get("completion_tokens"))
      .and_then(|x| x.as_u64());
    return (pt, ct);
  }
  (None, None)
}

pub(super) fn parse_usage_any_json(bytes: &[u8]) -> (Option<u64>, Option<u64>) {
  let v: Value = match serde_json::from_slice(bytes) {
    Ok(v) => v,
    Err(_) => return (None, None),
  };
  parse_usage_any_value(&v)
}
