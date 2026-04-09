use std::{collections::HashMap, pin::Pin, sync::Arc, time::Duration};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;
use tokio::sync::RwLock;
use uuid::Uuid;

pub type ChatStream = Pin<Box<dyn Stream<Item = Result<OpenAiChatChunk, RouterError>> + Send>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
  OpenAi,
  Claude,
  Copilot,
  Codex,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
  pub name: String,
  pub kind: ProviderKind,
  pub base_url: String,
  pub api_key_env: String,
  #[serde(default = "default_timeout_ms")]
  pub timeout_ms: u64,
  #[serde(default = "default_backoff_ms")]
  pub backoff_ms: u64,
}

fn default_timeout_ms() -> u64 {
  30_000
}

fn default_backoff_ms() -> u64 {
  10_000
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteConfig {
  pub name: String,
  pub model_aliases: Vec<String>,
  pub failover_chain: Vec<String>,
  #[serde(default = "default_retry_budget")]
  pub retry_budget: usize,
  #[serde(default = "default_circuit_failures")]
  pub circuit_open_after_failures: u32,
}

fn default_retry_budget() -> usize {
  2
}

fn default_circuit_failures() -> u32 {
  3
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMapping {
  pub public_model: String,
  pub provider: String,
  pub provider_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListenerConfig {
  #[serde(default = "default_bind")]
  pub bind: String,
}

fn default_bind() -> String {
  "127.0.0.1:8787".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ViewerConfig {
  #[serde(default = "default_enabled")]
  pub enabled: bool,
  #[serde(default = "default_viewer_prefix")]
  pub path_prefix: String,
  #[serde(default = "default_db_path")]
  pub sqlite_path: String,
  #[serde(default = "default_max_rows")]
  pub max_rows: u64,
  #[serde(default = "default_max_age_seconds")]
  pub max_age_seconds: u64,
}

fn default_enabled() -> bool {
  true
}
fn default_viewer_prefix() -> String {
  "/viewer".to_string()
}
fn default_db_path() -> String {
  "llm-router.db".to_string()
}
fn default_max_rows() -> u64 {
  100_000
}
fn default_max_age_seconds() -> u64 {
  7 * 24 * 3600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
  #[serde(rename = "apiVersion")]
  pub api_version: String,
  pub listener: ListenerConfig,
  pub providers: Vec<ProviderConfig>,
  pub routes: Vec<RouteConfig>,
  pub model_mappings: Vec<ModelMapping>,
  pub viewer: ViewerConfig,
}

impl RouterConfig {
  pub fn validate_and_resolve(&self) -> Result<ResolvedConfig, RouterError> {
    if self.api_version.trim().is_empty() {
      return Err(RouterError::Config("apiVersion must be non-empty".to_string()));
    }

    let mut providers = HashMap::new();
    for p in &self.providers {
      let key = std::env::var(&p.api_key_env)
        .map_err(|_| RouterError::Config(format!("missing env var '{}' for provider '{}'", p.api_key_env, p.name)))?;
      providers.insert(
        p.name.clone(),
        ResolvedProvider {
          config: p.clone(),
          api_key: key,
        },
      );
    }

    let mut model_map = HashMap::new();
    for m in &self.model_mappings {
      model_map.insert(m.public_model.clone(), m.clone());
    }

    Ok(ResolvedConfig {
      raw: self.clone(),
      providers,
      model_map,
    })
  }
}

#[derive(Debug, Clone)]
pub struct ResolvedProvider {
  pub config: ProviderConfig,
  pub api_key: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
  pub raw: RouterConfig,
  pub providers: HashMap<String, ResolvedProvider>,
  pub model_map: HashMap<String, ModelMapping>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatMessage {
  pub role: String,
  pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionRequest {
  pub model: String,
  pub messages: Vec<OpenAiChatMessage>,
  #[serde(default)]
  pub stream: bool,
  #[serde(flatten)]
  pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiEmbeddingRequest {
  pub model: String,
  pub input: Value,
  #[serde(flatten)]
  pub extra: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatChoice {
  pub index: usize,
  pub message: OpenAiChatMessage,
  pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiUsage {
  pub prompt_tokens: u32,
  pub completion_tokens: u32,
  pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatCompletionResponse {
  pub id: String,
  pub object: String,
  pub created: i64,
  pub model: String,
  pub choices: Vec<OpenAiChatChoice>,
  pub usage: Option<OpenAiUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatChunkChoice {
  pub index: usize,
  pub delta: Value,
  pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiChatChunk {
  pub id: String,
  pub object: String,
  pub created: i64,
  pub model: String,
  pub choices: Vec<OpenAiChatChunkChoice>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiEmbeddingData {
  pub object: String,
  pub embedding: Vec<f32>,
  pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiEmbeddingResponse {
  pub object: String,
  pub data: Vec<OpenAiEmbeddingData>,
  pub model: String,
  pub usage: OpenAiUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiModelData {
  pub id: String,
  pub object: String,
  pub created: i64,
  pub owned_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiModelList {
  pub object: String,
  pub data: Vec<OpenAiModelData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiErrorEnvelope {
  pub error: OpenAiError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAiError {
  pub message: String,
  #[serde(rename = "type")]
  pub type_name: String,
  pub param: Option<String>,
  pub code: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RequestContext {
  pub request_id: String,
  pub started_at: DateTime<Utc>,
  pub api_key: Option<String>,
  pub route_name: Option<String>,
  pub provider_attempts: Vec<String>,
}

impl RequestContext {
  pub fn new(api_key: Option<String>) -> Self {
    Self {
      request_id: Uuid::new_v4().to_string(),
      started_at: Utc::now(),
      api_key,
      route_name: None,
      provider_attempts: Vec::new(),
    }
  }
}

#[derive(Debug, Clone)]
pub struct ProviderRequestMeta {
  pub provider_name: String,
  pub provider_model: String,
}

#[async_trait]
pub trait ProviderAdapter: Send + Sync {
  fn provider_name(&self) -> &str;

  async fn send_chat(
    &self,
    ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<OpenAiChatCompletionResponse, RouterError>;

  async fn send_chat_stream(
    &self,
    ctx: &RequestContext,
    req: &OpenAiChatCompletionRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<ChatStream, RouterError>;

  async fn send_embeddings(
    &self,
    ctx: &RequestContext,
    req: &OpenAiEmbeddingRequest,
    meta: &ProviderRequestMeta,
  ) -> Result<OpenAiEmbeddingResponse, RouterError>;

  async fn list_models(&self, _ctx: &RequestContext) -> Result<OpenAiModelList, RouterError>;

  fn map_error(&self, err: anyhow::Error) -> RouterError {
    RouterError::Upstream(err.to_string())
  }
}

pub struct AdapterRegistry {
  adapters: HashMap<String, Arc<dyn ProviderAdapter>>,
}

impl AdapterRegistry {
  pub fn new() -> Self {
    Self {
      adapters: HashMap::new(),
    }
  }

  pub fn register(&mut self, provider_name: String, adapter: Arc<dyn ProviderAdapter>) {
    self.adapters.insert(provider_name, adapter);
  }

  pub fn get(&self, provider_name: &str) -> Option<Arc<dyn ProviderAdapter>> {
    self.adapters.get(provider_name).cloned()
  }
}

impl Default for AdapterRegistry {
  fn default() -> Self {
    Self::new()
  }
}

#[derive(Debug, Clone)]
struct CircuitState {
  consecutive_failures: u32,
  open_until: Option<DateTime<Utc>>,
}

pub struct RouterEngine {
  resolved: ResolvedConfig,
  adapters: Arc<AdapterRegistry>,
  circuit: Arc<RwLock<HashMap<String, CircuitState>>>,
}

impl RouterEngine {
  pub fn new(resolved: ResolvedConfig, adapters: Arc<AdapterRegistry>) -> Self {
    Self {
      resolved,
      adapters,
      circuit: Arc::new(RwLock::new(HashMap::new())),
    }
  }

  pub fn config(&self) -> &ResolvedConfig {
    &self.resolved
  }

  fn route_for_model(&self, model: &str) -> Result<&RouteConfig, RouterError> {
    self
      .resolved
      .raw
      .routes
      .iter()
      .find(|r| r.model_aliases.iter().any(|m| m == model))
      .ok_or_else(|| RouterError::NotFound(format!("no route for model '{}'", model)))
  }

  fn provider_meta_for_model(&self, public_model: &str) -> Result<ProviderRequestMeta, RouterError> {
    let m = self
      .resolved
      .model_map
      .get(public_model)
      .ok_or_else(|| RouterError::NotFound(format!("no model mapping for '{}'", public_model)))?;
    Ok(ProviderRequestMeta {
      provider_name: m.provider.clone(),
      provider_model: m.provider_model.clone(),
    })
  }

  async fn is_circuit_open(&self, provider: &str) -> bool {
    let state = self.circuit.read().await;
    state
      .get(provider)
      .and_then(|s| s.open_until)
      .map(|t| Utc::now() < t)
      .unwrap_or(false)
  }

  async fn mark_success(&self, provider: &str) {
    let mut state = self.circuit.write().await;
    state.insert(
      provider.to_string(),
      CircuitState {
        consecutive_failures: 0,
        open_until: None,
      },
    );
  }

  async fn mark_failure(&self, provider: &str, route: &RouteConfig) {
    let mut state = self.circuit.write().await;
    let entry = state.entry(provider.to_string()).or_insert(CircuitState {
      consecutive_failures: 0,
      open_until: None,
    });
    entry.consecutive_failures += 1;

    if entry.consecutive_failures >= route.circuit_open_after_failures {
      let backoff_ms = self
        .resolved
        .providers
        .get(provider)
        .map(|p| p.config.backoff_ms)
        .unwrap_or(10_000);
      entry.open_until = Some(Utc::now() + chrono::Duration::milliseconds(backoff_ms as i64));
    }
  }

  pub async fn route_chat(
    &self,
    ctx: &mut RequestContext,
    req: &OpenAiChatCompletionRequest,
  ) -> Result<OpenAiChatCompletionResponse, RouterError> {
    let route = self.route_for_model(&req.model)?.clone();
    ctx.route_name = Some(route.name.clone());
    let mapped = self.provider_meta_for_model(&req.model)?;

    let mut attempts = 0usize;
    let mut last_err: Option<RouterError> = None;

    for provider_name in route.failover_chain.iter() {
      if attempts > route.retry_budget {
        break;
      }
      attempts += 1;

      if self.is_circuit_open(provider_name).await {
        continue;
      }

      let adapter = match self.adapters.get(provider_name) {
        Some(a) => a,
        None => continue,
      };

      let chosen_model = if provider_name == &mapped.provider_name {
        mapped.provider_model.clone()
      } else {
        req.model.clone()
      };

      let meta = ProviderRequestMeta {
        provider_name: provider_name.clone(),
        provider_model: chosen_model,
      };

      ctx.provider_attempts.push(provider_name.clone());
      let timeout_ms = self
        .resolved
        .providers
        .get(provider_name)
        .map(|p| p.config.timeout_ms)
        .unwrap_or(30_000);

      let call = tokio::time::timeout(Duration::from_millis(timeout_ms), adapter.send_chat(ctx, req, &meta)).await;

      match call {
        Ok(Ok(resp)) => {
          self.mark_success(provider_name).await;
          return Ok(resp);
        }
        Ok(Err(err)) => {
          self.mark_failure(provider_name, &route).await;
          last_err = Some(err);
        }
        Err(_) => {
          self.mark_failure(provider_name, &route).await;
          last_err = Some(RouterError::Timeout(format!("provider '{}' timed out", provider_name)));
        }
      }
    }

    Err(last_err.unwrap_or_else(|| RouterError::Upstream("all providers failed".to_string())))
  }

  pub async fn route_chat_stream(
    &self,
    ctx: &mut RequestContext,
    req: &OpenAiChatCompletionRequest,
  ) -> Result<ChatStream, RouterError> {
    let route = self.route_for_model(&req.model)?.clone();
    ctx.route_name = Some(route.name.clone());
    let mapped = self.provider_meta_for_model(&req.model)?;

    let mut attempts = 0usize;
    let mut last_err: Option<RouterError> = None;

    for provider_name in route.failover_chain.iter() {
      if attempts > route.retry_budget {
        break;
      }
      attempts += 1;

      if self.is_circuit_open(provider_name).await {
        continue;
      }
      let adapter = match self.adapters.get(provider_name) {
        Some(a) => a,
        None => continue,
      };

      let chosen_model = if provider_name == &mapped.provider_name {
        mapped.provider_model.clone()
      } else {
        req.model.clone()
      };

      let meta = ProviderRequestMeta {
        provider_name: provider_name.clone(),
        provider_model: chosen_model,
      };
      ctx.provider_attempts.push(provider_name.clone());
      let timeout_ms = self
        .resolved
        .providers
        .get(provider_name)
        .map(|p| p.config.timeout_ms)
        .unwrap_or(30_000);

      let call = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        adapter.send_chat_stream(ctx, req, &meta),
      )
      .await;

      match call {
        Ok(Ok(resp)) => {
          self.mark_success(provider_name).await;
          return Ok(resp);
        }
        Ok(Err(err)) => {
          self.mark_failure(provider_name, &route).await;
          last_err = Some(err);
        }
        Err(_) => {
          self.mark_failure(provider_name, &route).await;
          last_err = Some(RouterError::Timeout(format!("provider '{}' timed out", provider_name)));
        }
      }
    }

    Err(last_err.unwrap_or_else(|| RouterError::Upstream("all providers failed".to_string())))
  }

  pub async fn route_embeddings(
    &self,
    ctx: &mut RequestContext,
    req: &OpenAiEmbeddingRequest,
  ) -> Result<OpenAiEmbeddingResponse, RouterError> {
    let route = self.route_for_model(&req.model)?.clone();
    ctx.route_name = Some(route.name.clone());
    let mapped = self.provider_meta_for_model(&req.model)?;

    let mut last_err: Option<RouterError> = None;
    let mut attempts = 0usize;

    for provider_name in route.failover_chain.iter() {
      if attempts > route.retry_budget {
        break;
      }
      attempts += 1;

      if self.is_circuit_open(provider_name).await {
        continue;
      }
      let adapter = match self.adapters.get(provider_name) {
        Some(a) => a,
        None => continue,
      };

      let chosen_model = if provider_name == &mapped.provider_name {
        mapped.provider_model.clone()
      } else {
        req.model.clone()
      };

      let meta = ProviderRequestMeta {
        provider_name: provider_name.clone(),
        provider_model: chosen_model,
      };
      ctx.provider_attempts.push(provider_name.clone());

      let timeout_ms = self
        .resolved
        .providers
        .get(provider_name)
        .map(|p| p.config.timeout_ms)
        .unwrap_or(30_000);

      let call = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        adapter.send_embeddings(ctx, req, &meta),
      )
      .await;

      match call {
        Ok(Ok(resp)) => {
          self.mark_success(provider_name).await;
          return Ok(resp);
        }
        Ok(Err(err)) => {
          self.mark_failure(provider_name, &route).await;
          last_err = Some(err);
        }
        Err(_) => {
          self.mark_failure(provider_name, &route).await;
          last_err = Some(RouterError::Timeout(format!("provider '{}' timed out", provider_name)));
        }
      }
    }

    Err(last_err.unwrap_or_else(|| RouterError::Upstream("all providers failed".to_string())))
  }

  pub async fn list_models(&self, ctx: &RequestContext) -> Result<OpenAiModelList, RouterError> {
    let mut data = Vec::new();
    for adapter in self.adapters.adapters.values() {
      if let Ok(models) = adapter.list_models(ctx).await {
        data.extend(models.data);
      }
    }

    if data.is_empty() {
      for m in self.resolved.model_map.keys() {
        data.push(OpenAiModelData {
          id: m.clone(),
          object: "model".to_string(),
          created: Utc::now().timestamp(),
          owned_by: "llm-router".to_string(),
        });
      }
    }

    Ok(OpenAiModelList {
      object: "list".to_string(),
      data,
    })
  }
}

#[derive(Debug, Error, Clone)]
pub enum RouterError {
  #[error("bad request: {0}")]
  BadRequest(String),
  #[error("unauthorized: {0}")]
  Unauthorized(String),
  #[error("not found: {0}")]
  NotFound(String),
  #[error("timeout: {0}")]
  Timeout(String),
  #[error("upstream error: {0}")]
  Upstream(String),
  #[error("config error: {0}")]
  Config(String),
  #[error("internal error: {0}")]
  Internal(String),
}

impl RouterError {
  pub fn status_code(&self) -> u16 {
    match self {
      RouterError::BadRequest(_) => 400,
      RouterError::Unauthorized(_) => 401,
      RouterError::NotFound(_) => 404,
      RouterError::Timeout(_) => 504,
      RouterError::Upstream(_) => 502,
      RouterError::Config(_) | RouterError::Internal(_) => 500,
    }
  }

  pub fn as_openai_error(&self) -> OpenAiErrorEnvelope {
    let (type_name, code) = match self {
      RouterError::BadRequest(_) => ("invalid_request_error", Some("bad_request".to_string())),
      RouterError::Unauthorized(_) => ("authentication_error", Some("unauthorized".to_string())),
      RouterError::NotFound(_) => ("invalid_request_error", Some("not_found".to_string())),
      RouterError::Timeout(_) => ("server_error", Some("timeout".to_string())),
      RouterError::Upstream(_) => ("server_error", Some("upstream_error".to_string())),
      RouterError::Config(_) | RouterError::Internal(_) => ("server_error", Some("internal_error".to_string())),
    };

    OpenAiErrorEnvelope {
      error: OpenAiError {
        message: self.to_string(),
        type_name: type_name.to_string(),
        param: None,
        code,
      },
    }
  }

  pub fn simple_chunk(model: &str, content: &str, finish_reason: Option<&str>) -> OpenAiChatChunk {
    OpenAiChatChunk {
      id: format!("chatcmpl-{}", Uuid::new_v4()),
      object: "chat.completion.chunk".to_string(),
      created: Utc::now().timestamp(),
      model: model.to_string(),
      choices: vec![OpenAiChatChunkChoice {
        index: 0,
        delta: json!({"content": content}),
        finish_reason: finish_reason.map(ToString::to_string),
      }],
    }
  }
}
