mod ca;

pub use ca::{load_or_generate_ca, ProxyCa};
use ca::DynamicResolver;
use crate::api::{
  codec::decode_json_request,
  error::ApiError,
  AppState,
};
use crate::pipeline::infer_stream_request;
use crate::relay::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response, ForwardContext};
use crate::routing::RouteResolver;
use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Method, Request, Response, Uri};
use axum::response::IntoResponse;
use axum::Router;
use bytes::Bytes;
use http::header::{HeaderValue, HOST};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use llm_config::RouteMode;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;

const CONNECT_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const BAD_CONNECT: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\ncontent-length: 0\r\n\r\n";
const DEFAULT_INTERCEPT_HOSTS: &[&str] = &[
  "api.openai.com",
  "api.anthropic.com",
  "api.githubcopilot.com",
  "api.z.ai",
  "open.bigmodel.cn",
  "openrouter.ai",
  "api.openai.com",
  "chatgpt.com",
  "ab.chatgpt.com",
];

#[derive(Clone, Debug)]
pub struct ProxyOptions {
  pub addr: SocketAddr,
  pub ca_dir: PathBuf,
  pub intercept_hosts: Vec<String>,
  pub passthrough_hosts: Vec<String>,
}

pub async fn serve(state: AppState, options: ProxyOptions) -> Result<()> {
  let listener = TcpListener::bind(options.addr)
    .await
    .with_context(|| format!("bind {}", options.addr))?;
  let ca = Arc::new(load_or_generate_ca(&options.ca_dir, false)?);
  let state = Arc::new(state);
  let route_resolver = state.route.clone();
  let http = state.http.clone();
  let router = proxy_router((*state).clone());
  let host_policy = HostPolicy::new(&options);

  tracing::info!(addr = %options.addr, ca_dir = %options.ca_dir.display(), "llm-router proxy listening");

  let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
  tokio::spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    let _ = shutdown_tx.send(());
  });

  loop {
    tokio::select! {
      _ = &mut shutdown_rx => break,
      accept = listener.accept() => {
        let (stream, peer) = accept?;
        let router = router.clone();
        let ca = ca.clone();
        let state = state.clone();
        let host_policy = host_policy.clone();
        let route_resolver = route_resolver.clone();
        let http = http.clone();
        tokio::spawn(async move {
          if let Err(err) = handle_client(stream, peer, state, router, ca, host_policy, route_resolver, http).await {
            tracing::warn!(%peer, error = %err, "proxy connection failed");
          }
        });
      }
    }
  }

  Ok(())
}

#[derive(Clone)]
struct HostPolicy {
  intercept: Arc<HashSet<String>>,
}

impl HostPolicy {
  fn new(options: &ProxyOptions) -> Self {
    let mut intercept = DEFAULT_INTERCEPT_HOSTS
      .iter()
      .map(|s| s.to_string())
      .collect::<HashSet<_>>();
    intercept.extend(options.intercept_hosts.iter().map(|s| s.to_ascii_lowercase()));
    for host in &options.passthrough_hosts {
      intercept.remove(&host.to_ascii_lowercase());
    }
    Self {
      intercept: Arc::new(intercept),
    }
  }

  fn should_intercept(&self, host: &str) -> bool {
    self.intercept.contains(&host.to_ascii_lowercase())
  }
}

/// Extract route mode from Proxy-Authorization Basic header username.
/// Format: `Proxy-Authorization: Basic <base64(username:password)>`
/// The username is parsed as a route mode; password is ignored.
fn extract_proxy_auth_mode(header_value: &str) -> Option<String> {
  let encoded = header_value
    .strip_prefix("Basic ")
    .or_else(|| header_value.strip_prefix("basic "))?;
  let decoded = String::from_utf8(base64_decode(encoded.trim())?).ok()?;
  let username = decoded.split(':').next().unwrap_or("");
  if username.is_empty() {
    return None;
  }
  // Validate it's a known mode
  match username {
    "route" | "passthrough" | "exact" | "fuzzy" => Some(username.to_string()),
    _ => None,
  }
}

fn base64_decode(input: &str) -> Option<Vec<u8>> {
  use base64::Engine;
  base64::engine::general_purpose::STANDARD.decode(input).ok()
}

