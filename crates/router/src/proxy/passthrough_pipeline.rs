//! Proxy MITM passthrough dispatch via the shared `llm-requests`
//! [`Pipeline`].
//!
//! This is the pipeline-based replacement for [`super::passthrough::proxy_passthrough`].
//! It builds a [`llm_requests::RawInbound`] from the intercepted request,
//! supplies a [`llm_requests::RunConfig`] populated with the resolved
//! authority / method / path / scheme under the `proxy.*` keys, and
//! invokes [`AppState::proxy_passthrough_pipeline`] via
//! [`llm_requests::Pipeline::run_with`].
//!
//! Host/port resolution lives in this module — see
//! [`resolve_host_with_port`]. The resolved value is used as the
//! upstream authority (URL host) **and** as the outbound `Host` header
//! (preserved verbatim by
//! [`PassthroughBuildHeaders::preserve_host`](llm_requests::stages::PassthroughBuildHeaders::preserve_host)).
//!
//! The pipeline itself ([`ProxyResolve`] + [`ProxySend`]) reads those
//! keys, dispatches the request to `{scheme}://{host}{path}` preserving
//! the client's own `Authorization`, and emits the standard
//! `RecordEvent::*` observability stream — no legacy `LegacyRequest`
//! events are produced here.
//!
//! [`ProxyResolve`]: llm_requests::stages::ProxyResolve
//! [`ProxySend`]: llm_requests::stages::ProxySend
//! [`AppState::proxy_passthrough_pipeline`]: crate::api::AppState::proxy_passthrough_pipeline

use crate::api::error::ApiError;
use crate::api::AppState;
use crate::pipeline::request_header_extract;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::request::Parts;
use axum::http::{HeaderValue, Request, Response};
use axum::response::IntoResponse;
use bytes::Bytes;
use http::header::HOST;
use llm_core::event::Event as CoreEvent;
use llm_core::provider::Endpoint;
use llm_core::request_event::{RecordEvent, RequestEvent, RequestEventPayload};
use smol_str::SmolStr;

/// Dispatch an intercepted MITM request through the proxy-passthrough
/// pipeline.
///
/// * `intercepted_host` — bare host (no port) from the CONNECT
///   authority. Used as the **fallback** authority and as the bare-host
///   fallback for `provider_id` when identity resolution returns
///   `None`.
/// * `intercepted_port` — port from the CONNECT authority. Used as the
///   **fallback** port when neither `req.uri()` nor the `Host` header
///   carry one.
/// * `scheme` — `"http"` or `"https"`. Production always passes
///   `"https"` (the MITM only runs for port 443); tests may pass
///   `"http"`.
/// * `peer_addr` / `local_addr` — inbound TCP connection endpoints,
///   forwarded into `RecordEvent::InboundConnection` for persistence.
pub(super) async fn proxy_passthrough_via_pipeline(
  state: &AppState,
  intercepted_host: &str,
  intercepted_port: u16,
  scheme: &str,
  peer_addr: Option<String>,
  local_addr: Option<String>,
  req: Request<hyper::body::Incoming>,
) -> Result<Response<Body>> {
  let (parts, body) = req.into_parts();
  let raw_body = axum::body::to_bytes(Body::new(body), usize::MAX)
    .await
    .context("read proxy passthrough request body")?;
  Ok(
    proxy_passthrough_via_pipeline_inner(
      state,
      intercepted_host,
      intercepted_port,
      scheme,
      peer_addr,
      local_addr,
      parts,
      raw_body,
    )
    .await,
  )
}

