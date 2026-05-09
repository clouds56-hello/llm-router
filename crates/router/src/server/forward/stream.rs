use super::context::ForwardContext;
use super::observers::{spawn_stream_recorder, StreamMeta};
use super::recording::CompletedEventBuilder;
use crate::db::HttpSnapshot;
use crate::server::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use llm_convert::sse::{EndpointTranslator, SsePipeline};

use serde_json::Value;

pub(crate) async fn stream_response(
  s: AppState,
  resp: reqwest::Response,
  ctx: ForwardContext,
  req_body: &Value,
) -> Response {
  let status = resp.status();
  let resp_headers = resp.headers().clone();
  let max_body = s.body_max_bytes;
  let headers = sse_headers(ctx.session_id.as_deref());

  let builder = CompletedEventBuilder::new(
    max_body,
    ctx.request_id.clone(),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: headers.clone(),
      body: Bytes::new(),
    },
    ctx.started,
    status.as_u16(),
  )
  .with_ids(ctx.session_id.as_deref(), None)
  .with_attempt(ctx.attempt)
  .with_request_body(req_body, ctx.endpoint);

  let endpoint = ctx.endpoint.unwrap_or(ctx.upstream_endpoint);
  let meta = StreamMeta {
    request_id: ctx.request_id.clone(),
    attempt: ctx.attempt,
    final_status: status.as_u16(),
    started: ctx.started,
    model: ctx.model.clone(),
    endpoint: endpoint.as_str().to_string(),
    events: s.events.clone(),
  };
  let tx = spawn_stream_recorder(builder, resp_headers, s.events.clone(), max_body, meta);

  let mut pipeline = SsePipeline::from_response_with_tap(resp, tx);
  if ctx.upstream_endpoint != endpoint {
    pipeline = pipeline.with_transformer(EndpointTranslator::new(ctx.upstream_endpoint, endpoint));
  }

  (status, headers, Body::from_stream(pipeline.run())).into_response()
}

fn sse_headers(session_id: Option<&str>) -> HeaderMap {
  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("text/event-stream"),
  );
  if let Some(id) = session_id {
    if let Ok(value) = HeaderValue::from_str(id) {
      headers.insert(crate::server::SESSION_ID_HEADER, value);
    }
  }
  headers.insert(axum::http::header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
  headers.insert(axum::http::header::CONNECTION, HeaderValue::from_static("keep-alive"));
  headers
}
