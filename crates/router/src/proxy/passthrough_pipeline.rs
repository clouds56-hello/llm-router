//! Proxy MITM passthrough dispatch via the shared `llm-requests`
//! [`Pipeline`].
//!
//! This is the pipeline-based replacement for [`super::passthrough::proxy_passthrough`].
//! It builds a [`llm_requests::RawInbound`] from the intercepted request,
//! supplies a [`llm_requests::RunConfig`] populated with the intercepted
//! TLS host / method / path under the `proxy.*` keys, and invokes
//! [`AppState::proxy_passthrough_pipeline`] via
//! [`llm_requests::Pipeline::run_with`].
//!
//! The pipeline itself ([`ProxyResolve`] + [`ProxySend`]) reads those
//! keys, dispatches the request to `https://{host}{path}` preserving
//! the client's own `Authorization`, and emits the standard
//! `RecordEvent::*` observability stream — no legacy `LegacyRequest`
//! events are produced here.
//!
//! [`ProxyResolve`]: llm_requests::stages::ProxyResolve
//! [`ProxySend`]: llm_requests::stages::ProxySend
//! [`AppState::proxy_passthrough_pipeline`]: crate::api::AppState::proxy_passthrough_pipeline

use crate::api::error::ApiError;
use crate::api::AppState;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Request, Response};
use axum::response::IntoResponse;
use llm_core::provider::Endpoint;

/// Dispatch an intercepted MITM request through the proxy-passthrough
/// pipeline.
///
/// `host` is the authority of the intercepted CONNECT tunnel (no port);
/// the pipeline builds `https://{host}{path_and_query}` and forwards
/// the request body and headers verbatim, minus hop-by-hop and
/// router-owned headers (stripped inside [`PassthroughBuildHeaders`]).
///
/// `peer_addr` / `local_addr` are unused for now — kept in the
/// signature so the call site in [`super::transport`] stays a
/// one-line swap from the legacy helper. They can be threaded into
/// `RecordEvent::InboundConnection` once `PassthroughExtract` is
/// extended to accept them via [`llm_requests::RunConfig`].
///
/// [`PassthroughBuildHeaders`]: llm_requests::stages::PassthroughBuildHeaders
pub(super) async fn proxy_passthrough_via_pipeline(
  state: &AppState,
  host: &str,
  _peer_addr: std::net::SocketAddr,
  _local_addr: std::net::SocketAddr,
  req: Request<hyper::body::Incoming>,
) -> Result<Response<Body>> {
  let path_and_query = req
    .uri()
    .path_and_query()
    .map(|v| v.as_str().to_string())
    .unwrap_or_else(|| "/".to_string());
  let path_only = path_and_query.split('?').next().unwrap_or(&path_and_query).to_string();
  let method = req.method().clone();

  let (parts, body) = req.into_parts();
  let raw_body = axum::body::to_bytes(Body::new(body), usize::MAX)
    .await
    .context("read proxy passthrough request body")?;

  // The pipeline never inspects this for proxy passthrough (no body
  // parse, no route lookup), but `RawInbound` requires a value.
  // Pick the closest match from the path so downstream introspection
  // (e.g. logs) is sensible.
  let endpoint = infer_endpoint(&path_only);

  // Pre-decoded body matches raw body — `PassthroughConvertRequest`
  // forwards bytes verbatim and never reads `decoded_body`. Same for
  // `body_json` (kept as `Null`).
  let decoded_body = raw_body.clone();
  let body_json = serde_json::Value::Null;

  // Identity resolution — fingerprint the inbound bearer against
  // locally-known accounts so DB rows / events attribute the
  // intercepted request to a concrete `account_id` + `provider_id`
  // when we can. This mirrors the legacy proxy passthrough path
  // (`super::passthrough::proxy_passthrough` lines 122–131).
  //
  // Fallbacks (also legacy-parity):
  //   * `provider_id` → intercepted `host` when neither the
  //     fingerprint table nor the URL registry has a hit.
  //   * `account_id` → `account_fp_<suffix>` synthesised by
  //     `AccountIdentityResolver::resolve` for long bearer tokens;
  //     otherwise left unset and `ProxyResolve` substitutes
  //     `"proxy"`.
  let identity = state.identity.resolve(&parts.headers, host, &state.provider_registry);
  let resolved_provider_id = identity.provider_id.unwrap_or_else(|| host.to_string());

  let cfg = llm_requests::RunConfig::builder()
    .with_str(llm_requests::stages::resolve::proxy::keys::HOST, host)
    .with_str(llm_requests::stages::send::proxy::send_keys::PATH, &path_and_query)
    .with_str(llm_requests::stages::send::proxy::send_keys::METHOD, method.as_str())
    .with_str(
      llm_requests::stages::resolve::proxy::keys::PROVIDER_ID,
      &resolved_provider_id,
    )
    .with_str_opt(
      llm_requests::stages::resolve::proxy::keys::ACCOUNT_ID,
      identity.account_id.as_deref(),
    )
    .build();

  let raw = llm_requests::RawInbound {
    endpoint,
    headers: (&parts.headers).into(),
    raw_body,
    decoded_body,
    body_json,
    request_id: None,
  };

  match state.proxy_passthrough_pipeline.run_with(raw, cfg).await {
    Ok(converted) => Ok(crate::api::response::converted_to_axum(converted)),
    Err(err) => {
      tracing::warn!(%host, error = %err.message(), "proxy passthrough pipeline failed");
      Ok(ApiError::bad_gateway(err.message().into_owned()).into_response())
    }
  }
}

/// Best-effort guess at which [`Endpoint`] variant a given path
/// represents. Used only to populate [`llm_requests::RawInbound::endpoint`];
/// the proxy passthrough pipeline never branches on it.
fn infer_endpoint(path: &str) -> Endpoint {
  if path.ends_with("/responses") {
    Endpoint::Responses
  } else if path.ends_with("/messages") {
    Endpoint::Messages
  } else {
    Endpoint::ChatCompletions
  }
}
