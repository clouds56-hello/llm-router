pub mod codec;
pub mod endpoints;
pub mod error;
pub mod identity;
pub mod models;
pub mod response;

use crate::api::identity::AccountIdentityResolver;
use anyhow::Result;
use axum::http::{HeaderMap, HeaderName, Request, Response};
use axum::middleware::{self, Next};
use axum::routing::{get, post};
use axum::Router;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::Duration;
use tokn_accounts::registry::Registry as ProviderRegistry;
use tokn_accounts::routing::RouteResolver;
use tokn_accounts::AccountPool;
use tokn_config::Config;
use tokn_config::RouteMode;
use tokn_core::account::AccountConfig;
use tokn_core::event::EventBus;

const PIPELINE_RETRY_POLICY: tokn_requests::RetryPolicy =
  tokn_requests::RetryPolicy::new(2, Duration::from_millis(100));
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::trace::TraceLayer;
use tracing::{Level, Span};

#[derive(Clone)]
pub struct AppState {
  pub pool: Arc<AccountPool>,
  pub provider_registry: Arc<ProviderRegistry>,
  pub identity: Arc<AccountIdentityResolver>,
  pub route: Arc<RouteResolver>,
  pub http: reqwest::Client,
  pub events: Arc<EventBus>,
  pub body_max_bytes: usize,
  /// Shared `tokn-requests` pipeline used for router-owned JSON endpoints.
  pub request_pipeline: Arc<tokn_requests::Pipeline>,
  /// Shared `tokn-requests` pipeline used when the resolved route mode is
  /// [`RouteMode::Passthrough`]. Forwards the inbound request verbatim
  /// (no JSON parse, no cross-endpoint translation) while still
  /// emitting `RecordEvent::*` for observability and persistence.
  pub passthrough_pipeline: Arc<tokn_requests::Pipeline>,
  /// Shared `tokn-requests` pipeline used by the MITM proxy passthrough
  /// path. Unlike [`Self::passthrough_pipeline`], this variant does **no
  /// account resolution** — the intercepted TLS host is the upstream
  /// and the client's own `Authorization` reaches it unchanged. Wired
  /// via `RunConfig` keys (`proxy.host`, `proxy.path`, `proxy.method`,
  /// `proxy.provider_id`, `proxy.account_id`) that the proxy transport
  /// layer fills before calling `run_with`.
  pub proxy_passthrough_pipeline: Arc<tokn_requests::Pipeline>,
  /// Shared `tokn-requests` pipeline used by the MITM proxy `switch`
  /// path. This variant resolves the provider from the intercepted URL,
  /// selects a configured account for that provider, and forwards the
  /// request bytes verbatim with router-managed auth injection.
  pub proxy_switch_pipeline: Arc<tokn_requests::Pipeline>,
}

/// Header name used for request ids. Honors inbound `x-request-id` if present.
pub const REQUEST_ID_HEADER: &str = "x-request-id";
pub const SESSION_ID_HEADER: &str = "x-session-id";
pub const SESSION_ID_HEADERS: &[&str] = &[
  "x-session-id",
  "x-client-session-id",
  "session_id",
  "x-session-affinity",
  "x-opencode-session",
];
pub const REQUEST_ID_HEADERS: &[&str] = &["x-request-id", "x-interaction-id", "x-opencode-request"];
pub const PROJECT_ID_HEADERS: &[&str] = &["x-opencode-project"];

pub(crate) fn is_router_owned_header(name: &axum::http::HeaderName) -> bool {
  let name = name.as_str();
  name.starts_with("x-tokn-router-") || name == "x-route-mode" || name == "x-behave-as"
}

pub(crate) fn first_header<'a>(headers: &'a HeaderMap, names: &[&str]) -> Option<&'a str> {
  names.iter().find_map(|name| {
    headers
      .get(*name)
      .and_then(|v| v.to_str().ok())
      .map(str::trim)
      .filter(|s| !s.is_empty())
  })
}

tokio::task_local! {
  static REQUEST_TRACKING: Mutex<RequestTracking>;
}

#[derive(Default)]
struct RequestTracking {
  account: Option<Arc<str>>,
  upstream_url: Option<Arc<str>>,
}

