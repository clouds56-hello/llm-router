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
use async_trait::async_trait;
use bytes::Bytes;
use llm_core::provider::Endpoint;
use llm_core::ClientId;
use llm_headers::HeaderMap;
use serde_json::Value;
use smol_str::SmolStr;

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
#[derive(Debug, Clone)]
pub struct Extracted {
  pub endpoint: Endpoint,
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
  pub body_json: Value,
}

/// Output of [`ResolveStage`]: which account+upstream answers this request.
///
/// PR1 keeps the account handle abstract (the `account_handle` field is an
/// `Arc<dyn Any>` for now). PR2 will replace it with a typed handle once we
/// extract the account-pool types into a shared crate.
#[derive(Clone)]
pub struct Resolved {
  pub client_id: Option<ClientId>,
  pub model: SmolStr,
  pub upstream_model: SmolStr,
  pub upstream_endpoint: Endpoint,
  pub account_id: SmolStr,
  pub provider_id: SmolStr,
  pub account_handle: std::sync::Arc<dyn std::any::Any + Send + Sync>,
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

/// Placeholder output of [`BuildHeadersStage`] (PR2 will flesh this out).
#[derive(Debug, Clone, Default)]
pub struct BuiltHeaders {
  pub headers: HeaderMap,
}

/// Placeholder output of [`ConvertRequestStage`] (PR2).
#[derive(Debug, Clone)]
pub struct ConvertedRequest {
  pub upstream_body: Value,
  pub upstream_wire_body: Bytes,
}

/// Placeholder output of [`SendStage`] (PR2).
pub struct SentResponse;

/// Placeholder output of [`ConvertResponseStage`] (PR2).
pub struct ConvertedResponse;

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
    resolved: &Resolved,
    headers: &BuiltHeaders,
    body: &ConvertedRequest,
  ) -> Result<SentResponse, PipelineError>;
}

#[async_trait]
pub trait ConvertResponseStage: Send + Sync {
  async fn convert_response(
    &self,
    ctx: &PipelineCtx,
    sent: SentResponse,
  ) -> Result<ConvertedResponse, PipelineError>;
}
