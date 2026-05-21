//! Persona-driven BuildHeaders stage.
//!
//! Composes the outbound HeaderMap from the inbound request using the
//! [`tokn_headers`] persona + overlay registry. The flow is:
//!
//! 1. Derive an effective [`Persona`] for the request — `extracted.client_id`
//!    wins if set, else [`tokn_headers::detect_persona`] inspects the inbound
//!    `User-Agent`.
//! 2. If the effective persona is [`Persona::Custom`], resolve it to a
//!    concrete persona via two-level fallback:
//!    - per-provider default (`client_defaults[provider_id]`), then
//!    - the stage-wide `unknown_persona_default`.
//! 3. Build [`TemplateVars`] from the inbound `HeaderMap` (the same scan
//!    behavior as the legacy router's `api::first_header`).
//! 4. Ask the [`registry::lookup`] for the schema pair:
//!    - `Some(schema)` → call `persona.build_outbound(vars, inbound)` and, if
//!      `schema.overlay` is `Some`, build the overlay's typed struct via
//!      `OverlayKind`-specific dispatch and `.dump()` it; compose with
//!      [`ResolvedSchema::compose`].
//!    - `None` (unknown provider) → fall back to a persona-only map; no
//!      overlay.
//!
//! Output: [`BuiltHeaders { headers, vars }`]. `vars` is retained so later
//! stages can splice correlation values into bodies without re-parsing the
//! inbound map.

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{BuildHeadersStage, BuiltHeaders, Extracted, Resolved};
use async_trait::async_trait;
use tokn_headers::registry::{lookup, OverlayKind, ResolvedSchema};
use tokn_headers::schemas::{CodexOverlay, CopilotOverlay};
use tokn_headers::{HeaderMap, Persona, TemplateVars};
use smol_str::SmolStr;
use std::collections::HashMap;

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

/// Persona-driven BuildHeaders stage. See module docs for the resolution
/// algorithm.
pub struct PersonaBuildHeaders {
  /// Per-provider fallback persona used when the inbound persona is
  /// [`Persona::Custom`]. Indexed by `provider_id` (e.g. `"openai"`,
  /// `"copilot"`). Missing entries fall through to
  /// [`PersonaBuildHeaders::unknown_persona_default`].
  client_defaults: HashMap<SmolStr, Persona>,
  /// Stage-wide fallback persona used when the inbound persona is
  /// [`Persona::Custom`] AND no per-provider default matches.
  unknown_persona_default: Persona,
}

impl PersonaBuildHeaders {
  pub fn new(client_defaults: HashMap<SmolStr, Persona>, unknown_persona_default: Persona) -> Self {
    Self {
      client_defaults,
      unknown_persona_default,
    }
  }

  /// Convenience constructor with `unknown_persona_default = Persona::Opencode`
  /// and no per-provider defaults.
  pub fn with_opencode_default() -> Self {
    Self::new(HashMap::new(), Persona::Opencode)
  }

  /// Pick the effective [`Persona`] for this request:
  /// 1. If `extracted.client_id` is set, parse it as a persona (never fails;
  ///    falls back to `Custom`).
  /// 2. Otherwise, run [`tokn_headers::detect_persona`] over the inbound
  ///    headers.
  fn effective_persona(&self, extracted: &Extracted) -> Persona {
    if let Some(cid) = &extracted.client_id {
      return Persona::from_str_lossy(cid.as_str());
    }
    tokn_headers::detect_persona(&extracted.headers)
  }

  /// Resolve a `Custom` persona to a concrete one via two-level fallback.
  /// Known personas (anything other than `Custom`) are returned unchanged.
  fn resolve_custom(&self, persona: Persona, provider_id: &str) -> Persona {
    if !matches!(persona, Persona::Custom(_)) {
      return persona;
    }
    if let Some(p) = self.client_defaults.get(provider_id) {
      return p.clone();
    }
    self.unknown_persona_default.clone()
  }
}

