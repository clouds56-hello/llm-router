//! Client-id-driven BuildHeaders stage.
//!
//! Composes the outbound HeaderMap from the inbound request using the
//! [`tokn_headers`] schema + overlay registry. The flow is:
//!
//! 1. Resolve an effective [`tokn_core::ClientId`] — `extracted.client_id`
//!    wins if set, else the stage's per-provider default mapping is used, else
//!    a stage-wide fallback.
//! 2. Build [`TemplateVars`] from the inbound `HeaderMap` (the same scan
//!    behavior as the legacy router's `api::first_header`).
//! 3. Ask the [`registry::lookup`] for the schema pair:
//!    - `Some(schema)` → build the client-id headers and, if
//!      `schema.overlay` is `Some`, build the overlay's typed struct via
//!      `OverlayKind`-specific dispatch and `.dump()` it; compose with
//!      [`ResolvedSchema::compose`].
//!    - `None` (unknown provider) → fall back to a client-id-only map; no
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
use tokn_core::ClientId;
use tokn_headers::persona::Persona;
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

/// Client-id-driven BuildHeaders stage. See module docs for the resolution
/// algorithm.
pub struct ClientIdBuildHeaders {
  /// Per-provider fallback client id. Indexed by `provider_id`.
  client_defaults: HashMap<SmolStr, ClientId>,
  /// Stage-wide fallback client id used when no explicit or provider default
  /// exists.
  unknown_client_id_default: ClientId,
}

impl ClientIdBuildHeaders {
  pub fn new(client_defaults: HashMap<SmolStr, ClientId>, unknown_client_id_default: ClientId) -> Self {
    Self {
      client_defaults,
      unknown_client_id_default,
    }
  }

  /// Convenience constructor with built-in provider defaults and an Opencode
  /// fallback for unknown providers.
  pub fn with_provider_defaults() -> Self {
    let mut client_defaults = HashMap::new();
    for provider_id in [
      "openai",
      "deepseek",
      "zai",
      "zai-coding-plan",
      "zhipuai",
      "zhipuai-coding-plan",
    ] {
      client_defaults.insert(SmolStr::new(provider_id), ClientId::Opencode);
    }
    client_defaults.insert(SmolStr::new("codex"), ClientId::CodexCli);
    client_defaults.insert(SmolStr::new("copilot"), ClientId::CopilotCli);
    client_defaults.insert(SmolStr::new("github-copilot"), ClientId::CopilotCli);
    Self::new(client_defaults, ClientId::Opencode)
  }

  fn effective_client_id(&self, extracted: &Extracted, provider_id: &str) -> ClientId {
    extracted
      .client_id
      .clone()
      .or_else(|| self.client_defaults.get(provider_id).cloned())
      .unwrap_or_else(|| self.unknown_client_id_default.clone())
  }
}

#[async_trait]
impl BuildHeadersStage for ClientIdBuildHeaders {
  async fn build_headers(
    &self,
    _ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
  ) -> Result<BuiltHeaders, PipelineError> {
    let inbound = &extracted.headers;
    let vars = build_template_vars(inbound);
    let client_id = self.effective_client_id(extracted, resolved.provider_id.as_str());

    let persona = persona_from_client_id(&client_id);
    let headers = match lookup(resolved.provider_id.as_str(), &persona) {
      Some(schema) => compose_with_schema(&schema, &client_id, &vars, inbound),
      None => build_outbound(&client_id, &vars, inbound),
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

/// Build the client-id half and, if the schema names an overlay, build the
/// overlay's typed struct and `.dump()` it. Then [`ResolvedSchema::compose`]
/// merges with overlay-wins semantics.
fn compose_with_schema(
  schema: &ResolvedSchema,
  client_id: &ClientId,
  vars: &TemplateVars,
  inbound: &HeaderMap,
) -> HeaderMap {
  let client_id_map = build_outbound(client_id, vars, inbound);
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
  ResolvedSchema::compose(client_id_map, overlay_map)
}

fn persona_from_client_id(client_id: &ClientId) -> Persona {
  match client_id {
    ClientId::Opencode => Persona::Opencode,
    ClientId::CodexCli => Persona::CodexCli,
    ClientId::ClaudeCode => Persona::ClaudeCode,
    ClientId::Cline => Persona::Cline,
    ClientId::CopilotCli => Persona::CopilotCli,
    ClientId::Other(value) => Persona::Custom(value.clone()),
  }
}

fn build_outbound(client_id: &ClientId, vars: &TemplateVars, inbound: &HeaderMap) -> HeaderMap {
  persona_from_client_id(client_id).build_outbound(vars, inbound)
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

  fn extracted(headers: HeaderMap, client_id: Option<ClientId>) -> Extracted {
    Extracted {
      client_id,
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
      client_id: None,
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
    let stage = ClientIdBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("copilot"))
      .await
      .unwrap();
    assert!(out.headers.contains_key(&keys::EDITOR_VERSION));
    assert!(out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID));
  }

  #[tokio::test]
  async fn provider_default_without_overlay_uses_client_id_only() {
    let stage = ClientIdBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("deepseek"))
      .await
      .unwrap();
    assert!(!out.headers.is_empty(), "client_id map should be non-empty");
    assert!(!out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID));
  }

  #[tokio::test]
  async fn missing_client_id_falls_back_to_custom_provider_default() {
    let mut defaults = HashMap::new();
    defaults.insert(SmolStr::new("copilot"), ClientId::CopilotCli);
    let stage = ClientIdBuildHeaders::new(defaults, ClientId::Opencode);
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("copilot"))
      .await
      .unwrap();
    assert!(out.headers.contains_key(&keys::EDITOR_VERSION));
  }

  #[tokio::test]
  async fn missing_client_id_falls_back_to_global_default() {
    let stage = ClientIdBuildHeaders::new(HashMap::new(), ClientId::Opencode);
    let out = stage
      .build_headers(&ctx(), &extracted(HeaderMap::new(), None), &resolved("nonesuch"))
      .await
      .unwrap();
    assert!(!out.headers.is_empty());
  }

  #[tokio::test]
  async fn explicit_client_id_overrides_provider_default() {
    let stage = ClientIdBuildHeaders::with_provider_defaults();
    let out = stage
      .build_headers(
        &ctx(),
        &extracted(HeaderMap::new(), Some(ClientId::CodexCli)),
        &resolved("openai"),
      )
      .await
      .unwrap();
    assert!(
      out.headers.contains_key(&keys::ORIGINATOR),
      "Codex overlay's `originator` header missing — explicit client_id was ignored"
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
    let stage = ClientIdBuildHeaders::with_provider_defaults();
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
