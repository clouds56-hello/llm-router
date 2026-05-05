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
use crate::pool::{AccountHandle, EndpointAcquire};
use crate::provider::{new_outbound_capture, Endpoint, OutboundCapture};
use crate::route::RouteResolution;
use axum::http::StatusCode;
use serde_json::Value;
use std::future::Future;
use std::sync::Arc;
use tracing::{debug, info_span, warn, Instrument};

const MAX_RETRIES: usize = 2;

pub(crate) struct DispatchOk {
  pub acct: Arc<AccountHandle>,
  pub resp: reqwest::Response,
  pub upstream_endpoint: Endpoint,
  /// The outbound snapshot captured by the provider during the *successful*
  /// attempt, if any. `None` if the provider didn't call
  /// `RequestCtx::capture_outbound` (e.g. it returned `Err` before reaching
  /// `.send()`).
  pub outbound: Option<OutboundSnapshot>,
}

struct DispatchAttempt {
  acct: Arc<AccountHandle>,
  upstream_endpoint: Endpoint,
  body: Arc<Value>,
  capture: OutboundCapture,
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
  route: &RouteResolution,
  endpoint: Endpoint,
  body: Arc<Value>,
  send: F,
) -> Result<DispatchOk, ApiError>
where
  F: Fn(Arc<AccountHandle>, Endpoint, Arc<Value>, OutboundCapture) -> Fut,
  Fut: Future<Output = crate::provider::Result<reqwest::Response>>,
{
  let mut last_err: Option<(StatusCode, String)> = None;

  for attempt in 0..=MAX_RETRIES {
    let resolved_model = &route.upstream_model;
    let DispatchAttempt {
      acct,
      upstream_endpoint,
      body: upstream_body,
      capture,
    } = prepare_attempt(state, session_id, route, endpoint, body.as_ref(), attempt)?;
    let account_id = acct.id();
    super::record_last_account(&account_id);

    let attempt_span = info_span!(
      "attempt",
      attempt,
      account = %account_id,
      provider = %acct.provider.info().id,
      %endpoint,
      upstream_endpoint = %upstream_endpoint,
      model = %route.requested_model,
      upstream_model = %resolved_model,
      status = tracing::field::Empty,
    );

    let result = async {
      debug!("sending upstream request");
      send(acct.clone(), upstream_endpoint, upstream_body.clone(), capture.clone()).await
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
      state.pool.record_session(id, &acct.id());
    }
    let outbound = capture.get().cloned();
    return Ok(DispatchOk {
      acct,
      resp,
      upstream_endpoint,
      outbound,
    });
  }

  let (status, msg) = last_err.unwrap_or((StatusCode::BAD_GATEWAY, "all attempts failed".into()));
  Err(ApiError::upstream(status, msg))
}

fn prepare_attempt(
  state: &AppState,
  session_id: Option<&str>,
  route: &RouteResolution,
  endpoint: Endpoint,
  body: &Value,
  attempt: usize,
) -> Result<DispatchAttempt, ApiError> {
  let (acct, upstream_endpoint) = acquire_attempt_account(state, session_id, route, endpoint, attempt)?;
  let capture = new_outbound_capture();
  let body = prepare_upstream_body(body, &route.upstream_model, endpoint, upstream_endpoint)?;
  Ok(DispatchAttempt {
    acct,
    upstream_endpoint,
    body,
    capture,
  })
}

fn acquire_attempt_account(
  state: &AppState,
  session_id: Option<&str>,
  route: &RouteResolution,
  endpoint: Endpoint,
  attempt: usize,
) -> Result<(Arc<AccountHandle>, Endpoint), ApiError> {
  match state.pool.acquire_for_route(session_id, route, endpoint) {
    EndpointAcquire::Account { acct, endpoint } => Ok((acct, endpoint)),
    EndpointAcquire::SessionExpired => {
      let id = session_id.unwrap_or_default();
      warn!(%endpoint, model = %route.requested_model, session_id = %id, attempt, "session expired");
      Err(ApiError::session_expired(id))
    }
    EndpointAcquire::None => {
      warn!(%endpoint, model = %route.requested_model, attempt, "no account supports endpoint/model");
      Err(ApiError::not_implemented(
        endpoint.to_string(),
        route.requested_model.clone(),
      ))
    }
  }
}

fn prepare_upstream_body(
  body: &Value,
  resolved_model: &str,
  endpoint: Endpoint,
  upstream_endpoint: Endpoint,
) -> Result<Arc<Value>, ApiError> {
  let routed_body = rewrite_model(body, resolved_model);
  if upstream_endpoint == endpoint {
    return Ok(Arc::new(routed_body));
  }
  match crate::convert::convert_request(endpoint, upstream_endpoint, &routed_body) {
    Ok(v) => Ok(Arc::new(v)),
    Err(e) => Err(ApiError::bad_gateway(format!("request conversion failed: {e}"))),
  }
}

fn rewrite_model(body: &Value, model: &str) -> Value {
  let mut body = body.clone();
  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".into(), Value::String(model.to_string()));
  }
  body
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::{Account as AccountCfg, AuthType, Config};
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
      enabled: true,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: Some(AuthType::Bearer),
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    });
    cfg.db.enabled = false;
    let state = build_state(&cfg, None).unwrap();
    let attempts = Arc::new(AtomicUsize::new(0));
    let url = format!("http://{addr}/probe");
    let http = state.http.clone();
    let result = dispatch(
      &state,
      None,
      &crate::route::RouteResolution {
        mode: llm_config::RouteMode::Route,
        requested_model: "".into(),
        upstream_model: "".into(),
        selector: crate::route::RouteSelector::Model,
      },
      Endpoint::ChatCompletions,
      Arc::new(serde_json::json!({ "messages": [] })),
      {
        let attempts = attempts.clone();
        let url = url.clone();
        let http = http.clone();
        move |_acct, _endpoint, _body, capture| {
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
      },
    )
    .await
    .unwrap();
    server.await.unwrap();
    assert_eq!(result.resp.status(), StatusCode::OK);
    assert_eq!(result.outbound.unwrap().body.as_ref(), b"attempt-2");
  }
}
