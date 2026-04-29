pub mod github_copilot;
pub mod headers;
pub mod models;
pub mod oauth;
pub mod token;
pub mod user;

pub mod config;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, Provider, ProviderInfo, RequestCtx, Result, ID_GITHUB_COPILOT,
};
pub use llm_core::{provider, util};

pub use github_copilot::*;

use std::sync::Arc;

pub static DESCRIPTOR: llm_core::provider::ProviderDescriptor = llm_core::provider::ProviderDescriptor {
  id: ID_GITHUB_COPILOT,
  validate,
  build,
};

pub fn validate(account: &llm_core::account::AccountConfig) -> llm_core::provider::Result<()> {
  github_copilot::CopilotProvider::validate_account(account)
}

pub fn build(
  account: Arc<llm_core::account::AccountConfig>,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(github_copilot::CopilotProvider::from_account(account)?))
}
