//! Send stage for the MITM proxy passthrough pipeline.
//!
//! Unlike [`DefaultSend`](super::DefaultSend), the proxy variant **does
//! not delegate to `Provider::chat / responses / messages`**. The
//! upstream URL is `https://{proxy.host}{proxy.path}` (taken straight
//! from [`PipelineCtx::config`]), the HTTP method is the inbound method
//! (also in the bag), and the headers are the pre-pruned [`BuiltHeaders`]
//! from [`PassthroughBuildHeaders`](super::super::build_headers::PassthroughBuildHeaders)
//! — including the client's own `Authorization`, which we preserve
//! verbatim. No auth injection, no URL rewriting, no provider hooks.
//!
//! The body sent on the wire is `ConvertedRequest::upstream_wire_body`,
//! which for the passthrough variant is the inbound raw bytes
//! ([`PassthroughConvertRequest`](super::super::convert_request::PassthroughConvertRequest)
//! returns them verbatim).
//!
//! Failure classification mirrors [`DefaultSend`]: 5xx → recoverable,
//! 4xx → permanent, transport errors → recoverable.

use crate::event::Stage;
use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::{PipelineError, ProviderError, RequestsError};
use crate::pipeline::stages::{BuiltHeaders, ConvertedRequest, Extracted, Resolved, SendStage, SentResponse};
use async_trait::async_trait;
use bytes::Bytes;
use smol_str::SmolStr;
use tokn_core::provider::HeaderPatchCtx;
use tokn_headers::HeaderMap;
use tracing::{debug, instrument, warn};

use crate::stages::resolve::proxy::keys;

/// Config keys consumed by [`ProxySend`]. These complement the keys
/// read by [`ProxyResolve`](crate::stages::ProxyResolve) and must be
/// populated by the proxy transport layer before calling
/// `pipeline.run_with`.
pub mod send_keys {
  /// HTTP method as an upper-case string, e.g. `"POST"`. When absent,
  /// `POST` is used as the default (matches the common LLM API case).
  pub const METHOD: &str = "proxy.method";
  /// Request path + query, e.g. `"/v1/chat/completions?foo=bar"`. Must
  /// start with `/`. Defaults to `/` when absent.
  pub const PATH: &str = "proxy.path";
  /// URL scheme, either `"https"` (production / MITM-intercepted TLS) or
  /// `"http"` (test fixtures pointing at plain HTTP mock servers).
  /// Defaults to `"https"` when absent.
  pub const SCHEME: &str = "proxy.scheme";
  /// When true, the selected provider patches auth onto the outbound
  /// request before proxy dispatch.
  pub const INJECT_AUTH: &str = "proxy.inject_auth";
}

pub struct ProxySend {
  http: reqwest::Client,
}

impl ProxySend {
  pub fn new(http: reqwest::Client) -> Self {
    Self { http }
  }
}

