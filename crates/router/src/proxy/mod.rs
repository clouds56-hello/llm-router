mod ca;
pub mod header_pipeline;
mod passthrough;
mod transport;

use crate::api::AppState;
use anyhow::{Context, Result};
use axum::http::Method;
use axum::Router;
pub use ca::{load_or_generate_ca, ProxyCa};
use std::collections::HashSet;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::net::TcpListener;
use transport::handle_client;
const DEFAULT_INTERCEPT_HOSTS: &[&str] = &[
  "api.openai.com",
  "api.anthropic.com",
  "api.githubcopilot.com",
  "api.z.ai",
  "open.bigmodel.cn",
  "openrouter.ai",
  "api.openai.com",
  "chatgpt.com",
  // "ab.chatgpt.com",
  "opencode.ai",
];

#[derive(Clone, Debug)]
pub struct ProxyOptions {
  pub addr: SocketAddr,
  pub ca_dir: PathBuf,
  pub intercept_hosts: Vec<String>,
  pub passthrough_hosts: Vec<String>,
}

pub async fn serve<F>(state: AppState, options: ProxyOptions, shutdown: F) -> Result<()>
where
  F: Future<Output = ()> + Send,
{
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

  tokio::pin!(shutdown);

  loop {
    tokio::select! {
      _ = &mut shutdown => break,
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
pub(super) struct HostPolicy {
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

  pub(super) fn should_intercept(&self, host: &str) -> bool {
    self.intercept.contains(&host.to_ascii_lowercase())
  }
}

/// Extract route mode from Proxy-Authorization Basic header username.
/// Format: `Proxy-Authorization: Basic <base64(username:password)>`
/// The username is parsed as a route mode; password is ignored.
pub(super) fn extract_proxy_auth_mode(header_value: &str) -> Option<String> {
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
