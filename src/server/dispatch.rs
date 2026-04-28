//! Endpoint-agnostic retry / cooldown / 401-invalidation loop.
//!
//! Every inbound endpoint (`/v1/chat/completions`, `/v1/responses`,
//! `/v1/messages`) has the same upstream-failure semantics:
//! - 401 → invalidate cached credentials, retry on a fresh account
//! - 403 / 429 / 5xx → mark account in cooldown, retry on another account
//! - any other status → forward verbatim to the client
//!
//! Per-endpoint handlers parameterise this loop with:
//! - the [`Endpoint`] tag (drives pool selection + logging),
//! - the model name, and
//! - a closure that builds the right [`RequestCtx`] and calls the matching
//!   `Provider::{chat,responses,messages}` method.

use super::error::ApiError;
use super::AppState;
use crate::pool::{Account, SessionAcquire};
use crate::provider::Endpoint;
use axum::http::StatusCode;
use std::future::Future;
use std::sync::Arc;
use tracing::{debug, info_span, warn, Instrument};

const MAX_RETRIES: usize = 2;

pub(crate) struct DispatchOk {
  pub acct: Arc<Account>,
  pub resp: reqwest::Response,
}

/// Run up to `MAX_RETRIES + 1` attempts of `send` against accounts the pool
/// hands us, applying the standard retry / cooldown / credential-invalidation
/// rules.
///
/// Returns `Err(ApiError)` with status `501` if the pool has no account that
/// supports `(model, endpoint)`, or `502 + last upstream message` if every
/// attempt failed.
pub(crate) async fn dispatch<F, Fut>(
  state: &AppState,
  session_id: Option<&str>,
  model: &str,
  endpoint: Endpoint,
  send: F,
) -> Result<DispatchOk, ApiError>
where
  F: Fn(Arc<Account>) -> Fut,
  Fut: Future<Output = crate::provider::Result<reqwest::Response>>,
{
  let mut last_err: Option<(StatusCode, String)> = None;

  for attempt in 0..=MAX_RETRIES {
    let acct = match state.pool.acquire_for_session(session_id, Some(model), endpoint) {
      SessionAcquire::Account(acct) => acct,
      SessionAcquire::SessionExpired => {
        let id = session_id.unwrap_or_default();
        warn!(%endpoint, %model, session_id = %id, attempt, "session expired");
        return Err(ApiError::session_expired(id));
      }
      SessionAcquire::None => {
        // No account in the pool advertises this endpoint at all — this
        // is a configuration / capability mismatch, not a transient
        // failure, so don't retry.
        warn!(%endpoint, %model, attempt, "no account supports endpoint/model");
        return Err(ApiError::not_implemented(endpoint.to_string(), model.to_string()));
      }
    };
    super::record_last_account(&acct.id);

    let attempt_span = info_span!(
      "attempt",
      attempt,
      account = %acct.id,
      provider = %acct.provider.info().id,
      %endpoint,
      %model,
      status = tracing::field::Empty,
    );

    let result = async {
      debug!("sending upstream request");
      send(acct.clone()).await
    }
    .instrument(attempt_span.clone())
    .await;

    let resp = match result {
      Ok(r) => r,
      Err(e) => {
        warn!(parent: &attempt_span, error = %e, "provider request failed");
        acct.mark_failure(state.pool.cooldown_base());
        last_err = Some((StatusCode::BAD_GATEWAY, e.to_string()));
        continue;
      }
    };

    let status = resp.status();
    attempt_span.record("status", status.as_u16());

    if status == StatusCode::UNAUTHORIZED {
      warn!(parent: &attempt_span, "401 from upstream; refreshing creds");
      acct.invalidate_credentials();
      last_err = Some((status, "unauthorized".into()));
      continue;
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::FORBIDDEN || status.is_server_error() {
      let body_text = resp.text().await.unwrap_or_default();
      warn!(parent: &attempt_span, %status, body = %body_text, "upstream error; cooldown");
      acct.mark_failure(state.pool.cooldown_base());
      last_err = Some((status, body_text));
      continue;
    }

    debug!(parent: &attempt_span, "upstream accepted");
    acct.mark_success();
    if let Some(id) = session_id {
      state.pool.record_session(id, &acct.id);
    }
    return Ok(DispatchOk { acct, resp });
  }

  let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
  Err(ApiError::upstream(status, msg))
}
