pub mod auth;
pub mod github_copilot;
pub mod headers;
pub mod import;
pub mod models;
pub mod oauth;
pub mod token;
pub mod user;

pub mod config;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, ID_GITHUB_COPILOT,
};
pub use llm_core::{provider, util};

pub use github_copilot::*;

use llm_auth::descriptor::{EndpointSpec, ProviderDescriptor};
use llm_auth::provider::CredentialFlavor;
use std::sync::Arc;

pub const COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";
pub const COPILOT_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
pub const COPILOT_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
pub const COPILOT_TOKEN_EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub const COPILOT_USER_INFO_URL: &str = "https://api.github.com/copilot_internal/user";

pub static DESCRIPTOR: ProviderDescriptor = ProviderDescriptor {
  id: ID_GITHUB_COPILOT,
  display_name: "GitHub Copilot",
  hosts: &["api.github.com", "api.githubcopilot.com"],
  base_url: COPILOT_BASE_URL,
  credentials: &[CredentialFlavor::RefreshToken],
  endpoints: &[
    EndpointSpec {
      endpoint: Endpoint::ChatCompletions,
      method: "POST",
      path: "/v1/chat/completions",
      aliases: &[],
    },
    EndpointSpec {
      endpoint: Endpoint::Messages,
      method: "POST",
      path: "/v1/messages",
      aliases: &[],
    },
    EndpointSpec {
      endpoint: Endpoint::Responses,
      method: "POST",
      path: "/v1/responses",
      aliases: &[],
    },
  ],
  rewrites: &[],
  auth_urls: &[
    ("device_authorize", COPILOT_DEVICE_CODE_URL),
    ("device_token", COPILOT_ACCESS_TOKEN_URL),
    ("token_exchange", COPILOT_TOKEN_EXCHANGE_URL),
    ("user_info", COPILOT_USER_INFO_URL),
  ],
  matches_url,
  validate,
  build,
  build_auth: Some(crate::auth::provider_auth),
};

pub fn matches_url(host: &str, _path: &str, _id: &'static str) -> bool {
  DESCRIPTOR.hosts.contains(&host)
}

pub fn validate(account: &llm_core::account::AccountConfig) -> llm_core::provider::Result<()> {
  github_copilot::CopilotProvider::validate_account(account)
}

pub fn build(
  account: Arc<llm_core::account::AccountConfig>,
) -> llm_core::provider::Result<Arc<dyn llm_core::provider::Provider>> {
  Ok(Arc::new(github_copilot::CopilotProvider::from_account(account)?))
}
