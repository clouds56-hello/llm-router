use super::observers::{background_stream_recorder, StreamMeta};
use super::recording::CallRecordBuilder;
use crate::db::{HttpSnapshot, OutboundSnapshot};
use crate::pool::AccountHandle;
use crate::provider::Endpoint;
use crate::server::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use llm_convert::sse::{observer_channel, EndpointTranslator, SsePipeline};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn stream_response(
  s: AppState,
  reporter: Arc<dyn llm_core::pipeline::RequestReporter>,
  acct: Arc<AccountHandle>,
  resp: reqwest::Response,
  endpoint: Endpoint,
  upstream_endpoint: Endpoint,
  model: String,
  initiator: String,
  session_id: Option<String>,
  request_id: Option<String>,
  project_id: Option<String>,
  req_headers: HeaderMap,
  req_body: Value,
  outbound: Option<OutboundSnapshot>,
  started: Instant,
) -> Response {
  let status = resp.status();
  let resp_headers = resp.headers().clone();
  let max_body = s.body_max_bytes;
  let headers = sse_headers(session_id.as_deref());
  let inbound_resp_headers = headers.clone();

  let builder = CallRecordBuilder::for_endpoint(
    max_body,
    &acct.id(),
    acct.provider.info().id.as_str(),
    endpoint,
    &model,
    &initiator,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: inbound_resp_headers,
      body: Bytes::new(),
    },
    started,
    status.as_u16(),
    true,
  )
  .with_ids(
    session_id.as_deref(),
    request_id.as_deref(),
    None,
    project_id.as_deref(),
  )
  .with_request_json(&req_headers, &req_body)
  .with_outbound_request(outbound);

  let (tx, rx) = observer_channel();
  let meta = StreamMeta {
    request_id: request_id.clone(),
    model: model.clone(),
    endpoint: endpoint.as_str().to_string(),
    events: s.events.clone(),
  };
  tokio::spawn(background_stream_recorder(rx, builder, resp_headers, reporter, max_body, meta));

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
