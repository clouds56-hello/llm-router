pub mod auth;
pub mod deepseek;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, HeaderPatchCtx, TemplateVars, Provider, ProviderInfo, RequestCtx, Result, ID_DEEPSEEK,
};
pub use llm_core::{account as config, provider, util};

pub use deepseek::*;

use llm_auth::descriptor::{EndpointSpec, ProviderDescriptor};
use llm_auth::provider::CredentialFlavor;
use std::sync::Arc;

pub static DEFAULT_ENDPOINTS: &[Endpoint] = &[Endpoint::ChatCompletions, Endpoint::Messages];

pub static DESCRIPTOR: ProviderDescriptor = ProviderDescriptor {
  id: ID_DEEPSEEK,
  display_name: "DeepSeek",
  hosts: &["api.deepseek.com"],
  base_url: deepseek::DEFAULT_BASE_URL,
  credentials: &[CredentialFlavor::ApiKey],
  endpoints: &[
    EndpointSpec {
      endpoint: Endpoint::ChatCompletions,
      method: "POST",
      path: "/v1/chat/completions",
      aliases: &["/chat/completions"],
    },
    EndpointSpec {
      endpoint: Endpoint::Messages,
      method: "POST",
      path: "/v1/messages",
      aliases: &["/anthropic/v1/messages"],
    },
  ],
  model_endpoint_rules: Some(&[]),
  rewrites: &[],
  auth_urls: &[],
  matches_url,
  validate,
  build,
  build_auth: Some(crate::auth::provider_auth),
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