/// Inner core that does identity resolution, `RunConfig` construction,
/// pipeline invocation, and response conversion. Split from the public
/// wrapper so integration tests (which can't construct a real
/// `hyper::body::Incoming`) can drive the pipeline with pre-read body
/// bytes and a custom `scheme` (e.g. `"http"` to point at a plain mock
/// upstream).
#[allow(clippy::too_many_arguments)]
pub async fn proxy_passthrough_via_pipeline_inner(
  state: &AppState,
  intercepted_host: &str,
  intercepted_port: u16,
  scheme: &str,
  peer_addr: Option<String>,
  local_addr: Option<String>,
  mut parts: Parts,
  raw_body: Bytes,
) -> Response<Body> {
  let path_and_query = parts
    .uri
    .path_and_query()
    .map(|v| v.as_str().to_string())
    .unwrap_or_else(|| "/".to_string());
  let path_only = parts.uri.path();
  let method = parts.method.clone();

  // Resolve the authoritative host[:port] using the precedence:
  //   req.uri authority → Host header → intercepted host:port.
  // The result has default ports (`:443` for https, `:80` for http)
  // normalized out.
  let host_with_port = resolve_host_with_port(&parts, intercepted_host, intercepted_port, scheme);

  // Rewrite the inbound `Host` header to the resolved authority so
  // `PassthroughBuildHeaders::preserve_host()` forwards the correct
  // value to the upstream.
  if let Ok(hv) = HeaderValue::from_str(&host_with_port) {
    parts.headers.insert(HOST, hv);
  }

  // The pipeline never inspects this for proxy passthrough (no body
  // parse, no route lookup), but `RawInbound` requires a value.
  // Pick the closest match from the path so downstream introspection
  // (e.g. logs) is sensible.
  let endpoint = Endpoint::infer_from(path_only).unwrap_or(Endpoint::ChatCompletions);

  // Pre-decoded body matches raw body — `PassthroughConvertRequest`
  // forwards bytes verbatim and never reads `decoded_body`. Same for
  // `body_json` (kept as `Null`).
  let decoded_body = raw_body.clone();
  let body_json = serde_json::Value::Null;

  // Reconstruct the full URL the client targeted post-CONNECT.
  // Identity resolution gets it (path helps providers like
  // `matches_url` disambiguate shared-host scenarios — see
  // `Registry::provider_id_for_url`); `RecordEvent::InboundConnection`
  // persists it for the `requests.inbound_req_url` column.
  let full_url = format!("{scheme}://{host_with_port}{path_and_query}");

  // Identity resolution — fingerprint the inbound bearer against
  // locally-known accounts so DB rows / events attribute the
  // intercepted request to a concrete `account_id` + `provider_id`.
  // Pass the full URL so descriptors with path-based `matches_url`
  // discriminate correctly. Registry strips the port internally.
  let identity = state
    .identity
    .resolve(&parts.headers, &full_url, &state.provider_registry);
  // Fallback to the bare intercepted host (not host:port and not the
  // full URL) so the synthetic provider_id stays stable across
  // requests to different paths/ports on the same upstream.
  let resolved_provider_id = identity.provider_id.unwrap_or_else(|| intercepted_host.to_string());

  // Emit InboundConnection so persistence populates `local_addr`,
  // `peer_addr`, `mode`, `method`, `inbound_req_method`, and
  // `inbound_req_url` for the request row. Uses the same `request_id`
  // the pipeline will derive from `parts.headers` (via
  // `request_header_extract`) so the persistence UPDATE hits the same
  // row the pipeline's `StageEvent::Started` INSERT creates.
  let hx = request_header_extract(&parts.headers);
  state.events.emit(CoreEvent::Requests(RequestEvent {
    request_id: SmolStr::new(&hx.request_id),
    attempt: 0,
    ts: llm_core::util::now_unix_ms(),
    payload: RequestEventPayload::Record(RecordEvent::InboundConnection {
      local_addr: local_addr.map(SmolStr::from),
      peer_addr: peer_addr.map(SmolStr::from),
      mode: SmolStr::new("passthrough"),
      method: SmolStr::new("proxy"),
      inbound_method: SmolStr::new(method.as_str()),
      url: Some(SmolStr::from(full_url.as_str())),
    }),
  }));

  let mut cfg_builder = llm_requests::RunConfig::builder()
    .with_str(llm_requests::stages::resolve::proxy::keys::HOST, &host_with_port)
    .with_str(llm_requests::stages::send::proxy::send_keys::PATH, &path_and_query)
    .with_str(llm_requests::stages::send::proxy::send_keys::METHOD, method.as_str())
    .with_str(llm_requests::stages::send::proxy::send_keys::SCHEME, scheme)
    .with_str(
      llm_requests::stages::resolve::proxy::keys::PROVIDER_ID,
      &resolved_provider_id,
    );
  if let Some(account_id) = identity.account_id.as_deref() {
    cfg_builder = cfg_builder.with_str(llm_requests::stages::resolve::proxy::keys::ACCOUNT_ID, account_id);
  }
  let cfg = cfg_builder.build();

  let raw = llm_requests::RawInbound {
    endpoint,
    headers: (&parts.headers).into(),
    raw_body,
    decoded_body,
    body_json,
    request_id: Some(SmolStr::new(&hx.request_id)),
  };

  match state.proxy_passthrough_pipeline.run_with(raw, cfg).await {
    Ok(converted) => crate::api::response::converted_to_axum(converted),
    Err(err) => {
      tracing::warn!(host = %host_with_port, error = %err.message(), "proxy passthrough pipeline failed");
      ApiError::bad_gateway(err.message().into_owned()).into_response()
    }
  }
}

