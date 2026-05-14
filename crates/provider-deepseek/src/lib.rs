pub mod auth;
pub mod deepseek;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, ID_DEEPSEEK,
};
pub use llm_core::{account as config, provider, util};

pub use deepseek::*;

use std::sync::Arc;

pub static DESCRIPTOR: llm_core::provider::ProviderDescriptor = llm_core::provider::ProviderDescriptor {
  id: ID_DEEPSEEK,
  hosts: &["api.deepseek.com"],
  matches_url,
  validate,
  build,
};

pub fn matches_url(host: &str, _path: &str, _id: &'static str) -> bool {
  DESCRIPTOR.hosts.contains(&host)
}

pub fn validate(account: &llm_core::account::AccountConfig) -> llm_core::provider::Result<()> {
  deepseek::DeepSeekProvider::validate_account(account)
}

pub fn build(
  account: Arc<llm_core::account::AccountConfig>,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(deepseek::DeepSeekProvider::from_account(account)?))
}