async fn handle_client(
  stream: TcpStream,
  peer: SocketAddr,
  state: Arc<AppState>,
  router: Router,
  ca: Arc<ProxyCa>,
  host_policy: HostPolicy,
  route_resolver: Arc<RouteResolver>,
  http: reqwest::Client,
) -> Result<()> {
  let mut reader = BufReader::new(stream);
  let mut request_line = String::new();
  if reader.read_line(&mut request_line).await? == 0 {
    return Ok(());
  }
  let request_line = request_line.trim_end_matches(['\r', '\n']);
  let mut parts = request_line.split_whitespace();
  let method = parts.next().unwrap_or_default();
  let authority = parts.next().unwrap_or_default();
  let _version = parts.next().unwrap_or_default();

  // Parse CONNECT headers to extract Proxy-Authorization
  let mut proxy_route_mode: Option<String> = None;
  loop {
    let mut header_line = String::new();
    if reader.read_line(&mut header_line).await? == 0 {
      break;
    }
    if header_line == "\r\n" || header_line == "\n" {
      break;
    }
    // Check for Proxy-Authorization header
    if let Some(value) = header_line
      .strip_prefix("Proxy-Authorization:")
      .or_else(|| header_line.strip_prefix("proxy-authorization:"))
    {
      if let Some(mode) = extract_proxy_auth_mode(value.trim().trim_end_matches(['\r', '\n'])) {
        proxy_route_mode = Some(mode);
      }
    }
  }

  let mut stream = reader.into_inner();
  if method != Method::CONNECT.as_str() {
    stream.write_all(BAD_CONNECT).await?;
    return Ok(());
  }

  let (host, port) = split_authority(authority)?;
  let intercept = port == 443 && host_policy.should_intercept(&host);
  tracing::info!(%peer, host = %host, port, intercept, proxy_route_mode = ?proxy_route_mode, "proxy_connect");

  if intercept {
    stream.write_all(CONNECT_OK).await?;
    stream.flush().await?;
    intercept_tls(stream, &host, state, router, ca, route_resolver, http, proxy_route_mode).await
  } else {
    tunnel(stream, &host, port).await
  }
}

async fn tunnel(mut client: TcpStream, host: &str, port: u16) -> Result<()> {
  let mut upstream = TcpStream::connect((host, port))
    .await
    .with_context(|| format!("connect upstream {host}:{port}"))?;
  client.write_all(CONNECT_OK).await?;
  client.flush().await?;
  tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
  Ok(())
}

async fn intercept_tls(
  stream: TcpStream,
  host: &str,
  state: Arc<AppState>,
  router: Router,
  ca: Arc<ProxyCa>,
  route_resolver: Arc<RouteResolver>,
  http: reqwest::Client,
  proxy_route_mode: Option<String>,
) -> Result<()> {
  let resolver = Arc::new(DynamicResolver {
    ca,
    fallback_host: host.to_string(),
  });
  let tls = TlsAcceptor::from(Arc::new(
    rustls::ServerConfig::builder()
      .with_no_client_auth()
      .with_cert_resolver(resolver),
  ));
  let tls_stream = tls.accept(stream).await.context("TLS handshake failed")?;
  let mut http1_builder = http1::Builder::new();
  http1_builder.keep_alive(true).title_case_headers(true);

  let service = service_fn(move |req| {
    route_intercepted_request(
      state.clone(),
      router.clone(),
      route_resolver.clone(),
      http.clone(),
      req,
      proxy_route_mode.clone(),
    )
  });
  http1_builder
    .serve_connection(TokioIo::new(tls_stream), service)
    .await
    .context("serve intercepted HTTP connection")?;
  Ok(())
}