#[allow(dead_code)]
pub(crate) fn record_upstream_url(url: &str) {
  let _ = REQUEST_TRACKING.try_with(|state| {
    state.lock().upstream_url = Some(Arc::from(url));
  });
}

fn tracking_snapshot() -> (String, String) {
  REQUEST_TRACKING
    .try_with(|state| {
      let g = state.lock();
      (
        g.account.as_deref().unwrap_or("-").to_string(),
        g.upstream_url.as_deref().unwrap_or("-").to_string(),
      )
    })
    .unwrap_or_else(|_| ("-".into(), "-".into()))
}

async fn track_request(req: Request<axum::body::Body>, next: Next) -> Response<axum::body::Body> {
  REQUEST_TRACKING
    .scope(Mutex::new(RequestTracking::default()), next.run(req))
    .await
}

/// Validates a route mode string from a path segment.
pub(crate) fn validate_path_mode(mode: &str) -> Result<(), ApiError> {
  match mode {
    "route" | "passthrough" | "switch" | "exact" | "fuzzy" => Ok(()),
    _ => Err(error::ApiError::bad_request(format!(
      "invalid route mode '{mode}' in path; expected route|passthrough|switch|exact|fuzzy"
    ))),
  }
}

use error::ApiError;

pub fn router(state: AppState) -> Router {
  let request_id_header = HeaderName::from_static(REQUEST_ID_HEADER);

  // TraceLayer is customised so the per-request span carries `request_id`
  // (set by SetRequestIdLayer below) and emits a single info-level summary
  // line at the response edge with status + latency. Per-step debug events
  // come from the handlers themselves and inherit this span.
  let trace = TraceLayer::new_for_http()
    .make_span_with(|req: &Request<_>| {
      let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
      tracing::info_span!(
        "http",
        method = %req.method(),
        uri = %req.uri(),
        request_id = %request_id,
        account = tracing::field::Empty,
        upstream_url = tracing::field::Empty,
        status = tracing::field::Empty,
        latency_ms = tracing::field::Empty,
      )
    })
    .on_request(|req: &Request<_>, _span: &Span| {
      let len = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-");
      tracing::debug!(content_length = %len, "request started");
    })
    .on_response(|resp: &Response<_>, latency: Duration, span: &Span| {
      let status = resp.status();
      let ms = latency.as_millis() as u64;
      let (account, upstream_url) = tracking_snapshot();
      span.record("status", status.as_u16());
      span.record("latency_ms", ms);
      span.record("account", account.as_str());
      span.record("upstream_url", upstream_url.as_str());
      if status.is_server_error() || status.is_client_error() {
        tracing::event!(Level::WARN, status = %status, latency_ms = ms, account = %account, upstream_url = %upstream_url, "request finished with error");
      } else {
        tracing::event!(Level::INFO, status = %status, latency_ms = ms, account = %account, upstream_url = %upstream_url, "request finished");
      }
    })
    .on_failure(
      |err: tower_http::classify::ServerErrorsFailureClass, latency: Duration, _span: &Span| {
        let (account, upstream_url) = tracking_snapshot();
        tracing::warn!(error = %err, latency_ms = latency.as_millis() as u64, account = %account, upstream_url = %upstream_url, "request failed");
      },
    );

  // Mode-prefixed routes: /{mode}/v1/...
  let mode_routes = Router::new()
    .route("/{mode}/v1/models", get(models::list_models_with_mode))
    .route(
      "/{mode}/v1/chat/completions",
      post(endpoints::chat_completions_with_mode),
    )
    .route("/{mode}/v1/responses", post(endpoints::responses_with_mode))
    .route("/{mode}/v1/messages", post(endpoints::messages_with_mode));

  Router::new()
    .route("/v1/models", get(models::list_models))
    .route("/v1/chat/completions", post(endpoints::chat_completions))
    .route("/v1/responses", post(endpoints::responses))
    .route("/v1/messages", post(endpoints::messages))
    .merge(mode_routes)
    .route("/healthz", get(health))
    .with_state(state)
    // Layers run outermost-first on request, innermost-first on response.
    // SetRequestIdLayer with MakeRequestUuid only assigns a fresh UUID when
    // the inbound request lacks the header, so client-supplied ids pass
    // through unchanged. PropagateRequestIdLayer copies it onto the
    // response so clients can correlate.
    .layer(PropagateRequestIdLayer::new(request_id_header.clone()))
    .layer(trace)
    .layer(SetRequestIdLayer::new(request_id_header, MakeRequestUuid))
    .layer(middleware::from_fn(track_request))
}

