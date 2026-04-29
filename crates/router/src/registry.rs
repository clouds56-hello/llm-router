use llm_core::config::{Account, CopilotHeaders};
use llm_core::provider::{error, Provider, Result, ID_GITHUB_COPILOT, ZAI_ALIASES};
use std::sync::Arc;

pub fn build_for_account(a: &Account, global_headers: &CopilotHeaders) -> Result<Arc<dyn Provider>> {
  match a.provider.as_str() {
    ID_GITHUB_COPILOT => llm_provider_copilot::build(a, global_headers),
    id if ZAI_ALIASES.contains(&id) => llm_provider_zai::build(a),
    other => error::UnknownProviderSnafu {
      id: other.to_string(),
      account: a.id.clone(),
    }
    .fail(),
  }
}
