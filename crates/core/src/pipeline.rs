use crate::provider::Endpoint;
use async_trait::async_trait;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;

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
  /// Merged initiator (header takes precedence over body-derived).
  pub initiator: String,
  /// Raw initiator from x-initiator header, if valid.
  pub header_initiator: Option<String>,
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

pub trait RequestResolver: Send + Sync {
  type State;
  type Resolved;
  type Error;

  fn resolve(&self, state: &Self::State, parsed: ParsedRequest, attempt: usize) -> Result<Self::Resolved, Self::Error>;
}

pub trait RequestSender: Send + Sync {
  type State;
  type Request;
  type Response;
  type Error;

  fn send<'a>(
    &'a self,
    state: &'a Self::State,
    req: &'a Self::Request,
  ) -> Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'a>>;
}

#[async_trait]
pub trait OutputTransformer: Send + Sync {
  type State;
  type Upstream;
  type Output;

  async fn transform_result(&self, state: Self::State, upstream: Self::Upstream) -> Self::Output;

  async fn transform_sse(
    &self,
    state: Self::State,
    upstream: Self::Upstream,
  ) -> Self::Output;
}
