//! Default BuildHeaders stage.
//!
//! Composes the outbound HeaderMap from the inbound request using the
//! [`tokn_headers`] schema + overlay registry. The flow is:
//!
//! 1. Resolve an effective [`tokn_core::AgentId`] — `extracted.agent_id`
//!    wins if set, else the stage's per-provider default mapping is used, else
//!    a stage-wide fallback.
//! 2. Build [`TemplateVars`] from the inbound `HeaderMap` (the same scan
//!    behavior as the legacy router's `api::first_header`).
//! 3. Ask the [`registry::lookup`] for the schema pair:
//!    - `Some(schema)` → build the agent headers and, if
//!      `schema.overlay` is `Some`, build the overlay's typed struct via
//!      `OverlayKind`-specific dispatch and `.dump()` it; compose with
//!      [`ResolvedSchema::compose`].
//!    - `None` (unknown provider) → fall back to an agent-only map; no
//!      overlay.
//!
//! Output: [`BuiltHeaders { headers, vars }`]. `vars` is retained so later
//! stages can splice correlation values into bodies without re-parsing the
//! inbound map.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{BuildHeadersStage, BuiltHeaders, Extracted, Resolved};
use async_trait::async_trait;
use smol_str::SmolStr;
use std::collections::HashMap;
use tokn_core::AgentId;
use tokn_headers::agent::build_agent_headers;
use tokn_headers::registry::{lookup, OverlayKind, ResolvedSchema};
use tokn_headers::schemas::{CodexOverlay, CopilotOverlay};
use tokn_headers::{HeaderMap, TemplateVars};

/// Inbound header names (lowercase) scanned, in order, to populate
/// [`TemplateVars::session_id`]. Mirrors `tokn_router::api::SESSION_ID_HEADERS`.
const SESSION_ID_HEADERS: &[&str] = &[
  "x-session-id",
  "x-client-session-id",
  "session_id",
  "x-session-affinity",
  "x-opencode-session",
];

/// Inbound header names (lowercase) scanned, in order, for
/// [`TemplateVars::request_id`].
const REQUEST_ID_HEADERS: &[&str] = &["x-request-id", "x-interaction-id", "x-opencode-request"];

/// Inbound header names (lowercase) scanned, in order, for
/// [`TemplateVars::project_cwd`].
const PROJECT_ID_HEADERS: &[&str] = &["x-opencode-project", "x-project-cwd"];

/// Inbound header names (lowercase) scanned for
/// [`TemplateVars::interaction_id`].
const INTERACTION_ID_HEADERS: &[&str] = &["x-interaction-id"];

/// Inbound header names (lowercase) scanned for
/// [`TemplateVars::account_id`].
const ACCOUNT_ID_HEADERS: &[&str] = &["chatgpt-account-id"];

/// Default BuildHeaders stage. See module docs for the resolution
/// algorithm.
pub struct DefaultBuildHeaders {
  /// Per-provider fallback agent id. Indexed by `provider_id`.
  agent_defaults: HashMap<SmolStr, AgentId>,
  /// Stage-wide fallback agent id used when no explicit or provider default
  /// exists.
  unknown_agent_id_default: AgentId,
}

impl DefaultBuildHeaders {
  pub fn new(agent_defaults: HashMap<SmolStr, AgentId>, unknown_agent_id_default: AgentId) -> Self {
    Self {
      agent_defaults,
      unknown_agent_id_default,
    }
  }

  /// Convenience constructor with built-in provider defaults and an Opencode
  /// fallback for unknown providers.
  pub fn with_provider_defaults() -> Self {
    let mut agent_defaults = HashMap::new();
    for provider_id in [
      "openai",
      "deepseek",
      "zai",
      "zai-coding-plan",
      "zhipuai",
      "zhipuai-coding-plan",
    ] {
      agent_defaults.insert(SmolStr::new(provider_id), AgentId::Opencode);
    }
    agent_defaults.insert(SmolStr::new("codex"), AgentId::CodexCli);
    agent_defaults.insert(SmolStr::new("copilot"), AgentId::CopilotCli);
    agent_defaults.insert(SmolStr::new("github-copilot"), AgentId::CopilotCli);
    Self::new(agent_defaults, AgentId::Opencode)
  }

  fn effective_agent_id(&self, extracted: &Extracted, provider_id: &str) -> AgentId {
    extracted
      .agent_id
      .clone()
      .or_else(|| self.agent_defaults.get(provider_id).cloned())
      .unwrap_or_else(|| self.unknown_agent_id_default.clone())
  }
}

#[async_trait]
impl BuildHeadersStage for DefaultBuildHeaders {
  async fn build_headers(
    &self,
    _ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
  ) -> Result<BuiltHeaders, PipelineError> {
    let inbound = &extracted.headers;
    let vars = build_template_vars(inbound);
    let agent_id = self.effective_agent_id(extracted, resolved.provider_id.as_str());

    let headers = match lookup(resolved.provider_id.as_str(), agent_id.as_str()) {
      Some(schema) => compose_with_schema(&schema, &vars, inbound),
      None => build_agent_headers(agent_id.as_str(), &vars, inbound),
    };

    Ok(BuiltHeaders { headers, vars })
  }
}

/// Read the first non-empty value from `headers` matching any name in `names`
/// (case-insensitively, since `HeaderMap::get` is case-insensitive).
fn first_header(headers: &HeaderMap, names: &[&str]) -> Option<SmolStr> {
  for name in names {
    if let Some(v) = headers.get(*name) {
      let s = v.as_str();
      if !s.is_empty() {
        return Some(SmolStr::new(s));
      }
    }
  }
  None
}

