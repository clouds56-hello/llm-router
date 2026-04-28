//! Provider abstraction.
//!
//! A provider knows how to authenticate, list models, and execute a chat
//! completion request. The gateway pool stores providers behind a trait object
//! so adding new upstreams (Anthropic, Gemini, …) is purely additive.
//!
//! Providers also publish a [`ProviderInfo`] describing themselves and a
//! catalogue of well-known models with capability/cost/limit metadata. The
//! server uses this to enrich `/v1/models` and providers themselves use it to
//! drive request shaping (e.g. injecting a `thinking` block for reasoning
//! models).

use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

pub mod error;
pub mod github_copilot;
pub mod profiles;
pub mod zai;

pub use error::{Error, Result};

pub const ID_GITHUB_COPILOT: &str = "github-copilot";

/// Canonical Z.ai provider id. The four wire-aliases below all resolve to the
/// same backend implementation; the user-chosen alias is preserved verbatim on
/// the resulting [`ProviderInfo`] so usage logs reflect what the operator
/// configured.
pub const ID_ZAI_CODING_PLAN: &str = "zai-coding-plan";
pub const ID_ZAI: &str = "zai";
pub const ID_ZHIPUAI_CODING_PLAN: &str = "zhipuai-coding-plan";
pub const ID_ZHIPUAI: &str = "zhipuai";

/// All accepted Z.ai aliases (canonical first).
pub const ZAI_ALIASES: &[&str] = &[ID_ZAI_CODING_PLAN, ID_ZAI, ID_ZHIPUAI_CODING_PLAN, ID_ZHIPUAI];

/// How a provider authenticates. Used by the CLI to pick the right login flow.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
  /// GitHub-style device flow yielding an OAuth token, exchanged for a
  /// short-lived API token.
  OAuthDeviceFlow,
  /// Static long-lived API key pasted by the user.
  StaticApiKey,
}

/// Per-modality flags for `Capabilities::input` / `Capabilities::output`.
#[derive(Debug, Clone, Serialize)]
pub struct Modalities {
  pub text: bool,
  pub audio: bool,
  pub image: bool,
  pub video: bool,
  pub pdf: bool,
}

impl Modalities {
  #[allow(dead_code)]
  pub const TEXT_ONLY: Self = Self {
    text: true,
    audio: false,
    image: false,
    video: false,
    pdf: false,
  };
  #[allow(dead_code)]
  pub const TEXT_IMAGE: Self = Self {
    text: true,
    audio: false,
    image: true,
    video: false,
    pdf: false,
  };
}