async fn route_intercepted_request(
  state: Arc<AppState>,
  router: Router,
  route_resolver: Arc<RouteResolver>,
  http: reqwest::Client,
  mut req: Request<hyper::body::Incoming>,
  proxy_route_mode: Option<String>,
) -> Result<Response<Body>, std::convert::Infallible> {
  // Inject proxy-auth-derived route mode as X-Route-Mode header if not already set
  if let Some(ref mode) = proxy_route_mode {
    if !req.headers().contains_key(RouteResolver::mode_header()) {
      if let Ok(val) = HeaderValue::from_str(mode) {
        req
          .headers_mut()
          .insert(http::header::HeaderName::from_static("x-route-mode"), val);
      }
    }
  }

  let host = req
    .headers()
    .get(HOST)
    .and_then(|v| v.to_str().ok())
    .map(|s| s.split(':').next().unwrap_or(s).to_string())
    .unwrap_or_default();
  let path = req.uri().path().to_string();
  let method = req.method().clone();

  let route_mode = req
    .headers()
    .get(RouteResolver::mode_header())
    .and_then(|v| v.to_str().ok());

  let resolved_mode = route_resolver.resolve_mode(route_mode);
  if matches!(resolved_mode, Ok(RouteMode::Passthrough)) {
    return Ok(
      proxy_passthrough(state.as_ref(), &http, &host, req)
        .await
        .unwrap_or_else(|err| ApiError::bad_gateway(err.to_string()).into_response()),
    );
  }
  if let Err(err) = resolved_mode {
    return Ok(ApiError::bad_request(err.to_string()).into_response());
  }

  let rewritten = if let Some(rewritten) = rewrite_target(&host, &path, &method) {
    rewritten
  } else {
    return Ok(ApiError::not_implemented(path, host).into_response());
  };

  let path_and_query = req.uri().path_and_query().map(|v| v.as_str()).unwrap_or(&path);
  let rewritten_path_and_query = path_and_query.replacen(&path, rewritten, 1);
  let uri = Uri::builder()
    .path_and_query(rewritten_path_and_query.as_str())
    .build()
    .unwrap_or_else(|_| Uri::from_static("/"));

  let (parts, body) = req.into_parts();
  let mut builder = Request::builder().method(method).uri(uri).version(parts.version);
  for (key, value) in &parts.headers {
    if key != HOST {
      builder = builder.header(key, value);
    }
  }
  builder = builder.header(
    HOST,
    HeaderValue::from_str(&host).unwrap_or_else(|_| HeaderValue::from_static("localhost")),
  );
  let body = Body::new(body);
  let request = builder.body(body).unwrap_or_else(|_| Request::new(Body::empty()));

  use tower::ServiceExt;
  let response = router
    .oneshot(request)
    .await
    .unwrap_or_else(|err| ApiError::bad_gateway(err.to_string()).into_response());
  Ok(response)
}

