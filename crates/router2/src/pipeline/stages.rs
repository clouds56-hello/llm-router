//! Stage traits + the data structs that flow between them.
//!
//! Each stage is an `async_trait`-object so [`Profile`] can store them behind
//! `Arc<dyn ...>`. Stages take `&PipelineCtx` (not `&mut`) — the runner owns
//! the ctx and is the sole authority on the per-request state shape. Stages
//! may publish custom events through `ctx.emit_custom`.
//!
//! Each stage returns `Result<Output, PipelineError>`. The runner emits the
//! corresponding success [`StageEvent`] on `Ok` and a tagged
//! [`StageEvent::Error`] on `Err`.
//!
//! [`Profile`]: crate::profile::Profile
//! [`StageEvent`]: crate::event::StageEvent
//! [`StageEvent::Error`]: crate::event::StageEvent::Error

use crate::pipeline::ctx::PipelineCtx;
use crate::pipeline::error::PipelineError;
use crate::utils::codec::ContentEncodingKind;
use async_trait::async_trait;
use bytes::Bytes;
use llm_accounts::AccountHandle;
use llm_core::provider::Endpoint;
use llm_core::ClientId;
use llm_headers::{HeaderMap, TemplateVars};
use serde_json::Value;
use smol_str::SmolStr;
use std::sync::Arc;

/// Raw inbound HTTP payload passed to the Extract stage. The runner is
/// responsible for assembling this from whatever transport is in front of
/// router2 (axum in production, fixtures in tests).
#[derive(Debug, Clone)]
pub struct RawInbound {
  pub endpoint: Endpoint,
  pub headers: HeaderMap,
  /// Original wire body (still compressed if it arrived compressed). PR1
  /// does not require decompression; the production path will populate
  /// `decoded_body` separately when wiring real transport.
  pub raw_body: Bytes,
  /// Post-decompression body bytes — equal to `raw_body` when the inbound
  /// payload was uncompressed.
  pub decoded_body: Bytes,
  pub body_json: Value,
  /// Optional request id supplied by the transport. When `None`, the runner
  /// generates one before constructing [`PipelineCtx`].
  pub request_id: Option<SmolStr>,
}

/// Output of [`ExtractStage`]: everything subsequent stages need to know
/// about the inbound request, in normalized form.
///
/// `Extracted` deliberately does **not** carry the inbound endpoint — that
/// lives on [`PipelineCtx`] (`ctx.endpoint`) because the runner has it from
/// the start (out of `RawInbound`) and every stage gets `&PipelineCtx`. Keeping
/// it on the ctx avoids duplication and ensures a single source of truth.
#[derive(Debug, Clone)]
pub struct Extracted {
  pub client_id: Option<ClientId>,
  pub model: SmolStr,
  pub stream: bool,
  pub session_id: Option<SmolStr>,
  pub project_id: Option<SmolStr>,
  pub initiator: SmolStr,
  pub header_initiator: Option<SmolStr>,
  pub route_mode_hint: Option<SmolStr>,
  pub headers: HeaderMap,
  pub raw_body: Bytes,
  pub decoded_body: Bytes,
  pub body_json: Arc<Value>,
  /// Content-encoding the client used on the request body, parsed from
  /// the inbound `Content-Encoding` header. `None` when the body arrived
  /// uncompressed. ConvertRequest uses this to re-encode the outbound
  /// payload with the same codec when possible.
  pub content_encoding: Option<ContentEncodingKind>,
}

/// Output of [`ResolveStage`]: which account+upstream answers this request.
#[derive(Clone)]
pub struct Resolved {
  pub client_id: Option<ClientId>,
  pub model: SmolStr,
  pub upstream_model: SmolStr,
  pub upstream_endpoint: Endpoint,
  pub account_id: SmolStr,
  pub provider_id: SmolStr,
  /// Typed handle to the selected account. Holding the [`AccountHandle`]
  /// directly (instead of an `Arc<dyn Any>`) lets downstream stages call
  /// `provider.input_transformer()` etc. without a downcast.
  pub account_handle: std::sync::Arc<AccountHandle>,
}

impl std::fmt::Debug for Resolved {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("Resolved")
      .field("client_id", &self.client_id)
      .field("model", &self.model)
      .field("upstream_model", &self.upstream_model)
      .field("upstream_endpoint", &self.upstream_endpoint)
      .field("account_id", &self.account_id)
      .field("provider_id", &self.provider_id)
      .field("account_handle", &"<opaque>")
      .finish()
  }
}

/// Output of [`BuildHeadersStage`]: the composed outbound `HeaderMap` that
/// the Send stage will use as the upstream request's headers, plus the
/// [`TemplateVars`] derived from the inbound request (kept around so later
/// stages can splice values without re-parsing inbound headers).
#[derive(Debug, Clone, Default)]
pub struct BuiltHeaders {
  pub headers: HeaderMap,
  pub vars: TemplateVars,
}

