pub mod auth;
pub mod openai;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, ID_CODEX, ID_OPENAI,
};
pub use llm_core::{account as config, provider, util};

pub use openai::*;

use std::sync::Arc;

pub static DESCRIPTOR_OPENAI: llm_core::provider::ProviderDescriptor = descriptor(ID_OPENAI);
pub static DESCRIPTOR_CODEX: llm_core::provider::ProviderDescriptor = descriptor(ID_CODEX);

const fn descriptor(id: &'static str) -> llm_core::provider::ProviderDescriptor {
  llm_core::provider::ProviderDescriptor {
    id,
    hosts: &["api.openai.com", "chatgpt.com"],
    matches_url,
    validate,
    build,
  }
}

pub fn matches_url(host: &str, path: &str, id: &'static str) -> bool {
  match (host, id) {
    ("api.openai.com", ID_OPENAI) => true,
    ("chatgpt.com", ID_CODEX) => path.starts_with("/backend-api/codex"),
    _ => false,
  }
}

pub fn validate(account: &llm_core::account::AccountConfig) -> llm_core::provider::Result<()> {
  openai::OpenAiProvider::validate_account(account)
}

pub fn build(
  account: Arc<llm_core::account::AccountConfig>,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(openai::OpenAiProvider::from_account(account)?))
}