async fn proxy_passthrough(
  state: &AppState,
  http: &reqwest::Client,
  host: &str,
  req: Request<hyper::body::Incoming>,
) -> Result<Response<Body>> {
  let started = std::time::Instant::now();
  let path_and_query = req
    .uri()
    .path_and_query()
    .map(|v| v.as_str().to_string())
    .unwrap_or_else(|| "/".to_string());
  let url = format!("https://{host}{path_and_query}");
  let (parts, body) = req.into_parts();
  let request_body = axum::body::to_bytes(Body::new(body), usize::MAX)
    .await
    .context("read passthrough request body")?;
  let decoded_req = match decode_json_request(&parts.headers, request_body.clone()) {
    Ok(decoded) => decoded,
    Err(err) => return Ok(err.into_response()),
  };

  let mut upstream = http.request(parts.method.clone(), &url).body(request_body.clone());
  let mut outbound_req_headers = parts.headers.clone();
  for (name, value) in &parts.headers {
    if name != HOST {
      upstream = upstream.header(name, value);
    }
  }
  upstream = upstream.header(HOST, host);
  outbound_req_headers.insert(
    HOST,
    HeaderValue::from_str(host).unwrap_or_else(|_| HeaderValue::from_static("localhost")),
  );

  // Build ForwardContext from passthrough request data
  let path = path_and_query.split('?').next().unwrap_or(&path_and_query);
  let req_body_json = decoded_req.value.clone();
  let ctx = ForwardContext::from_passthrough(&parts.method, path, &parts.headers, &req_body_json, started);

  // Emit lifecycle events (caller owns lifecycle)
  let project_id = crate::api::first_header(&parts.headers, crate::api::PROJECT_ID_HEADERS).map(|s| s.to_string());
  let header_initiator = parts
    .headers
    .get("x-initiator")
    .and_then(|v| v.to_str().ok())
    .map(|v| v.trim().to_ascii_lowercase())
    .filter(|v| v == "user" || v == "agent");
  state.events.emit(llm_core::event::Event::RequestStarted {
    request_id: ctx.request_id.clone(),
    ts: std::time::SystemTime::now()
      .duration_since(std::time::UNIX_EPOCH)
      .unwrap_or_default()
      .as_secs() as i64,
    endpoint: ctx.endpoint.map(|e| e.as_str()).unwrap_or(path).to_string(),
    initiator: header_initiator.clone(),
    session_id: ctx.session_id.clone(),
    project_id: project_id.clone(),
    inbound_req: crate::db::HttpSnapshot {
      method: Some(parts.method.to_string()),
      url: Some(url.clone()),
      status: None,
      headers: parts.headers.clone(),
      body: request_body.clone(),
    },
  });
  let mut completion = crate::api::completion::CompletionGuard::new(state.events.clone(), ctx.request_id.clone(), started);
  let initiator = header_initiator
    .unwrap_or_else(|| crate::util::initiator::classify_initiator(&req_body_json).to_string());
  let stream = infer_stream_request(&parts.headers, &req_body_json);
  state.events.emit(llm_core::event::Event::RequestParsed {
    request_id: ctx.request_id.clone(),
    attempt: ctx.attempt,
    account_id: "passthrough".to_string(),
    provider_id: host.to_string(),
    model: ctx.model.clone(),
    stream,
    initiator,
    outbound_req: Some(crate::db::HttpSnapshot {
      method: Some(parts.method.to_string()),
      url: Some(url.clone()),
      status: None,
      headers: outbound_req_headers.clone(),
      body: Bytes::from(serde_json::to_vec(&req_body_json).unwrap_or_default()),
    }),
  });

  let response = match upstream.send().await {
    Ok(response) => response,
    Err(err) => {
      completion.failure(None, err.to_string());
      return Err(err).context("send passthrough upstream request");
    }
  };
  let status = response.status();
  state.events.emit(llm_core::event::Event::RequestResponded {
    request_id: ctx.request_id.clone(),
    attempt: ctx.attempt,
    status: status.as_u16(),
    latency_ms: started.elapsed().as_millis() as u64,
    resp_headers: response.headers().clone(),
  });

  if is_sse_response(response.headers(), stream) {
    // Background recorder emits RequestCompleted after stream ends.
    completion.disarm();
    let resp = passthrough_streaming_response(state.clone(), ctx, &req_body_json, response);
    return Ok(resp);
  }

  let resp = passthrough_buffered_response(state, &ctx, &req_body_json, response).await;
  completion.disarm();
  // RequestResult and RequestCompleted already emitted by passthrough_buffered_response
  Ok(resp)
}

pub(crate) fn rewrite_target(host: &str, path: &str, method: &Method) -> Option<&'static str> {
  match (host, method, path) {
    (_, &Method::GET, "/v1/models") => Some("/v1/models"),
    ("api.openai.com", &Method::POST, "/v1/chat/completions") => Some("/v1/chat/completions"),
    ("api.openai.com", &Method::POST, "/v1/responses") => Some("/v1/responses"),
    ("api.anthropic.com", &Method::POST, "/v1/messages") => Some("/v1/messages"),
    ("api.githubcopilot.com", &Method::POST, "/v1/chat/completions") => Some("/v1/chat/completions"),
    ("api.z.ai", &Method::POST, "/v1/chat/completions") => Some("/v1/chat/completions"),
    ("open.bigmodel.cn", &Method::POST, "/api/paas/v4/chat/completions") => Some("/v1/chat/completions"),
    ("openrouter.ai", &Method::POST, "/api/v1/chat/completions") => Some("/v1/chat/completions"),
    ("chatgpt.com", &Method::POST, "/backend-api/codex/responses") => Some("/v1/responses"),
    _ => None,
  }
}

fn proxy_router(state: AppState) -> Router {
  crate::api::router(state)
}

fn split_authority(authority: &str) -> Result<(String, u16)> {
  let (host, port) = authority
    .rsplit_once(':')
    .with_context(|| format!("invalid CONNECT authority '{authority}'"))?;
  Ok((
    host.to_ascii_lowercase(),
    port
      .parse()
      .with_context(|| format!("invalid CONNECT port in '{authority}'"))?,
  ))
}
