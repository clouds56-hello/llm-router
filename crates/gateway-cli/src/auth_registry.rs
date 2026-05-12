//! Provider-id → [`ProviderAuth`] dispatch table.
//!
//! This lives in `gateway-cli` rather than `llm-auth` because it is the
//! only crate that legitimately depends on every provider implementation
//! at once. Putting it here keeps `llm-auth` provider-agnostic and avoids
//! a dependency cycle (providers → llm-auth → providers).
//!
//! Functions are `#[allow(dead_code)]` until Phase 5 wires the CLI
//! account subcommands through them.
#![allow(dead_code)]

use llm_auth::ProviderAuth;

/// Resolve the [`ProviderAuth`] impl for a provider id, or `None` if no
/// known provider matches.
///
/// Lookup is exact-match. The four Z.ai aliases each get their own
/// static impl so the returned [`ProviderAuth::id`] always matches the
/// stored account verbatim.
pub fn provider_auth_for(id: &str) -> Option<&'static dyn ProviderAuth> {
  use llm_provider_copilot::auth as cop;
  use llm_provider_zai::auth as zai;
  match id {
    "github-copilot" => Some(cop::provider_auth()),
    "zai-coding-plan" => Some(zai::zai_coding_plan_auth()),
    "zai" => Some(zai::zai_auth()),
    "zhipuai-coding-plan" => Some(zai::zhipuai_coding_plan_auth()),
    "zhipuai" => Some(zai::zhipuai_auth()),
    _ => None,
  }
}

/// All provider ids known to the registry. Useful for CLI pickers.
pub fn known_providers() -> &'static [&'static str] {
  &[
    "github-copilot",
    "zai-coding-plan",
    "zai",
    "zhipuai-coding-plan",
    "zhipuai",
  ]
}