/// Whether the model supports interleaved reasoning content. `false` for most
/// providers; some OpenAI-compatible upstreams (DeepSeek, GLM) ship reasoning
/// alongside tool-calls in a side-channel field.
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Interleaved {
  Disabled(bool), // serialised as `false`
  Field { field: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
  pub temperature: bool,
  pub reasoning: bool,
  pub attachment: bool,
  pub toolcall: bool,
  pub input: Modalities,
  pub output: Modalities,
  pub interleaved: Interleaved,
}

#[derive(Debug, Clone, Serialize)]
pub struct Cost {
  /// USD per 1K input tokens.
  pub input: f64,
  /// USD per 1K output tokens.
  pub output: f64,
  pub cache: Option<CacheCost>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CacheCost {
  pub read: f64,
  pub write: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Limits {
  pub context: u32,
  pub output: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelInfo {
  /// Wire id (what the model reports as `id` and what callers send).
  pub id: String,
  pub name: String,
  pub capabilities: Capabilities,
  pub cost: Option<Cost>,
  pub limit: Limits,
  pub release_date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
  /// User-facing canonical id. For aliased providers (e.g. Z.ai) this is
  /// whichever alias the operator wrote in `[[accounts]] provider = "..."`.
  pub id: String,
  /// All ids that resolve to this provider impl (for documentation only).
  pub aliases: &'static [&'static str],
  pub display_name: &'static str,
  pub upstream_url: String,
  pub auth_kind: AuthKind,
  /// Built-in model catalogue. Used as a metadata overlay on `/v1/models`
  /// and as the source-of-truth for capability-driven request shaping.
  pub default_models: Vec<ModelInfo>,
}

/// The inbound API surface a request belongs to. Each variant is wired to a
/// distinct upstream path on providers that support it. Bytes are forwarded
/// verbatim; no cross-format translation happens in this crate today.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
  /// `POST /v1/chat/completions` (OpenAI Chat Completions).
  ChatCompletions,
  /// `POST /v1/responses` (OpenAI Responses API).
  Responses,
  /// `POST /v1/messages` (Anthropic Messages API).
  Messages,
}

impl Endpoint {
  pub fn as_str(self) -> &'static str {
    match self {
      Endpoint::ChatCompletions => "chat_completions",
      Endpoint::Responses => "responses",
      Endpoint::Messages => "messages",
    }
  }
}

impl std::fmt::Display for Endpoint {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Per-request context handed to a provider. The same shape covers every
/// inbound endpoint — the `endpoint` field tells the provider (and the
/// dispatcher) which surface this request belongs to.
pub struct RequestCtx<'a> {
  pub endpoint: Endpoint,
  pub http: &'a reqwest::Client,
  pub body: &'a Value,
  pub stream: bool,
  /// "user" or "agent" — pre-classified by the server. Providers that don't
  /// care can ignore it.
  pub initiator: &'a str,
  /// Inbound headers from the downstream client. Providers may forward
  /// selected ones (e.g. an explicit `X-Initiator`).
  pub inbound_headers: &'a HeaderMap,
  /// Persona override from the inbound `X-Behave-As` request header, if any.
  /// Takes precedence over config-file `behave_as` settings.
  pub behave_as: Option<&'a str>,
}

#[async_trait]
pub trait Provider: Send + Sync {
  /// Stable id, e.g. "github-copilot:personal" or "zai-coding-plan:work".
  #[allow(dead_code)]
  fn id(&self) -> &str;

  /// Provider-level metadata (display name, auth kind, model catalogue).
  fn info(&self) -> &ProviderInfo;

  /// Look up our metadata for a wire-id. Default impl scans
  /// `info().default_models`.
  fn model_info(&self, model: &str) -> Option<&ModelInfo> {
    self.info().default_models.iter().find(|m| m.id == model)
  }

  /// Whether this provider speaks `endpoint` for the given model. The pool
  /// uses this to skip accounts that cannot service a request.
  ///
  /// Default impl: every provider is assumed chat-completions-capable for
  /// every model; Responses / Messages are opt-in (return `false`).
  /// Providers that natively passthrough to upstream `/responses` or
  /// `/v1/messages` should override.
  fn supports(&self, _model: &str, endpoint: Endpoint) -> bool {
    matches!(endpoint, Endpoint::ChatCompletions)
  }

  /// `GET /models`. Result must be a JSON object suitable for inclusion in
  /// an OpenAI `/v1/models` response (an `{ "object":"list", "data":[...] }`
  /// object — the server unions `data` arrays across providers).
  async fn list_models(&self, http: &reqwest::Client) -> Result<Value>;

  /// Execute a chat completion. Returns the raw upstream `reqwest::Response`
  /// so the caller can stream or buffer the body verbatim.
  async fn chat(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response>;

  /// Execute a Responses-API request (`POST /v1/responses` upstream).
  /// Default impl returns an error — providers that natively support the
  /// surface override.
  async fn responses(&self, _ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    error::UnsupportedEndpointSnafu {
      provider: self.info().id.clone(),
      endpoint: "/v1/responses",
    }
    .fail()
  }

  /// Execute an Anthropic Messages-API request (`POST /v1/messages`
  /// upstream). Default impl returns an error.
  async fn messages(&self, _ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    error::UnsupportedEndpointSnafu {
      provider: self.info().id.clone(),
      endpoint: "/v1/messages",
    }
    .fail()
  }

  /// Called by the pool when an upstream 401 occurs, so the provider may
  /// invalidate any cached short-lived token. Default: no-op.
  fn on_unauthorized(&self) {}
}

/// Build a provider from an account config row.
pub fn build_for_account(
  a: &crate::config::Account,
  global_headers: &crate::config::CopilotHeaders,
) -> Result<Arc<dyn Provider>> {
  match a.provider.as_str() {
    ID_GITHUB_COPILOT => {
      let p = github_copilot::CopilotProvider::from_account(a, global_headers)?;
      Ok(Arc::new(p))
    }
    id if ZAI_ALIASES.contains(&id) => {
      let p = zai::ZaiProvider::from_account(a)?;
      Ok(Arc::new(p))
    }
    other => error::UnknownProviderSnafu {
      id: other.to_string(),
      account: a.id.clone(),
    }
    .fail(),
  }
}