/// Build [`TemplateVars`] from inbound headers. Mirrors the legacy
/// `pipeline/parse.rs` scan order.
fn build_template_vars(inbound: &HeaderMap) -> TemplateVars {
  TemplateVars {
    session_id: first_header(inbound, SESSION_ID_HEADERS),
    request_id: first_header(inbound, REQUEST_ID_HEADERS),
    project_cwd: first_header(inbound, PROJECT_ID_HEADERS),
    interaction_id: first_header(inbound, INTERACTION_ID_HEADERS),
    account_id: first_header(inbound, ACCOUNT_ID_HEADERS),
  }
}

/// Build the agent half and, if the schema names an overlay, build the
/// overlay's typed struct and `.dump()` it. Then [`ResolvedSchema::compose`]
/// merges with overlay-wins semantics.
fn compose_with_schema(schema: &ResolvedSchema, vars: &TemplateVars, inbound: &HeaderMap) -> HeaderMap {
  let agent_map = schema.agent.build_outbound(vars, inbound);
  let overlay_map = schema.overlay.map(|kind| match kind {
    OverlayKind::Copilot => {
      use tokn_headers::HeaderSchema as _;
      CopilotOverlay::build(vars, inbound).dump()
    }
    OverlayKind::Codex => {
      use tokn_headers::HeaderSchema as _;
      CodexOverlay::build(vars, inbound).dump()
    }
  });
  ResolvedSchema::compose(agent_map, overlay_map)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use bytes::Bytes;
  use serde_json::json;
  use std::sync::Arc;
  use tokn_core::provider::Endpoint;
  use tokn_headers::{keys, HeaderValue};

  fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (k, v) in pairs {
      m.insert(*k, HeaderValue::from_string((*v).to_string()));
    }
    m
  }

  fn extracted(headers: HeaderMap, agent_id: Option<AgentId>) -> Extracted {
    Extracted {
      agent_id,
      model: "gpt-4o".into(),
      stream: false,
      session_id: None,
      project_id: None,
      initiator: "user".into(),
      header_initiator: None,
      route_mode_hint: None,
      headers,
      raw_body: Bytes::new(),
      decoded_body: Bytes::new(),
      body_json: Arc::new(json!({})),
      content_encoding: None,
    }
  }

  fn resolved(provider_id: &str) -> Resolved {
    Resolved {
      agent_id: None,
      model: "gpt-4o".into(),
      upstream_model: "gpt-4o".into(),
      upstream_endpoint: Endpoint::ChatCompletions,
      account_id: "acct-1".into(),
      provider_id: provider_id.into(),
      account_handle: crate::test_support::mock_handle("acct-1", provider_id),
    }
  }

  fn ctx() -> PipelineCtx {
    PipelineCtx::new("req-bh", Endpoint::ChatCompletions, Arc::new(EventBus::new(64)))
  }

  #[tokio::test]
  async fn provider_default_with_overlay_composes_both() {
    let stage = DefaultBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("copilot"))
      .await
      .unwrap();
    assert!(out.headers.contains_key(&keys::EDITOR_VERSION));
    assert!(out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID));
  }

  #[tokio::test]
  async fn provider_default_without_overlay_uses_agent_id_only() {
    let stage = DefaultBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("deepseek"))
      .await
      .unwrap();
    assert!(!out.headers.is_empty(), "agent header map should be non-empty");
    assert!(!out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID));
  }

  #[tokio::test]
  async fn missing_agent_id_falls_back_to_custom_provider_default() {
    let mut defaults = HashMap::new();
    defaults.insert(SmolStr::new("copilot"), AgentId::CopilotCli);
    let stage = DefaultBuildHeaders::new(defaults, AgentId::Opencode);
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("copilot"))
      .await
      .unwrap();
    assert!(out.headers.contains_key(&keys::EDITOR_VERSION));
  }

  #[tokio::test]
  async fn missing_agent_id_falls_back_to_global_default() {
    let stage = DefaultBuildHeaders::new(HashMap::new(), AgentId::Opencode);
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("nonesuch"))
      .await
      .unwrap();
    assert!(!out.headers.is_empty());
  }

  #[tokio::test]
  async fn explicit_agent_id_overrides_provider_default() {
    let stage = DefaultBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(
        &ctx(),
        &extracted(HeaderMap::new(), Some(AgentId::CodexCli)),
        &resolved("openai"),
      )
      .await
      .unwrap();
    assert!(
      out.headers.contains_key(&keys::ORIGINATOR),
      "Codex overlay's `originator` header missing — explicit agent_id was ignored"
    );
  }

  #[tokio::test]
  async fn template_vars_populated_from_inbound() {
    let headers = header_map(&[
      ("x-session-id", "ses_abc"),
      ("x-request-id", "req_xyz"),
      ("x-opencode-project", "/home/me/proj"),
      ("chatgpt-account-id", "acct_42"),
    ]);
    let stage = DefaultBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(&ctx(), &extracted(headers, None), &resolved("deepseek"))
      .await
      .unwrap();
    assert_eq!(out.vars.session_id.as_deref(), Some("ses_abc"));
    assert_eq!(out.vars.request_id.as_deref(), Some("req_xyz"));
    assert_eq!(out.vars.project_cwd.as_deref(), Some("/home/me/proj"));
    assert_eq!(out.vars.account_id.as_deref(), Some("acct_42"));
  }
}