async fn health() -> &'static str {
  "ok"
}

pub fn build_state(cfg: &Config, accounts: &[AccountConfig], events: Arc<EventBus>) -> Result<AppState> {
  cfg.validate()?;
  let provider_registry = Arc::new(ProviderRegistry::builtin());
  let identity = Arc::new(AccountIdentityResolver::from_accounts(accounts));
  let pool = if accounts.is_empty() && matches!(cfg.server.route_mode, RouteMode::Passthrough) {
    AccountPool::empty(cfg)
  } else {
    let registry = provider_registry.clone();
    AccountPool::from_accounts_with(accounts, cfg, move |account| registry.build(account))?
  };
  let route = Arc::new(RouteResolver::new(cfg.server.route_mode, &cfg.model_families));
  let http = tokn_core::util::http::build_client(&cfg.proxy.to_http_options())?;
  let body_max_bytes = if cfg.db.enabled { cfg.db.body_max_bytes } else { 0 };
  let request_pipeline = build_request_pipeline(pool.clone(), route.clone(), http.clone(), events.clone());
  let passthrough_pipeline = build_passthrough_pipeline(pool.clone(), route.clone(), http.clone(), events.clone());
  let proxy_passthrough_pipeline = build_proxy_passthrough_pipeline(http.clone(), events.clone());
  let proxy_switch_pipeline = build_proxy_switch_pipeline(pool.clone(), http.clone(), events.clone());
  Ok(AppState {
    pool,
    provider_registry,
    identity,
    route,
    http,
    events,
    body_max_bytes,
    request_pipeline,
    passthrough_pipeline,
    proxy_passthrough_pipeline,
    proxy_switch_pipeline,
  })
}

