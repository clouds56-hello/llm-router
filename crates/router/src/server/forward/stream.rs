use super::observers::build_stream_record;
use super::recording::CallRecordBuilder;
use super::usage::parse_usage_any_value;
use crate::db::{HttpSnapshot, OutboundSnapshot};
use crate::pool::AccountHandle;
use crate::provider::Endpoint;
use crate::server::AppState;
use axum::body::Body;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use llm_convert::sse::{observer_channel, EndpointTranslator, ObserverMsg, SsePipeline};
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

  // Create observer channel and spawn background recorder
  let (tx, rx) = observer_channel();
  tokio::spawn(background_stream_recorder(rx, builder, resp_headers, reporter, max_body));

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

/// Background task that processes observer messages to build a call record.
async fn background_stream_recorder(
  mut rx: llm_convert::sse::ObserverReceiver,
  base_builder: CallRecordBuilder,
  resp_headers: reqwest::header::HeaderMap,
  reporter: Arc<dyn llm_core::pipeline::RequestReporter>,
  max_body: usize,
) {
  let mut body_buf: Vec<u8> = Vec::new();
  let mut usage: (Option<u64>, Option<u64>) = (None, None);
  let mut had_error = false;

  while let Some(msg) = rx.recv().await {
    match msg {
      ObserverMsg::To(bytes) => {
        // Accumulate outbound body (what client sees)
        if max_body > 0 {
          let remaining = max_body.saturating_sub(body_buf.len());
          if remaining > 0 {
            body_buf.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
          }
        }
      }
      ObserverMsg::Transformed(Some(value)) => {
        // Extract usage from transformed events (post-transformer JSON)
        let (pt, ct) = parse_usage_any_value(&value);
        if pt.is_some() {
          usage.0 = pt;
        }
        if ct.is_some() {
          usage.1 = ct;
        }
      }
      ObserverMsg::Done => break,
      ObserverMsg::Error(_) => {
        had_error = true;
        break;
      }
      _ => {} // From, Parsed, Transformed(None) — not needed here
    }
  }

  let request_error = had_error.then_some("stream terminated before completion");
  let captured = Bytes::from(body_buf);
  let record = build_stream_record(
    base_builder.with_request_error(request_error),
    usage,
    captured,
    &resp_headers,
  );
  reporter.report(record);
}