#[async_trait]
impl BuildHeadersStage for PersonaBuildHeaders {
  async fn build_headers(
    &self,
    _ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
  ) -> Result<BuiltHeaders, PipelineError> {
    let inbound = &extracted.headers;
    let vars = build_template_vars(inbound);

    let raw_persona = self.effective_persona(extracted);
    let persona = self.resolve_custom(raw_persona, resolved.provider_id.as_str());

    let headers = match lookup(resolved.provider_id.as_str(), &persona) {
      Some(schema) => compose_with_schema(&schema, &persona, &vars, inbound),
      None => persona.build_outbound(&vars, inbound),
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

/// Build the persona half via [`Persona::build_outbound`] and, if the schema
/// names an overlay, build the overlay's typed struct and `.dump()` it.
/// Then [`ResolvedSchema::compose`] merges with overlay-wins semantics.
fn compose_with_schema(
  schema: &ResolvedSchema,
  persona: &Persona,
  vars: &TemplateVars,
  inbound: &HeaderMap,
) -> HeaderMap {
  let persona_map = persona.build_outbound(vars, inbound);
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
  ResolvedSchema::compose(persona_map, overlay_map)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use crate::pipeline::ctx::PipelineCtx;
  use crate::pipeline::stages::Extracted;
  use bytes::Bytes;
  use tokn_core::provider::Endpoint;
  use tokn_headers::{keys, HeaderValue};
  use serde_json::json;
  use std::sync::Arc;

  fn header_map(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut m = HeaderMap::new();
    for (k, v) in pairs {
      m.insert(*k, HeaderValue::from_string((*v).to_string()));
    }
    m
  }

  fn extracted(headers: HeaderMap, client_id: Option<&str>) -> Extracted {
    Extracted {
      client_id: client_id.map(|s| tokn_core::ClientId::from(SmolStr::new(s))),
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
      body_json: std::sync::Arc::new(json!({})),
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
  async fn known_persona_with_overlay_composes_both() {
    // Copilot persona → Copilot provider → CopilotCli + Copilot overlay.
    let headers = header_map(&[("user-agent", "copilot/1.0.25")]);
    let stage = PersonaBuildHeaders::with_opencode_default();
    let out = stage
      .build_headers(&ctx(), &extracted(headers, None), &resolved("copilot"))
      .await
      .unwrap();
    // Overlay-managed gateway headers must be present.
    assert!(
      out.headers.contains_key(&keys::EDITOR_VERSION),
      "Editor-Version (overlay) missing"
    );
    assert!(
      out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID),
      "Copilot-Integration-Id (overlay) missing"
    );
  }

  #[tokio::test]
  async fn known_persona_without_overlay_uses_persona_only() {
    // deepseek provider has no overlay; persona half must still be present.
    let headers = header_map(&[("user-agent", "opencode/1.14.28")]);
    let stage = PersonaBuildHeaders::with_opencode_default();
    let out = stage
      .build_headers(&ctx(), &extracted(headers, None), &resolved("deepseek"))
      .await
      .unwrap();
    assert!(!out.headers.is_empty(), "persona map should be non-empty");
    // No Copilot-managed header should be present (no overlay).
    assert!(!out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID));
  }

  #[tokio::test]
  async fn custom_persona_falls_back_to_per_provider_default() {
    let mut defaults = HashMap::new();
    defaults.insert(SmolStr::new("copilot"), Persona::CopilotCli);
    let stage = PersonaBuildHeaders::new(defaults, Persona::Opencode);
    // User-Agent is an unknown slug; client_id is unset → persona = Custom("weird-tool")
    let headers = header_map(&[("user-agent", "weird-tool/0.1")]);
    let out = stage
      .build_headers(&ctx(), &extracted(headers, None), &resolved("copilot"))
      .await
      .unwrap();
    // Per-provider default = CopilotCli, so Copilot overlay still applies AND
    // the persona half should be CopilotCli's (not Opencode's).
    assert!(out.headers.contains_key(&keys::EDITOR_VERSION));
  }

  #[tokio::test]
  async fn custom_persona_falls_back_to_global_default_when_no_per_provider_entry() {
    let stage = PersonaBuildHeaders::new(HashMap::new(), Persona::Opencode);
    let headers = header_map(&[("user-agent", "weird-tool/0.1")]);
    let out = stage
      .build_headers(&ctx(), &extracted(headers, None), &resolved("deepseek"))
      .await
      .unwrap();
    // Opencode persona has a real schema → non-empty map.
    assert!(!out.headers.is_empty());
  }

  #[tokio::test]
  async fn unknown_provider_returns_persona_only_map() {
    let stage = PersonaBuildHeaders::with_opencode_default();
    let headers = header_map(&[("user-agent", "opencode/1.14.28")]);
    let out = stage
      .build_headers(&ctx(), &extracted(headers, None), &resolved("nonesuch"))
      .await
      .unwrap();
    // Persona half still builds; overlay does not.
    assert!(!out.headers.is_empty());
    assert!(!out.headers.contains_key(&keys::COPILOT_INTEGRATION_ID));
  }

  #[tokio::test]
  async fn client_id_overrides_user_agent_detection() {
    // UA says copilot, but client_id explicitly says codex-cli.
    let headers = header_map(&[("user-agent", "copilot/1.0.25")]);
    let stage = PersonaBuildHeaders::with_opencode_default();
    let out = stage
      .build_headers(&ctx(), &extracted(headers, Some("codex-cli")), &resolved("openai"))
      .await
      .unwrap();
    // openai + CodexCli → Codex overlay. Verify a Codex-overlay-managed
    // header is present (originator).
    assert!(
      out.headers.contains_key(&keys::ORIGINATOR),
      "Codex overlay's `originator` header missing — client_id override failed"
    );
  }

  #[tokio::test]
  async fn template_vars_populated_from_inbound() {
    let headers = header_map(&[
      ("user-agent", "opencode/1.14.28"),
      ("x-session-id", "ses_abc"),
      ("x-request-id", "req_xyz"),
      ("x-opencode-project", "/home/me/proj"),
      ("chatgpt-account-id", "acct_42"),
    ]);
    let stage = PersonaBuildHeaders::with_opencode_default();
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