/// Output of [`ConvertRequestStage`]: the upstream-shaped JSON body, the
/// (re-encoded) bytes we'll actually send on the wire, the
/// post-encoding-but-pre-compression bytes (handy for logging), and the
/// `Content-Encoding` value to put on the outbound request (when any).
#[derive(Debug, Clone)]
pub struct ConvertedRequest {
  /// Upstream-shaped JSON body. Wrapped in `Arc` so observers receiving
  /// [`StageEvent::ConvertRequest`](crate::event::StageEvent::ConvertRequest)
  /// can clone the payload cheaply.
  pub upstream_body: Arc<Value>,
  pub upstream_wire_body: Bytes,
  /// Uncompressed serialized JSON, mirroring the legacy
  /// `prepare_request` behaviour where structured logs / tests want to
  /// inspect the outbound payload without inflating it.
  pub debug_outbound_body: Bytes,
  pub content_encoding: Option<ContentEncodingKind>,
}

/// Output of [`SendStage`]: a live upstream HTTP response plus the metadata
/// downstream stages need without consuming the body.
///
/// Holds the raw [`reqwest::Response`] so that [`ConvertResponseStage`] can
/// choose at use-time whether to drain it into a buffered JSON payload or
/// wrap it in an SSE pipeline. Not `Clone`: the response is single-shot.
pub struct SentResponse {
  pub status: u16,
  pub headers: HeaderMap,
  /// Whether the request *asked* for SSE streaming (mirrors `Extracted.stream`).
  /// ConvertResponse uses this to pick the buffered vs. stream branch.
  pub stream: bool,
  /// Endpoint the upstream provider was actually called with — may differ
  /// from `ctx.endpoint` when a request-shape translation happened in
  /// ConvertRequest.
  pub upstream_endpoint: Endpoint,
  pub response: reqwest::Response,
}

impl std::fmt::Debug for SentResponse {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("SentResponse")
      .field("status", &self.status)
      .field("headers", &self.headers)
      .field("stream", &self.stream)
      .field("upstream_endpoint", &self.upstream_endpoint)
      .field("response", &"<reqwest::Response>")
      .finish()
  }
}

/// Output of [`ConvertResponseStage`]: either a fully-buffered response
/// ready for one-shot delivery, or a streaming SSE byte source that should
/// be forwarded to the client as it arrives.
pub enum ConvertedResponse {
  Buffered {
    status: u16,
    headers: HeaderMap,
    /// Buffered upstream JSON. `Arc`-wrapped so the matching
    /// [`StageEvent::ConvertResponse`](crate::event::StageEvent::ConvertResponse)
    /// payload can share the value without re-serializing the body.
    body_json: Arc<Value>,
    body_bytes: Bytes,
  },
  Stream {
    status: u16,
    headers: HeaderMap,
    /// SSE byte stream ready to forward to the client. When upstream and
    /// inbound endpoints differ, frames are already endpoint-translated.
    body: futures_util::stream::BoxStream<'static, std::io::Result<Bytes>>,
  },
}

impl ConvertedResponse {
  pub fn status(&self) -> u16 {
    match self {
      Self::Buffered { status, .. } | Self::Stream { status, .. } => *status,
    }
  }

  pub fn headers(&self) -> &HeaderMap {
    match self {
      Self::Buffered { headers, .. } | Self::Stream { headers, .. } => headers,
    }
  }
}

impl std::fmt::Debug for ConvertedResponse {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Buffered {
        status,
        headers,
        body_bytes,
        ..
      } => f
        .debug_struct("ConvertedResponse::Buffered")
        .field("status", status)
        .field("headers", headers)
        .field("body_bytes_len", &body_bytes.len())
        .finish(),
      Self::Stream { status, headers, .. } => f
        .debug_struct("ConvertedResponse::Stream")
        .field("status", status)
        .field("headers", headers)
        .field("body", &"<sse stream>")
        .finish(),
    }
  }
}

#[async_trait]
pub trait ExtractStage: Send + Sync {
  async fn extract(&self, ctx: &PipelineCtx, raw: RawInbound) -> Result<Extracted, PipelineError>;
}

#[async_trait]
pub trait ResolveStage: Send + Sync {
  async fn resolve(&self, ctx: &PipelineCtx, extracted: &Extracted) -> Result<Resolved, PipelineError>;
}

#[async_trait]
pub trait BuildHeadersStage: Send + Sync {
  async fn build_headers(
    &self,
    ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
  ) -> Result<BuiltHeaders, PipelineError>;
}

#[async_trait]
pub trait ConvertRequestStage: Send + Sync {
  async fn convert_request(
    &self,
    ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
  ) -> Result<ConvertedRequest, PipelineError>;
}

#[async_trait]
pub trait SendStage: Send + Sync {
  async fn send(
    &self,
    ctx: &PipelineCtx,
    extracted: &Extracted,
    resolved: &Resolved,
    headers: &BuiltHeaders,
    body: &ConvertedRequest,
  ) -> Result<SentResponse, PipelineError>;
}

#[async_trait]
pub trait ConvertResponseStage: Send + Sync {
  async fn convert_response(&self, ctx: &PipelineCtx, sent: SentResponse) -> Result<ConvertedResponse, PipelineError>;
}
