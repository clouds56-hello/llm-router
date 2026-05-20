//! Resolve stage for the MITM proxy passthrough pipeline.
//!
//! Unlike [`PoolResolve`](super::PoolResolve), the proxy variant has **no
//! account selection** — the intercepted TLS host (e.g. `api.openai.com`)
//! is the upstream, and the client's own `Authorization` header reaches
//! the upstream untouched. We still need to satisfy the [`Resolved`]
//! shape, so this stage:
//!
//! * Reads `proxy.host` and `proxy.provider_id` from
//!   [`PipelineCtx::config`] (the [`RunConfig`] populated by the proxy
//!   transport layer before calling `pipeline.run_with`).
//! * Picks a dummy `account_id` (`"proxy"`) and a `provider_id` set to
//!   either the caller-supplied value or the host.
//! * Constructs a [`ProxyStubProvider`] inside an [`AccountHandle`]; the
//!   handle is required by the [`Resolved`] struct contract but is never
//!   read by [`ProxySend`](super::super::send::ProxySend), which builds
//!   the outbound request directly from the bag.
//!
//! [`RunConfig`]: crate::RunConfig
//! [`AccountHandle`]: llm_accounts::AccountHandle

use crate::event::Stage;
use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::{PipelineError, RequestsError};
use crate::pipeline::stages::{Extracted, ResolveStage, Resolved};
use async_trait::async_trait;
use llm_accounts::AccountHandle;
use llm_core::account::AccountConfig;
use llm_core::provider::{AuthKind, ModelCache, Provider, ProviderInfo};
use serde_json::Value;
use smol_str::SmolStr;
use std::sync::Arc;

/// Config keys consumed by [`ProxyResolve`]. The proxy transport layer
/// is responsible for populating these before calling
/// `pipeline.run_with`.
pub mod keys {
  pub const HOST: &str = "proxy.host";
  pub const PROVIDER_ID: &str = "proxy.provider_id";
  pub const ACCOUNT_ID: &str = "proxy.account_id";
}

pub struct ProxyResolve;

#[async_trait]
impl ResolveStage for ProxyResolve {
  async fn resolve(&self, ctx: &PipelineCtx, extracted: &Extracted) -> Result<Resolved, PipelineError> {
    let host = ctx.config.get_str(keys::HOST).ok_or_else(|| {
      let msg = format!("proxy passthrough pipeline requires `{}` in RunConfig", keys::HOST);
      PipelineError::permanent(
        Stage::Resolve,
        RequestsError::Other {
          source: msg.into(),
        },
      )
    })?;
    let provider_id = ctx.config.get_str(keys::PROVIDER_ID).unwrap_or(host);
    let account_id = ctx.config.get_str(keys::ACCOUNT_ID).unwrap_or("proxy");
    Ok(Resolved {
      client_id: extracted.client_id.clone(),
      model: extracted.model.clone(),
      upstream_model: extracted.model.clone(),
      upstream_endpoint: ctx.endpoint,
      account_id: SmolStr::new(account_id),
      provider_id: SmolStr::new(provider_id),
      account_handle: stub_handle(account_id, provider_id),
    })
  }
}

/// Sentinel [`AccountHandle`] for the proxy pipeline. Carries enough
/// metadata to satisfy the [`Resolved`] type contract but is **never
/// consulted** by [`ProxySend`](super::super::send::ProxySend) — the
/// outbound request is built directly from `ctx.config` + the inbound
/// bytes + the pruned headers from [`PassthroughBuildHeaders`].
pub(crate) fn stub_handle(account_id: &str, provider_id: &str) -> Arc<AccountHandle> {
  let cfg = AccountConfig {
    id: account_id.to_string(),
    provider: provider_id.to_string(),
    enabled: true,
    tier: Default::default(),
    tags: Vec::new(),
    label: None,
    base_url: None,
    headers: Default::default(),
    auth_type: None,
    username: None,
    api_key: None,
    api_key_expires_at: None,
    access_token: None,
    access_token_expires_at: None,
    id_token: None,
    refresh_token: None,
    provider_account_id: None,
    extra: Default::default(),
    refresh_url: None,
    last_refresh: None,
    settings: Default::default(),
  };
  let provider = Arc::new(ProxyStubProvider::new(provider_id));
  Arc::new(AccountHandle::new(Arc::new(cfg), provider))
}

