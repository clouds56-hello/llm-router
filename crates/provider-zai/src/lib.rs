pub mod zai;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, ModelInfo, Provider, ProviderInfo, RequestCtx, Result, ZAI_ALIASES,
};
pub use llm_core::{account as config, provider, util};

pub use zai::*;

use std::sync::Arc;

pub fn build(
  account: &llm_core::account::Account,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(zai::ZaiProvider::from_account(account)?))
}