#[async_trait]
impl SendStage for ProxySend {
  #[instrument(name = "proxy_send", skip_all, fields(
    account = %resolved.account_id,
    provider = %resolved.provider_id,
    endpoint = ?resolved.upstream_endpoint,
    stream = extracted.stream,
  ))]
  async fn send(
    &self,
    ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
    headers: &BuiltHeaders,
    body: &ConvertedRequest,
  ) -> Result<SentResponse, PipelineError> {
    let host = ctx
      .config
      .get_str(keys::HOST)
      .ok_or_else(|| missing_config(keys::HOST))?;
    let path = ctx.config.get_str(send_keys::PATH).unwrap_or("/");
    let method_str = ctx.config.get_str(send_keys::METHOD).unwrap_or("POST");
    let method = reqwest::Method::from_bytes(method_str.as_bytes()).map_err(|e| {
      PipelineError::permanent(
        Stage::Send,
        RequestsError::Other {
          source: format!("invalid proxy method `{method_str}`: {e}").into(),
        },
      )
    })?;
    let scheme = ctx.config.get_str(send_keys::SCHEME).unwrap_or("https");
    let url = format!("{scheme}://{host}{path}");
    debug!(%url, %method, "proxy upstream dispatch");

    let inject_auth = ctx
      .config
      .get(send_keys::INJECT_AUTH)
      .and_then(|v| v.as_bool())
      .unwrap_or(false);
    let mut outbound_headers = headers.headers.clone();
    if inject_auth {
      resolved
        .account_handle
        .provider
        .patch_headers(
          &mut outbound_headers,
          &HeaderPatchCtx {
            endpoint: resolved.upstream_endpoint,
            body: body.upstream_body.as_ref(),
            bearer_token: None,
            content_encoding: body.content_encoding.map(|e| e.as_str()),
            stream: extracted.stream,
            initiator: extracted.initiator.as_str(),
            inbound_headers: &HeaderMap::new(),
            vars: &headers.vars,
          },
        )
        .map_err(|err| {
          PipelineError::permanent(
            Stage::Send,
            RequestsError::Provider {
              source: ProviderError::new(err),
            },
          )
        })?;
    }

    let mut req = self.http.request(method.clone(), &url);
    for (name, value) in outbound_headers.iter() {
      req = req.header(name.as_str(), value.as_str());
    }
    // Always set HOST to the intercepted host so virtual-hosted upstreams
    // (most LLM APIs are behind a CDN that vhosts by Host) route us to
    // the right backend. PassthroughBuildHeaders strips the inbound
    // HOST already.
    req = req.header(reqwest::header::HOST, host);
    req = req.body(body.upstream_wire_body.clone());

    // Emit the request-side record so the persistence handler can write
    // wire-accurate values into the row, mirroring DefaultSend.
    ctx.emit_record(tokn_core::request_event::RecordEvent::UpstreamReq {
      method: SmolStr::new(method.as_str()),
      url: SmolStr::new(&url),
      headers: outbound_headers.clone(),
      body: body.upstream_wire_body.clone(),
    });

    let resp = match req.send().await {
      Ok(r) => r,
      Err(err) => {
        let recoverable = err.is_connect() || err.is_timeout() || err.is_request();
        let source = RequestsError::Other {
          source: format!("proxy upstream `{url}` failed: {err}").into(),
        };
        return Err(if recoverable {
          PipelineError::recoverable(Stage::Send, source)
        } else {
          PipelineError::permanent(Stage::Send, source)
        });
      }
    };

    let status = resp.status().as_u16();
    let resp_headers = HeaderMap::from(resp.headers());
    debug!(%status, "proxy upstream responded");

    ctx.emit_record(tokn_core::request_event::RecordEvent::UpstreamResp {
      status,
      headers: resp_headers.clone(),
    });

    if status >= 500 {
      let body_text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
          return Err(PipelineError::recoverable(
            Stage::Send,
            RequestsError::UpstreamReadFailed { status, source: e },
          ));
        }
      };
      ctx.emit_record(tokn_core::request_event::RecordEvent::UpstreamBody {
        body: Bytes::copy_from_slice(body_text.as_bytes()),
        error: None,
      });
      return Err(PipelineError::recoverable(
        Stage::Send,
        RequestsError::UpstreamStatus {
          status,
          body: truncate(&body_text, 512),
        },
      ));
    }
    if status >= 400 {
      // For 4xx we propagate the response upstream so the client sees
      // the upstream's error envelope verbatim. The proxy contract is
      // "transparent forwarding" — wrapping a 4xx in our own envelope
      // would change semantics. Note: this differs from DefaultSend,
      // which converts 4xx to PipelineError::permanent. The legacy
      // proxy passthrough also forwarded 4xx bodies verbatim, and the
      // later ConvertResponse path will persist the drained body.
      warn!(%status, "proxy upstream 4xx — forwarding verbatim");
    }

    Ok(SentResponse {
      status,
      headers: resp_headers,
      stream: extracted.stream,
      upstream_endpoint: resolved.upstream_endpoint,
      response: resp,
    })
  }
}

fn missing_config(key: &str) -> PipelineError {
  PipelineError::permanent(
    Stage::Send,
    RequestsError::Other {
      source: format!("proxy passthrough pipeline requires `{key}` in RunConfig").into(),
    },
  )
}

