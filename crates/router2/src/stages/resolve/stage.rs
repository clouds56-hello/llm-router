//! Resolve stage — picks the upstream account and upstream model for an
//! extracted request.
//!
//! [`PoolResolve`] is the production wiring (PR2 onward) but in PR1 it depends
//! on a small [`AccountSelector`] trait rather than the legacy
//! `crates/router::accounts::AccountPool`. This keeps router2 free of any
//! dependency on the legacy crate; PR2 will provide a real implementation of
//! [`AccountSelector`] backed by the existing pool (or its successor in a
//! shared crate).

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::pipeline::stages::{Extracted, ResolveStage, Resolved};
use crate::event::Stage;
use async_trait::async_trait;
use llm_accounts::AccountHandle;
use llm_core::provider::Endpoint;
use smol_str::SmolStr;
use std::sync::Arc;

/// Outcome of consulting an account pool for a given extracted request.
pub enum SelectorOutcome {
  /// An account was selected. The handle is typed (not `Arc<dyn Any>`)
  /// so back-half stages can reach the provider via
  /// `handle.provider.input_transformer()` without a downcast.
  Selected {
    account_id: SmolStr,
    provider_id: SmolStr,
    upstream_endpoint: Endpoint,
    upstream_model: SmolStr,
    account_handle: Arc<AccountHandle>,
  },
  /// A session-affinity binding existed but its account is no longer
  /// available; the caller's session has effectively expired.
  SessionExpired { session_id: SmolStr },
  /// No account supports this endpoint+model combination.
  NoAccount,
}

#[async_trait]
pub trait AccountSelector: Send + Sync {
  async fn select(&self, extracted: &Extracted) -> Result<SelectorOutcome, PipelineError>;
}

pub struct PoolResolve<S: AccountSelector> {
  pub selector: Arc<S>,
}

impl<S: AccountSelector> PoolResolve<S> {
  pub fn new(selector: Arc<S>) -> Self {
    Self { selector }
  }
}

#[async_trait]
impl<S: AccountSelector + 'static> ResolveStage for PoolResolve<S> {
  async fn resolve(&self, _ctx: &PipelineCtx, extracted: &Extracted) -> Result<Resolved, PipelineError> {
    match self.selector.select(extracted).await? {
      SelectorOutcome::Selected {
        account_id,
        provider_id,
        upstream_endpoint,
        upstream_model,
        account_handle,
      } => Ok(Resolved {
        client_id: extracted.client_id.clone(),
        model: extracted.model.clone(),
        upstream_model,
        upstream_endpoint,
        account_id,
        provider_id,
        account_handle,
      }),
      SelectorOutcome::SessionExpired { session_id } => Err(PipelineError::permanent(
        Stage::Resolve,
        SmolStr::new(format!("session expired: {session_id}")),
      )),
      SelectorOutcome::NoAccount => Err(PipelineError::permanent(
        Stage::Resolve,
        SmolStr::new(format!(
          "no account supports endpoint {} for model {}",
          extracted.endpoint, extracted.model
        )),
      )),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use crate::pipeline::stages::Extracted;
  use crate::test_support::mock_handle;
  use bytes::Bytes;
  use llm_headers::HeaderMap;

  struct FixedSelector(SelectorOutcomeKind);

  enum SelectorOutcomeKind {
    Ok,
    None,
  }

  #[async_trait]
  impl AccountSelector for FixedSelector {
    async fn select(&self, _ex: &Extracted) -> Result<SelectorOutcome, PipelineError> {
      Ok(match self.0 {
        SelectorOutcomeKind::Ok => SelectorOutcome::Selected {
          account_id: SmolStr::new("acct-1"),
          provider_id: SmolStr::new("zai-coding-plan"),
          upstream_endpoint: Endpoint::ChatCompletions,
          upstream_model: SmolStr::new("glm-4"),
          account_handle: mock_handle("acct-1", "zai-coding-plan"),
        },
        SelectorOutcomeKind::None => SelectorOutcome::NoAccount,
      })
    }
  }

  fn fake_extracted() -> Extracted {
    Extracted {
      endpoint: Endpoint::ChatCompletions,
      client_id: None,
      model: SmolStr::new("input-model"),
      stream: false,
      session_id: None,
      project_id: None,
      initiator: SmolStr::new("user"),
      header_initiator: None,
      route_mode_hint: None,
      headers: HeaderMap::new(),
      raw_body: Bytes::new(),
      decoded_body: Bytes::new(),
      body_json: serde_json::Value::Null,
      content_encoding: None,
    }
  }

  fn ctx() -> PipelineCtx {
    PipelineCtx::new("req-r", Arc::new(EventBus::new()))
  }

  #[tokio::test]
  async fn happy_path_carries_upstream_model_and_provider() {
    let stage = PoolResolve::new(Arc::new(FixedSelector(SelectorOutcomeKind::Ok)));
    let res = stage.resolve(&ctx(), &fake_extracted()).await.unwrap();
    assert_eq!(res.upstream_model, "glm-4");
    assert_eq!(res.account_id, "acct-1");
    assert_eq!(res.provider_id, "zai-coding-plan");
    assert_eq!(res.model, "input-model");
  }

  #[tokio::test]
  async fn no_account_yields_permanent_resolve_error() {
    let stage = PoolResolve::new(Arc::new(FixedSelector(SelectorOutcomeKind::None)));
    let err = stage.resolve(&ctx(), &fake_extracted()).await.unwrap_err();
    assert_eq!(err.stage, Stage::Resolve);
    assert!(!err.recoverable);
    assert!(err.message.contains("no account"));
  }
}
