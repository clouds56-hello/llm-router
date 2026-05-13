pub mod auth;
pub mod models;
pub mod quota;
pub mod transform;
pub mod zai;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, ModelInfo, Provider, ProviderInfo, RequestCtx, Result, ID_ZAI, ID_ZAI_CODING_PLAN,
  ID_ZHIPUAI, ID_ZHIPUAI_CODING_PLAN, ZAI_PROVIDERS,
};
pub use llm_core::{account as config, provider, util};

pub use zai::*;

use std::sync::Arc;

pub static DESCRIPTOR_ZAI: llm_core::provider::ProviderDescriptor = descriptor(ID_ZAI);
pub static DESCRIPTOR_ZAI_CODING_PLAN: llm_core::provider::ProviderDescriptor = descriptor(ID_ZAI_CODING_PLAN);
pub static DESCRIPTOR_ZHIPUAI: llm_core::provider::ProviderDescriptor = descriptor(ID_ZHIPUAI);
pub static DESCRIPTOR_ZHIPUAI_CODING_PLAN: llm_core::provider::ProviderDescriptor = descriptor(ID_ZHIPUAI_CODING_PLAN);

const fn descriptor(id: &'static str) -> llm_core::provider::ProviderDescriptor {
  llm_core::provider::ProviderDescriptor {
    id,
    hosts: &["api.z.ai", "open.bigmodel.cn"],
    matches_url,
    validate,
    build,
  }
}

pub fn matches_url(host: &str, path: &str, id: &'static str) -> bool {
  match (host, id) {
    ("api.z.ai", ID_ZAI_CODING_PLAN) => path.starts_with("/api/coding/paas/v4"),
    ("api.z.ai", ID_ZAI) => path.is_empty() || path.starts_with("/api/paas/v4"),
    ("open.bigmodel.cn", ID_ZHIPUAI_CODING_PLAN) => path.starts_with("/api/coding/paas/v4"),
    ("open.bigmodel.cn", ID_ZHIPUAI) => path.is_empty() || path.starts_with("/api/paas/v4"),
    _ => false,
  }
}

pub fn validate(account: &llm_core::account::AccountConfig) -> llm_core::provider::Result<()> {
  zai::ZaiProvider::validate_account(account)
}

pub fn build(
  account: Arc<llm_core::account::AccountConfig>,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(zai::ZaiProvider::from_account(account)?))
}