/// Resolve the authoritative `host[:port]` for the upstream URL with
/// the precedence:
///
/// 1. `parts.uri().authority()` (set when the request line was
///    absolute-form — `req.uri()` port wins over the `Host` header
///    per the original CONNECT proxy contract).
/// 2. The inbound `Host` header.
/// 3. The intercepted CONNECT authority (`intercepted_host:intercepted_port`).
///
/// The result has the default port stripped when it matches the scheme
/// (`:443` for `https`, `:80` for `http`).
fn resolve_host_with_port(parts: &Parts, intercepted_host: &str, intercepted_port: u16, scheme: &str) -> String {
  let (host, port) = if let Some(auth) = parts.uri.authority() {
    (auth.host().to_string(), auth.port_u16())
  } else if let Some((h, p)) = parts
    .headers
    .get(HOST)
    .and_then(|v| v.to_str().ok())
    .map(split_host_port)
  {
    (h, p)
  } else {
    (intercepted_host.to_string(), Some(intercepted_port))
  };
  normalize_authority(&host, port, scheme)
}

/// Split `host` or `host:port` into `(host, Option<port>)`. Invalid
/// port digits yield `None` — falls through to the intercepted port
/// (the only other source). IPv6 literals (`[::1]:443`) are handled
/// by splitting on the last `:` outside the brackets.
fn split_host_port(value: &str) -> (String, Option<u16>) {
  let trimmed = value.trim();
  // IPv6 literal in brackets.
  if let Some(rest) = trimmed.strip_prefix('[') {
    if let Some(end) = rest.find(']') {
      let host = format!("[{}]", &rest[..end]);
      let after = &rest[end + 1..];
      let port = after.strip_prefix(':').and_then(|p| p.parse().ok());
      return (host, port);
    }
  }
  match trimmed.rsplit_once(':') {
    Some((h, p)) if !h.is_empty() && p.chars().all(|c| c.is_ascii_digit()) => (h.to_string(), p.parse().ok()),
    _ => (trimmed.to_string(), None),
  }
}

/// Format `host` + optional `port` into a canonical authority,
/// dropping the port when it equals the scheme's default.
fn normalize_authority(host: &str, port: Option<u16>, scheme: &str) -> String {
  let default = match scheme {
    "https" => Some(443),
    "http" => Some(80),
    _ => None,
  };
  match port {
    Some(p) if Some(p) != default => format!("{host}:{p}"),
    _ => host.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::http::{HeaderMap, Method, Uri, Version};

  fn parts_with(uri: &str, host_header: Option<&str>) -> Parts {
    let req = Request::builder()
      .method(Method::POST)
      .uri(Uri::try_from(uri).unwrap())
      .version(Version::HTTP_11)
      .body(())
      .unwrap();
    let (mut parts, _) = req.into_parts();
    if let Some(h) = host_header {
      parts.headers.insert(HOST, HeaderValue::from_str(h).unwrap());
    } else {
      // Builder may auto-add Host from the URI authority; clear it so
      // the test exercises the no-Host-header branch deterministically.
      parts.headers.remove(HOST);
    }
    parts
  }

  #[test]
  fn uri_authority_wins_over_host_header() {
    let p = parts_with("http://api.example.com:8443/v1/x", Some("other.com"));
    let _ = HeaderMap::new();
    assert_eq!(
      resolve_host_with_port(&p, "intercepted.example", 443, "https"),
      "api.example.com:8443"
    );
  }

  #[test]
  fn host_header_default_port_stripped_https() {
    let p = parts_with("/v1/x", Some("api.example.com:443"));
    assert_eq!(
      resolve_host_with_port(&p, "intercepted", 443, "https"),
      "api.example.com"
    );
  }

  #[test]
  fn host_header_nondefault_port_kept_http() {
    let p = parts_with("/v1/x", Some("api.example.com:8080"));
    assert_eq!(
      resolve_host_with_port(&p, "intercepted", 80, "http"),
      "api.example.com:8080"
    );
  }

  #[test]
  fn intercepted_default_port_stripped() {
    let p = parts_with("/v1/x", None);
    assert_eq!(
      resolve_host_with_port(&p, "api.example.com", 443, "https"),
      "api.example.com"
    );
  }

  #[test]
  fn intercepted_nondefault_port_kept() {
    let p = parts_with("/v1/x", None);
    assert_eq!(
      resolve_host_with_port(&p, "api.example.com", 8443, "https"),
      "api.example.com:8443"
    );
  }

  #[test]
  fn ipv6_host_header_with_port() {
    let p = parts_with("/v1/x", Some("[::1]:8443"));
    assert_eq!(resolve_host_with_port(&p, "intercepted", 443, "https"), "[::1]:8443");
  }

  #[test]
  fn ipv6_host_header_default_port_stripped() {
    let p = parts_with("/v1/x", Some("[::1]:443"));
    assert_eq!(resolve_host_with_port(&p, "intercepted", 443, "https"), "[::1]");
  }
}
