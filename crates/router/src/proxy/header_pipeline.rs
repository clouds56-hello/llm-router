use llm_config::profiles::{is_router_controlled, normalize_header_name, warn_if_unverified, Profiles, TemplateVars};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

#[derive(Debug, Clone)]
pub struct HeaderPipelineInput<'a> {
  pub profiles: &'a Profiles,
  pub persona: &'a str,
  pub provider_id: &'a str,
  pub inbound: &'a HeaderMap,
  pub provider_patch: Option<&'a HeaderMap>,
  pub vars: &'a TemplateVars,
}

#[derive(Debug, Clone)]
pub struct HeaderPipelineOutput {
  pub headers: HeaderMap,
  pub scopes_used: Vec<String>,
  pub verified: bool,
}

pub fn build_headers(input: HeaderPipelineInput<'_>) -> Option<HeaderPipelineOutput> {
  let resolved = input.profiles.resolve(input.persona, input.provider_id)?;
  warn_if_unverified(input.persona, input.provider_id, &resolved);
  let mut headers = HeaderMap::new();

  for (name, value) in resolved.render_headers(input.vars) {
    insert_header(&mut headers, &name, &value);
  }

  for name in &resolved.forward {
    if resolved.deny.contains(name) || is_router_controlled(name) {
      continue;
    }
    if let Some(value) = input.inbound.get(name.as_str()) {
      if let Ok(header_name) = HeaderName::from_bytes(name.as_bytes()) {
        headers.insert(header_name, value.clone());
      }
    }
  }

  if let Some(patch) = input.provider_patch {
    for (name, value) in patch {
      if !is_router_controlled(name.as_str()) {
        headers.insert(name.clone(), value.clone());
      }
    }
  }

  Some(HeaderPipelineOutput {
    headers,
    scopes_used: resolved.scopes_used,
    verified: resolved.verified,
  })
}

fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
  let name = normalize_header_name(name);
  if is_router_controlled(&name) {
    return;
  }
  let Ok(name) = HeaderName::from_bytes(name.as_bytes()) else {
    return;
  };
  let Ok(value) = HeaderValue::from_str(value) else {
    return;
  };
  headers.insert(name, value);
}

pub fn parse_inbound_vars(inbound: &HeaderMap) -> TemplateVars {
  TemplateVars {
    session_id: header_value(inbound, "x-session-affinity").or_else(|| header_value(inbound, "session_id")),
    request_id: header_value(inbound, "x-request-id"),
    project_cwd: header_value(inbound, "x-project-cwd"),
    interaction_id: header_value(inbound, "x-interaction-id"),
    account_id: header_value(inbound, "chatgpt-account-id"),
  }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
  headers
    .get(name)
    .and_then(|v| v.to_str().ok())
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
  use super::*;
  use reqwest::header::HeaderValue;

  #[test]
  fn persona_headers_then_forwarded_inbound_then_provider_patch() {
    let profiles = Profiles::parse(
      r#"
        [opencode]
        forward = ["x-session-affinity", "x-forwarded"]

        [opencode.default]
        "user-agent" = "opencode"
        "x-session-affinity" = "<session_id>"
        "x-forwarded" = "persona"
      "#,
    )
    .unwrap();
    let mut inbound = HeaderMap::new();
    inbound.insert("x-session-affinity", HeaderValue::from_static("ses_inbound"));
    inbound.insert("x-forwarded", HeaderValue::from_static("inbound"));
    inbound.insert("authorization", HeaderValue::from_static("Bearer no"));
    let mut patch = HeaderMap::new();
    patch.insert("x-forwarded", HeaderValue::from_static("patch"));
    patch.insert("accept", HeaderValue::from_static("application/json"));

    let out = build_headers(HeaderPipelineInput {
      profiles: &profiles,
      persona: "opencode",
      provider_id: "github-copilot",
      inbound: &inbound,
      provider_patch: Some(&patch),
      vars: &parse_inbound_vars(&inbound),
    })
    .unwrap();

    assert_eq!(
      out.headers.get("user-agent").and_then(|v| v.to_str().ok()),
      Some("opencode")
    );
    assert_eq!(
      out.headers.get("x-session-affinity").and_then(|v| v.to_str().ok()),
      Some("ses_inbound")
    );
    assert_eq!(
      out.headers.get("x-forwarded").and_then(|v| v.to_str().ok()),
      Some("patch")
    );
    assert!(out.headers.get("authorization").is_none());
    assert!(out.headers.get("accept").is_none());
  }

  #[test]
  fn unresolved_request_template_drops_header() {
    let profiles = Profiles::parse(
      r#"
        [opencode.default]
        "x-session-affinity" = "<session_id>"
      "#,
    )
    .unwrap();
    let out = build_headers(HeaderPipelineInput {
      profiles: &profiles,
      persona: "opencode",
      provider_id: "github-copilot",
      inbound: &HeaderMap::new(),
      provider_patch: None,
      vars: &parse_inbound_vars(&HeaderMap::new()),
    })
    .unwrap();
    assert!(out.headers.get("x-session-affinity").is_none());
  }
}
