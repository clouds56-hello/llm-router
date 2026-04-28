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
use crate::db::OutboundSnapshot;
use crate::pool::{Account, SessionAcquire};
use crate::provider::{new_outbound_capture, Endpoint, OutboundCapture};
use axum::http::StatusCode;
use std::future::Future;
use std::sync::Arc;
use tracing::{debug, info_span, warn, Instrument};

const MAX_RETRIES: usize = 2;

pub(crate) struct DispatchOk {
  pub acct: Arc<Account>,
  pub resp: reqwest::Response,
  /// The outbound snapshot captured by the provider during the *successful*
  /// attempt, if any. `None` if the provider didn't call
  /// `RequestCtx::capture_outbound` (e.g. it returned `Err` before reaching
  /// `.send()`).
  pub outbound: Option<OutboundSnapshot>,
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
  F: Fn(Arc<Account>, OutboundCapture) -> Fut,
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

    let capture = new_outbound_capture();
    let result = async {
      debug!("sending upstream request");
      send(acct.clone(), capture.clone()).await
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
    let outbound = capture.get().cloned();
    return Ok(DispatchOk { acct, resp, outbound });
  }

  let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
  Err(ApiError::upstream(status, msg))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::{Account as AccountCfg, Config, ZaiAccountConfig};
  use crate::server::build_state;
  use crate::util::secret::Secret;
  use bytes::Bytes;
  use std::sync::atomic::{AtomicUsize, Ordering};
  use tokio::io::{AsyncReadExt, AsyncWriteExt};

  #[tokio::test]
  async fn returns_outbound_from_last_successful_attempt() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
      for status in [500, 200] {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = vec![0_u8; 1024];
        let _ = stream.read(&mut buf).await.unwrap();
        let line = if status == 200 {
          "HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}"
        } else {
          "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\n\r\nfail"
        };
        stream.write_all(line.as_bytes()).await.unwrap();
      }
    });

    let mut cfg = Config::default();
    cfg.accounts.push(AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      github_token: None,
      api_token: None,
      api_token_expires_at: None,
      api_key: Some(Secret::new("sk-test".into())),
      copilot: None,
      zai: Some(ZaiAccountConfig { base_url: None }),
      behave_as: None,
    });
    cfg.db.enabled = false;
    let state = build_state(&cfg).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let url = format!("http://{addr}/probe");
    let http = state.http.clone();
    let result = dispatch(&state, None, "", Endpoint::ChatCompletions, {
      let attempts = attempts.clone();
      let url = url.clone();
      let http = http.clone();
      move |_acct, capture| {
        let attempts = attempts.clone();
        let url = url.clone();
        let http = http.clone();
        async move {
          let n = attempts.fetch_add(1, Ordering::SeqCst) + 1;
          let mut headers = reqwest::header::HeaderMap::new();
          headers.insert(
            "x-attempt",
            reqwest::header::HeaderValue::from_str(&n.to_string()).unwrap(),
          );
          let _ = capture.set(crate::db::OutboundSnapshot {
            method: Some("POST".into()),
            url: Some(url.clone()),
            status: None,
            headers,
            body: Bytes::from(format!("attempt-{n}")),
          });
          http
            .get(&url)
            .send()
            .await
            .map_err(|source| crate::provider::error::Error::Http {
              what: "dispatch test",
              source,
            })
        }
      }
    })
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(result.resp.status(), StatusCode::OK);
    assert_eq!(result.outbound.unwrap().body.as_ref(), b"attempt-2");
  }
}
