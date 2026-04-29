use llm_core::account::Account;
use llm_core::provider::{error, Provider, Result, ID_GITHUB_COPILOT, ZAI_ALIASES};
use llm_provider_copilot::config::CopilotHeaders;
use std::sync::Arc;

pub fn build_for_account(a: &Account, global_headers: &serde_json::Value) -> Result<Arc<dyn Provider>> {
  match a.provider.as_str() {
    ID_GITHUB_COPILOT => {
      let headers = CopilotHeaders::from_value(global_headers)?;
      llm_provider_copilot::build(a, &headers)
    }
    id if ZAI_ALIASES.contains(&id) => llm_provider_zai::build(a),
    other => error::UnknownProviderSnafu {
      id: other.to_string(),
      account: a.id.clone(),
    }
    .fail(),
  }
}
