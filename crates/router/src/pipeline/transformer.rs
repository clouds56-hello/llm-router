use super::request::PreparedRequest;
use crate::api::AppState;
use crate::relay::{buffered_response, stream_response, ForwardContext};
use async_trait::async_trait;
use axum::response::Response;
use tokn_core::pipeline::{OutputTransformer, RequestMeta};
use serde_json::Value;
use std::time::Instant;

pub(super) struct UpstreamResponse {
  pub(super) meta: RequestMeta,
  pub(super) inbound_body: Value,
  pub(super) resp: reqwest::Response,
  pub(super) started: Instant,
}

pub(super) struct EndpointOutputTransformer;

#[async_trait]
impl OutputTransformer for EndpointOutputTransformer {
  type State = AppState;
  type Upstream = UpstreamResponse;
  type Output = Response;

  async fn transform_result(&self, state: AppState, upstream: UpstreamResponse) -> Response {
    let ctx = ForwardContext::from_pipeline(
      upstream.meta.endpoint,
      upstream.meta.upstream_endpoint,
      upstream.meta.model,
      upstream.meta.session_id,
      upstream.meta.request_id.unwrap_or_default(),
      upstream.meta.attempt,
      upstream.started,
    );
    let mut ctx = ctx;
    ctx.downstream_headers = upstream.meta.inbound_headers.clone().into();
    buffered_response(state, upstream.resp, ctx, &upstream.inbound_body).await
  }

  async fn transform_sse(&self, state: AppState, upstream: UpstreamResponse) -> Response {
    let ctx = ForwardContext::from_pipeline(
      upstream.meta.endpoint,
      upstream.meta.upstream_endpoint,
      upstream.meta.model,
      upstream.meta.session_id,
      upstream.meta.request_id.unwrap_or_default(),
      upstream.meta.attempt,
      upstream.started,
    );
    let mut ctx = ctx;
    ctx.downstream_headers = upstream.meta.inbound_headers.clone().into();
    stream_response(state, upstream.resp, ctx, &upstream.inbound_body).await
  }
}

impl From<(PreparedRequest, reqwest::Response, Instant)> for UpstreamResponse {
  fn from((prepared, resp, started): (PreparedRequest, reqwest::Response, Instant)) -> Self {
    Self {
      meta: prepared.meta,
      inbound_body: prepared.inbound_body,
      resp,
      started,
    }
  }
}