fn truncate(s: &str, max: usize) -> String {
  if s.len() <= max {
    s.to_string()
  } else {
    format!("{}…", &s[..max])
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::event::EventBus;
  use crate::pipeline::config::RunConfig;
  use crate::pipeline::stages::ResolveStage;
  use crate::pipeline::stages::{BuiltHeaders, ConvertedRequest, Extracted, Resolved};
  use crate::stages::resolve::proxy::ProxyResolve;
  use crate::test_support::{mock_handle_with_provider, MockProvider};
  use bytes::Bytes;
  use serde_json::Value;
  use std::sync::Arc;
  use tokio::io::{AsyncReadExt, AsyncWriteExt};
  use tokn_core::provider::Endpoint;
  use tokn_headers::{HeaderName, HeaderValue};

  fn ctx_with(config: RunConfig) -> PipelineCtx {
    PipelineCtx::new_with_config(
      "req-px-send",
      Endpoint::ChatCompletions,
      Arc::new(EventBus::new(64)),
      Arc::new(config),
    )
  }

  fn fake_extracted() -> Extracted {
    Extracted {
      agent_id: None,
      model: SmolStr::new("gpt-4"),
      stream: false,
      session_id: None,
      project_id: None,
      initiator: SmolStr::new("user"),
      header_initiator: None,
      route_mode_hint: None,
      headers: HeaderMap::new(),
      raw_body: Bytes::new(),
      decoded_body: Bytes::new(),
      body_json: Arc::new(Value::Null),
      content_encoding: None,
    }
  }

  async fn fake_resolved(ctx: &PipelineCtx) -> Resolved {
    ProxyResolve.resolve(ctx, &fake_extracted()).await.unwrap()
  }

  fn fake_body() -> ConvertedRequest {
    let bytes = Bytes::from_static(b"hello world");
    ConvertedRequest {
      upstream_body: Arc::new(Value::Null),
      upstream_wire_body: bytes.clone(),
      debug_outbound_body: bytes,
      content_encoding: None,
    }
  }

  fn fake_headers() -> BuiltHeaders {
    let mut h = HeaderMap::new();
    h.insert(
      HeaderName::new("authorization"),
      HeaderValue::from_static("Bearer client-token"),
    );
    h.insert(HeaderName::new("user-agent"), HeaderValue::from_static("test"));
    BuiltHeaders {
      headers: h,
      vars: Default::default(),
    }
  }

  #[tokio::test]
  async fn missing_host_is_permanent_error() {
    let ctx = ctx_with(RunConfig::default());
    let resolved = Resolved {
      agent_id: None,
      model: SmolStr::new("m"),
      upstream_model: SmolStr::new("m"),
      upstream_endpoint: Endpoint::ChatCompletions,
      account_id: SmolStr::new("proxy"),
      provider_id: SmolStr::new("none"),
      account_handle: crate::stages::resolve::proxy::stub_handle("proxy", "none"),
    };
    let send = ProxySend::new(reqwest::Client::new());
    let err = send
      .send(&ctx, &fake_extracted(), &resolved, &fake_headers(), &fake_body())
      .await
      .unwrap_err();
    assert_eq!(err.stage, Stage::Send);
    assert!(err.message().contains("proxy.host"));
  }

  #[tokio::test]
  async fn invalid_method_is_permanent_error() {
    let cfg = RunConfig::builder()
      .with_str(keys::HOST, "127.0.0.1:1")
      .with_str(send_keys::METHOD, "TOTALLY BAD METHOD")
      .build();
    let ctx = ctx_with(cfg);
    let resolved = fake_resolved(&ctx).await;
    let send = ProxySend::new(reqwest::Client::new());
    let err = send
      .send(&ctx, &fake_extracted(), &resolved, &fake_headers(), &fake_body())
      .await
      .unwrap_err();
    assert_eq!(err.stage, Stage::Send);
    assert!(err.message().contains("invalid proxy method"));
  }

  #[tokio::test]
  async fn injects_router_managed_auth_when_enabled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
    let server = tokio::spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = vec![0_u8; 8192];
      let n = stream.read(&mut buf).await.unwrap();
      buf.truncate(n);
      stream
        .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
        .await
        .unwrap();
      stream.flush().await.unwrap();
      let _ = tx.send(buf);
    });

    let ctx = ctx_with(
      RunConfig::builder()
        .with_str(keys::HOST, addr.to_string())
        .with_str(send_keys::PATH, "/v1/chat/completions")
        .with_str(send_keys::METHOD, "POST")
        .with_str(send_keys::SCHEME, "http")
        .with(send_keys::INJECT_AUTH, true)
        .build(),
    );
    let resolved = Resolved {
      agent_id: None,
      model: SmolStr::new("gpt-4"),
      upstream_model: SmolStr::new("gpt-4"),
      upstream_endpoint: Endpoint::ChatCompletions,
      account_id: SmolStr::new("acct"),
      provider_id: SmolStr::new("mock"),
      account_handle: mock_handle_with_provider(
        "acct",
        MockProvider::new("mock").with_header("authorization", "Bearer router-token"),
      ),
    };

    let send = ProxySend::new(reqwest::Client::new());
    let sent = send
      .send(&ctx, &fake_extracted(), &resolved, &fake_headers(), &fake_body())
      .await
      .unwrap();
    assert_eq!(sent.status, 200);

    server.await.unwrap();
    let raw_req = String::from_utf8_lossy(&rx.await.unwrap()).to_ascii_lowercase();
    assert!(raw_req.contains("authorization: bearer router-token"));
    assert!(!raw_req.contains("authorization: bearer client-token"));
  }
}
