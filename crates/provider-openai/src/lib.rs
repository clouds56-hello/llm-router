pub mod auth;
pub mod auth_codex;
pub mod auth_openai;
pub mod jwt;
pub mod openai;

pub use llm_catalogue as catalogue;
pub use llm_core::provider::{
  error, AuthKind, Endpoint, HeaderPatchCtx, Provider, ProviderInfo, RequestCtx, Result, ID_CODEX, ID_OPENAI,
};
pub use llm_core::{account as config, provider, util};

pub use openai::*;

use llm_auth::descriptor::{EndpointSpec, ProviderDescriptor};
use llm_auth::provider::CredentialFlavor;
use std::sync::Arc;

pub const CODEX_DEVICE_USERCODE_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/usercode";
pub const CODEX_DEVICE_TOKEN_URL: &str = "https://auth.openai.com/api/accounts/deviceauth/token";
pub const CODEX_OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const CODEX_DEVICE_VERIFY_URL: &str = "https://auth.openai.com/codex/device";
pub const CODEX_DEVICE_REDIRECT_URL: &str = "https://auth.openai.com/deviceauth/callback";

pub static DESCRIPTOR_OPENAI: ProviderDescriptor = ProviderDescriptor {
  id: ID_OPENAI,
  display_name: "OpenAI",
  hosts: &["api.openai.com"],
  base_url: openai::OPENAI_BASE_URL,
  credentials: &[CredentialFlavor::ApiKey],
  endpoints: &[
    EndpointSpec {
      endpoint: Endpoint::ChatCompletions,
      method: "POST",
      path: "/v1/chat/completions",
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
  auth_urls: &[],
  matches_url,
  validate,
  build,
  build_auth: Some(crate::auth::openai_auth),
};

pub static DESCRIPTOR_CODEX: ProviderDescriptor = ProviderDescriptor {
  id: ID_CODEX,
  display_name: "ChatGPT (Codex)",
  hosts: &["chatgpt.com"],
  base_url: openai::CODEX_BASE_URL,
  credentials: &[CredentialFlavor::RefreshToken, CredentialFlavor::ApiKey],
  endpoints: &[EndpointSpec {
    endpoint: Endpoint::Responses,
    method: "POST",
    path: "/v1/responses",
    aliases: &["/backend-api/codex/responses"],
  }],
  rewrites: &[],
  auth_urls: &[
    ("device_usercode", CODEX_DEVICE_USERCODE_URL),
    ("device_token", CODEX_DEVICE_TOKEN_URL),
    ("oauth_token", CODEX_OAUTH_TOKEN_URL),
    ("device_verify", CODEX_DEVICE_VERIFY_URL),
    ("device_redirect", CODEX_DEVICE_REDIRECT_URL),
  ],
  matches_url,
  validate,
  build,
  build_auth: Some(crate::auth::codex_auth),
};

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
