//! Production [`AccountSelector`] backed by [`llm_accounts::AccountPool`].
//!
//! [`PoolAccountSelector`] bridges the requests [`AccountSelector`] trait
//! to the existing [`AccountPool`] + [`RouteResolver`] machinery. It
//! mirrors what `crates/router/src/pipeline/request.rs::resolve_account`
//! does, but returns a typed [`SelectorOutcome`] instead of poking at an
//! `AppState`.
//!
//! The selector takes ownership of a `RouteResolver` (cheap to clone via
//! `Arc`) and an `AccountPool`, so it can be assembled directly from the
//! CLI / gateway without dragging the legacy `AppState`.

use super::stage::{AccountSelector, SelectorOutcome};
use crate::event::Stage;
use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::{PipelineError, RequestsError};
use crate::pipeline::stages::Extracted;
use async_trait::async_trait;
use llm_accounts::{AccountPool, EndpointAcquire, RouteResolver};
use smol_str::SmolStr;
use std::sync::Arc;

pub struct PoolAccountSelector {
  pool: Arc<AccountPool>,
  resolver: Arc<RouteResolver>,
}

impl PoolAccountSelector {
  pub fn new(pool: Arc<AccountPool>, resolver: Arc<RouteResolver>) -> Self {
    Self { pool, resolver }
  }
}

#[async_trait]
impl AccountSelector for PoolAccountSelector {
  async fn select(&self, ctx: &PipelineCtx, extracted: &Extracted) -> Result<SelectorOutcome, PipelineError> {
    // Route mode hint comes from the inbound `x-route-mode` header (or
    // equivalent) — `DefaultExtract` parses this into
    // `extracted.route_mode_hint`.
    let route = self
      .resolver
      .resolve(extracted.model.as_str(), extracted.route_mode_hint.as_deref())
      .map_err(|e| PipelineError::permanent(Stage::Resolve, RequestsError::Resolve { source: e }))?;

    match self
      .pool
      .acquire_for_route(extracted.session_id.as_deref(), &route, ctx.endpoint)
    {
      EndpointAcquire::Account { acct, endpoint } => {
        let provider_id = SmolStr::from(acct.provider.info().id.as_str());
        let account_id = SmolStr::from(acct.id());
        Ok(SelectorOutcome::Selected {
          account_id,
          provider_id,
          upstream_endpoint: endpoint,
          upstream_model: SmolStr::from(route.upstream_model.as_str()),
          account_handle: acct,
        })
      }
      EndpointAcquire::SessionExpired => Ok(SelectorOutcome::SessionExpired {
        session_id: extracted.session_id.clone().unwrap_or_else(|| SmolStr::new("")),
      }),
      EndpointAcquire::None => Ok(SelectorOutcome::NoAccount),
    }
  }
}
