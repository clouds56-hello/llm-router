pub mod github_copilot;

pub use llm_core::{config, provider, util};
pub use llm_core::provider::{error, AuthKind, Endpoint, Provider, ProviderInfo, RequestCtx, Result, ID_GITHUB_COPILOT};
pub use llm_catalogue as catalogue;

pub use github_copilot::*;

use std::sync::Arc;

pub fn build(
  account: &llm_core::config::Account,
  headers: &llm_core::config::CopilotHeaders,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(github_copilot::CopilotProvider::from_account(account, headers)?))
}
