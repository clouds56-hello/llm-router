use super::observers::{background_stream_recorder, StreamMeta};
use super::recording::CompletedEventBuilder;
use crate::db::HttpSnapshot;
use crate::provider::Endpoint;
use crate::server::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use llm_convert::sse::{observer_channel, EndpointTranslator, SsePipeline};
use serde_json::Value;
use std::time::Instant;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_response(
  s: AppState,
  resp: reqwest::Response,
  endpoint: Endpoint,
  upstream_endpoint: Endpoint,
  model: String,
  session_id: Option<String>,
  request_id: Option<String>,
  req_body: Value,
  started: Instant,
) -> Response {
  let status = resp.status();
  let resp_headers = resp.headers().clone();
  let max_body = s.body_max_bytes;
  let headers = sse_headers(session_id.as_deref());
  let inbound_resp_headers = headers.clone();

  let builder = CompletedEventBuilder::new(
    max_body,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: inbound_resp_headers,
      body: Bytes::new(),
    },
    started,
    status.as_u16(),
  )
  .with_ids(session_id.as_deref(), request_id.as_deref(), None)
  .with_request_body(&req_body, Some(endpoint));

  let (tx, rx) = observer_channel();
  let meta = StreamMeta {
    request_id: request_id.clone(),
    model: model.clone(),
    endpoint: endpoint.as_str().to_string(),
    events: s.events.clone(),
  };
  tokio::spawn(background_stream_recorder(rx, builder, resp_headers, s.events.clone(), max_body, meta));

  let mut pipeline = SsePipeline::from_response_with_tap(resp, tx);
  if upstream_endpoint != endpoint {
    pipeline = pipeline.with_transformer(EndpointTranslator::new(upstream_endpoint, endpoint));
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