/// Stub provider that returns its [`ProviderInfo`] but panics on any
/// outbound call. The proxy passthrough pipeline never invokes
/// `Provider::chat / responses / messages` (those would route through
/// `DefaultSend`; the proxy uses `ProxySend` instead). The panic is a
/// loud invariant check: if someone ever wires this stub into a path
/// that calls the provider's request methods, we want to know
/// immediately rather than silently fall back to a 500.
pub struct ProxyStubProvider {
  info: ProviderInfo,
}

impl ProxyStubProvider {
  fn new(provider_id: &str) -> Self {
    Self {
      info: ProviderInfo {
        id: provider_id.to_string(),
        aliases: &[],
        display_name: "proxy-stub",
        upstream_url: String::new(),
        auth_kind: AuthKind::StaticApiKey,
        default_models: Vec::new(),
        default_endpoints: &[],
        model_cache: Arc::new(ModelCache::default()),
      },
    }
  }
}

#[async_trait]
impl Provider for ProxyStubProvider {
  fn id(&self) -> &str {
    &self.info.id
  }

  fn info(&self) -> &ProviderInfo {
    &self.info
  }

  async fn list_models(&self, _http: &reqwest::Client) -> llm_core::provider::error::Result<Value> {
    Ok(Value::Null)
  }

  async fn chat(
    &self,
    _ctx: llm_core::provider::RequestCtx<'_>,
  ) -> llm_core::provider::error::Result<reqwest::Response> {
    unreachable!("ProxyStubProvider::chat called; proxy pipeline must use ProxySend, not DefaultSend")
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use crate::pipeline::config::RunConfig;
  use crate::pipeline::stages::Extracted;
  use bytes::Bytes;
  use llm_core::provider::Endpoint;
  use llm_headers::HeaderMap;

  fn fake_extracted() -> Extracted {
    Extracted {
      client_id: None,
      model: SmolStr::new("gpt-4"),
      stream: false,
      session_id: None,
      project_id: None,
      initiator: SmolStr::new("user"),
      header_initiator: None,
      route_mode_hint: None,
      headers: HeaderMap::new(),
      raw_body: Bytes::new(),
      decoded_body: Bytes::new(),
      body_json: Arc::new(Value::Null),
      content_encoding: None,
    }
  }

  fn ctx_with(config: RunConfig) -> PipelineCtx {
    PipelineCtx::new_with_config(
      "req-px",
      Endpoint::ChatCompletions,
      Arc::new(EventBus::new(64)),
      Arc::new(config),
    )
  }

  #[tokio::test]
  async fn reads_host_from_config() {
    let cfg = RunConfig::builder()
      .with_str(keys::HOST, "api.openai.com")
      .build();
    let res = ProxyResolve.resolve(&ctx_with(cfg), &fake_extracted()).await.unwrap();
    assert_eq!(res.account_id, "proxy");
    assert_eq!(res.provider_id, "api.openai.com");
    assert_eq!(res.upstream_model, "gpt-4");
    assert_eq!(res.upstream_endpoint, Endpoint::ChatCompletions);
  }

  #[tokio::test]
  async fn missing_host_is_permanent_error() {
    let err = ProxyResolve
      .resolve(&ctx_with(RunConfig::default()), &fake_extracted())
      .await
      .unwrap_err();
    assert_eq!(err.stage, Stage::Resolve);
    assert!(!err.recoverable);
    assert!(err.message().contains("proxy.host"));
  }

  #[tokio::test]
  async fn explicit_provider_id_and_account_id_override_defaults() {
    let cfg = RunConfig::builder()
      .with_str(keys::HOST, "api.openai.com")
      .with_str(keys::PROVIDER_ID, "openai")
      .with_str(keys::ACCOUNT_ID, "user-bearer")
      .build();
    let res = ProxyResolve.resolve(&ctx_with(cfg), &fake_extracted()).await.unwrap();
    assert_eq!(res.account_id, "user-bearer");
    assert_eq!(res.provider_id, "openai");
  }
}