/// Construct the default `tokn-requests` pipeline for router-owned JSON
/// endpoints. The pipeline shares `AppState.events` so persistence
/// (`RequestEventHandler`) receives `StageEvent::*` and `RecordEvent::*`
/// automatically.
fn build_request_pipeline(
  pool: Arc<AccountPool>,
  route: Arc<RouteResolver>,
  http: reqwest::Client,
  events: Arc<EventBus>,
) -> Arc<tokn_requests::Pipeline> {
  use tokn_requests::stages::{
    DefaultBuildHeaders, DefaultConvertRequest, DefaultConvertResponse, DefaultExtract, DefaultSend,
    PoolAccountSelector, PoolResolve,
  };
  let selector = Arc::new(PoolAccountSelector::new(pool, route));
  let profile = tokn_requests::Profile::full(
    "router-default",
    Arc::new(DefaultExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(DefaultBuildHeaders::with_provider_defaults()),
    Arc::new(DefaultConvertRequest),
    Arc::new(DefaultSend::new(http)),
    Arc::new(DefaultConvertResponse::new()),
  );
  Arc::new(tokn_requests::Pipeline::new_with_retry(
    Arc::new(profile),
    events,
    PIPELINE_RETRY_POLICY,
  ))
}

/// Construct the passthrough `tokn-requests` pipeline. Forwards the
/// inbound request body bytes verbatim with no JSON parsing or
/// cross-endpoint translation. Auth is still injected by the provider
/// during Send (via the upstream account handle), and observability
/// events still flow through `events` so persistence works.
///
/// Mirrors the behaviour of the legacy `crates/router/src/relay/passthrough.rs`
/// helpers but reuses the standard pipeline plumbing.
fn build_passthrough_pipeline(
  pool: Arc<AccountPool>,
  route: Arc<RouteResolver>,
  http: reqwest::Client,
  events: Arc<EventBus>,
) -> Arc<tokn_requests::Pipeline> {
  use tokn_requests::stages::{
    DefaultSend, PassthroughBuildHeaders, PassthroughConvertRequest, PassthroughConvertResponse, PassthroughExtract,
    PoolAccountSelector, PoolResolve,
  };
  let selector = Arc::new(PoolAccountSelector::new(pool, route));
  let profile = tokn_requests::Profile::full(
    "router-passthrough",
    Arc::new(PassthroughExtract),
    Arc::new(PoolResolve::new(selector)),
    Arc::new(PassthroughBuildHeaders::new()),
    Arc::new(PassthroughConvertRequest),
    Arc::new(DefaultSend::new(http)),
    Arc::new(PassthroughConvertResponse::new()),
  );
  Arc::new(tokn_requests::Pipeline::new_with_retry(
    Arc::new(profile),
    events,
    PIPELINE_RETRY_POLICY,
  ))
}

/// Construct the proxy-passthrough `tokn-requests` pipeline used by the
/// MITM proxy when the resolved route mode is
/// [`RouteMode::Passthrough`]. Unlike [`build_passthrough_pipeline`],
/// this variant performs **no account resolution** — the intercepted
/// TLS host is the upstream, the client's `Authorization` reaches it
/// untouched, and there is no provider-side auth injection.
///
/// The proxy transport layer supplies per-request hints
/// (`proxy.host`, `proxy.path`, `proxy.method`, …) through a
/// [`tokn_requests::RunConfig`] passed to `Pipeline::run_with`.
/// [`ProxyResolve`] and [`ProxySend`] read those keys; the remaining
/// stages are the same as the standard passthrough variant.
fn build_proxy_passthrough_pipeline(http: reqwest::Client, events: Arc<EventBus>) -> Arc<tokn_requests::Pipeline> {
  use tokn_requests::stages::{
    PassthroughBuildHeaders, PassthroughConvertRequest, PassthroughConvertResponse, PassthroughExtract, ProxyResolve,
    ProxySend,
  };
  let profile = tokn_requests::Profile::full(
    "router-proxy-passthrough",
    Arc::new(PassthroughExtract),
    Arc::new(ProxyResolve),
    Arc::new(PassthroughBuildHeaders::preserve_host()),
    Arc::new(PassthroughConvertRequest),
    Arc::new(ProxySend::new(http)),
    Arc::new(PassthroughConvertResponse::new()),
  );
  Arc::new(tokn_requests::Pipeline::new_with_retry(
    Arc::new(profile),
    events,
    PIPELINE_RETRY_POLICY,
  ))
}

fn build_proxy_switch_pipeline(
  pool: Arc<AccountPool>,
  http: reqwest::Client,
  events: Arc<EventBus>,
) -> Arc<tokn_requests::Pipeline> {
  use tokn_requests::stages::{
    PassthroughBuildHeaders, PassthroughConvertRequest, PassthroughConvertResponse, PassthroughExtract,
    ProxyProviderResolve, ProxySend,
  };
  let profile = tokn_requests::Profile::full(
    "router-proxy-switch",
    Arc::new(PassthroughExtract),
    Arc::new(ProxyProviderResolve::new(pool)),
    Arc::new(PassthroughBuildHeaders::preserve_host_with_router_auth()),
    Arc::new(PassthroughConvertRequest),
    Arc::new(ProxySend::new(http)),
    Arc::new(PassthroughConvertResponse::new()),
  );
  Arc::new(tokn_requests::Pipeline::new(Arc::new(profile), events))
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::config::{Account as AccountCfg, Config};
  use crate::util::secret::Secret;
  use axum::body::{to_bytes, Body};
  use axum::http::{Request, StatusCode};
  use axum::routing::get;
  use bytes::Bytes;
  use tower::ServiceExt;

  fn zai_account() -> AccountCfg {
    AccountCfg {
      id: "acct".into(),
      provider: "zai-coding-plan".into(),
      enabled: true,
      tier: tokn_core::account::AccountTier::Active,
      tags: Vec::new(),
      label: None,
      base_url: None,
      headers: Default::default(),
      auth_type: None,
      username: None,
      api_key: Some(Secret::new("sk-test".into())),
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      provider_account_id: None,
      extra: Default::default(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  /// Build the same layer stack the real router uses, around a stub handler.
  /// This isolates the request-id middleware from `AppState` construction.
  fn test_router() -> Router {
    let header = HeaderName::from_static(REQUEST_ID_HEADER);
    Router::new()
      .route("/probe", get(|| async { "ok" }))
      .layer(PropagateRequestIdLayer::new(header.clone()))
      .layer(SetRequestIdLayer::new(header, MakeRequestUuid))
  }

  #[tokio::test]
  async fn inbound_request_id_passes_through() {
    let app = test_router();
    let req = Request::builder()
      .uri("/probe")
      .header(REQUEST_ID_HEADER, "client-supplied-123")
      .body(Body::empty())
      .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let echoed = resp
      .headers()
      .get(REQUEST_ID_HEADER)
      .expect("response missing x-request-id")
      .to_str()
      .unwrap();
    assert_eq!(echoed, "client-supplied-123");
  }

  #[tokio::test]
  async fn missing_request_id_is_generated() {
    let app = test_router();
    let req = Request::builder().uri("/probe").body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let id = resp
      .headers()
      .get(REQUEST_ID_HEADER)
      .expect("response missing generated x-request-id")
      .to_str()
      .unwrap();
    // MakeRequestUuid emits a hyphenated uuid v4.
    assert!(uuid::Uuid::parse_str(id).is_ok(), "not a uuid: {id}");
  }

  #[test]
  fn first_header_uses_priority_and_ignores_empty_values() {
    let mut headers = HeaderMap::new();
    headers.insert("x-session-id", "   ".parse().unwrap());
    headers.insert("x-client-session-id", " client-session ".parse().unwrap());
    headers.insert("x-opencode-session", "opencode-session".parse().unwrap());

    assert_eq!(first_header(&headers, SESSION_ID_HEADERS), Some("client-session"));
  }

  #[test]
  fn build_state_allows_empty_accounts_in_passthrough_mode() {
    let mut cfg = Config::default();
    cfg.server.route_mode = RouteMode::Passthrough;
    let bus = EventBus::new(16);

    let state = build_state(&cfg, &[], Arc::new(bus)).expect("passthrough mode should allow empty accounts");
    assert_eq!(state.pool.len(), 0);
  }

  #[test]
  fn build_state_rejects_empty_accounts_in_non_passthrough_mode() {
    let mut cfg = Config::default();
    cfg.server.route_mode = RouteMode::Route;
    let bus = EventBus::new(16);

    let res = build_state(&cfg, &[], Arc::new(bus));
    assert!(res.is_err(), "non-passthrough mode should require accounts");
    let err = res.err().expect("checked above");
    assert!(err.to_string().contains("no accounts configured"));
  }

  #[tokio::test]
  async fn route_mode_not_implemented_returns_json_error_body() {
    let cfg = Config::default();
    let accounts = vec![zai_account()];
    let state = build_state(&cfg, &accounts, Arc::new(EventBus::noop())).unwrap();
    let app = router(state);

    let req = Request::builder()
      .method("POST")
      .uri("/v1/responses")
      .header("content-type", "application/json")
      .header("x-route-mode", "route")
      .body(Body::from(Bytes::from_static(br#"{"model":"unknown","input":"hi"}"#)))
      .unwrap();
    let resp = app.oneshot(req).await.unwrap();

    assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
    assert_eq!(
      resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok()),
      Some("application/json")
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let message = json["error"]["message"].as_str().unwrap();
    assert!(!message.is_empty());
    assert!(message.contains("responses"));
    assert!(message.contains("unknown"));
    assert_eq!(json["error"]["type"], "not_implemented_error");
    assert_eq!(json["error"]["code"], 501);
  }

  #[test]
  fn is_router_owned_header_does_not_include_request_session_project_id_headers() {
    use axum::http::HeaderName;

    for header in REQUEST_ID_HEADERS.iter() {
      let name = HeaderName::try_from(*header).unwrap();
      assert!(!is_router_owned_header(&name), "{header} should NOT be router-owned");
    }

    for header in SESSION_ID_HEADERS.iter() {
      let name = HeaderName::try_from(*header).unwrap();
      assert!(!is_router_owned_header(&name), "{header} should NOT be router-owned");
    }

    for header in PROJECT_ID_HEADERS.iter() {
      let name = HeaderName::try_from(*header).unwrap();
      assert!(!is_router_owned_header(&name), "{header} should NOT be router-owned");
    }
  }
}
