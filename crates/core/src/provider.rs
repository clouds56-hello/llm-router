use crate::account::AccountConfig;
use async_trait::async_trait;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use serde::Serialize;
use serde_json::Value;
use std::sync::{Arc, OnceLock};

pub mod error;

pub use error::{Error, Result};

pub const ID_GITHUB_COPILOT: &str = "github-copilot";
pub const ID_ZAI_CODING_PLAN: &str = "zai-coding-plan";
pub const ID_ZAI: &str = "zai";
pub const ID_ZHIPUAI_CODING_PLAN: &str = "zhipuai-coding-plan";
pub const ID_ZHIPUAI: &str = "zhipuai";
pub const ZAI_PROVIDERS: &[&str] = &[ID_ZAI_CODING_PLAN, ID_ZAI, ID_ZHIPUAI_CODING_PLAN, ID_ZHIPUAI];

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthKind {
  OAuthDeviceFlow,
  StaticApiKey,
}

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

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum Interleaved {
  Disabled(bool),
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
  pub input: f64,
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
  pub id: String,
  pub name: String,
  pub capabilities: Capabilities,
  pub cost: Option<Cost>,
  pub limit: Limits,
  pub release_date: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
  pub id: String,
  pub aliases: &'static [&'static str],
  pub display_name: &'static str,
  pub upstream_url: String,
  pub auth_kind: AuthKind,
  pub default_models: Vec<ModelInfo>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Endpoint {
  ChatCompletions,
  Responses,
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

pub struct RequestCtx<'a> {
  pub endpoint: Endpoint,
  pub http: &'a reqwest::Client,
  pub body: &'a Value,
  pub stream: bool,
  pub initiator: &'a str,
  pub inbound_headers: &'a HeaderMap,
  pub behave_as: Option<&'a str>,
  pub outbound: Option<OutboundCapture>,
}

impl RequestCtx<'_> {
  pub fn capture_outbound(&self, method: &str, url: &str, headers: &HeaderMap, body: Bytes) {
    if let Some(slot) = self.outbound.as_ref() {
      let _ = slot.set(crate::db::OutboundSnapshot {
        method: Some(method.to_string()),
        url: Some(url.to_string()),
        status: None,
        headers: headers.clone(),
        body,
      });
    }
  }
}

pub type OutboundCapture = Arc<OnceLock<crate::db::OutboundSnapshot>>;

pub fn new_outbound_capture() -> OutboundCapture {
  Arc::new(OnceLock::new())
}

pub struct ProviderDescriptor {
  pub id: &'static str,
  pub validate: fn(&AccountConfig) -> Result<()>,
  pub build: fn(Arc<AccountConfig>) -> Result<Arc<dyn Provider>>,
}

impl ProviderDescriptor {
  pub fn matches(&self, id: &str) -> bool {
    self.id == id
  }
}

#[async_trait]
pub trait Provider: Send + Sync {
  fn id(&self) -> &str;
  fn info(&self) -> &ProviderInfo;

  fn model_info(&self, model: &str) -> Option<&ModelInfo> {
    self.info().default_models.iter().find(|m| m.id == model)
  }

  fn supports(&self, _model: &str, endpoint: Endpoint) -> bool {
    matches!(endpoint, Endpoint::ChatCompletions)
  }

  async fn list_models(&self, http: &reqwest::Client) -> Result<Value>;
  async fn chat(&self, ctx: RequestCtx<'_>) -> Result<reqwest::Response>;

  async fn responses(&self, _ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    error::UnsupportedEndpointSnafu {
      provider: self.info().id.clone(),
      endpoint: "/v1/responses",
    }
    .fail()
  }

  async fn messages(&self, _ctx: RequestCtx<'_>) -> Result<reqwest::Response> {
    error::UnsupportedEndpointSnafu {
      provider: self.info().id.clone(),
      endpoint: "/v1/messages",
    }
    .fail()
  }

  fn on_unauthorized(&self) {}

  fn needs_refresh(&self, _cfg: &AccountConfig) -> bool {
    false
  }

  async fn refresh(&self, cfg: &AccountConfig, _http: &reqwest::Client) -> Result<AccountConfig> {
    Ok(cfg.clone())
  }
}
