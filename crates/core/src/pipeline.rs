use crate::provider::Endpoint;
use reqwest::header::HeaderMap;
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct RequestMeta {
  pub endpoint: Endpoint,
  pub upstream_endpoint: Endpoint,
  pub model: String,
  pub upstream_model: String,
  pub stream: bool,
  pub session_id: Option<String>,
  pub request_id: Option<String>,
  pub project_id: Option<String>,
  pub initiator: String,
  pub behave_as: Option<String>,
  pub inbound_headers: HeaderMap,
}

#[derive(Clone, Debug)]
pub struct ParsedRequest {
  pub meta: RequestMeta,
  pub body: Value,
}

pub trait InputTransformer: Send + Sync {
  fn transform_input(&self, meta: &RequestMeta, body: Value) -> crate::provider::Result<Value>;
}
