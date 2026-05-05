use crate::provider::Endpoint;
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

pub trait OutputTransformer: Send + Sync {
  type State;
  type Upstream;
  type Output;

  fn transform_result<'a>(
    &'a self,
    state: Self::State,
    upstream: Self::Upstream,
  ) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

  fn transform_sse<'a>(
    &'a self,
    state: Self::State,
    upstream: Self::Upstream,
  ) -> Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;
}
