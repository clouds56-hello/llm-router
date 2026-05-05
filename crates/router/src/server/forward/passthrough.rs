use super::observers::{
  build_stream_record, BodyCaptureObserver, RecordingObserver, SharedBody, SharedUsage, UsageObserver,
};
use super::recording::CallRecordBuilder;
use super::usage::parse_usage_any_json;
use crate::db::HttpSnapshot;
use crate::provider::Endpoint;
use crate::server::AppState;
use crate::util::initiator::{classify_initiator, classify_initiator_responses};
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use llm_convert::sse::SsePipeline;
use serde_json::Value;
use std::time::Instant;

pub(crate) fn is_sse_response(headers: &HeaderMap) -> bool {
  headers
    .get(CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.split(';').next())
    .map(str::trim)
    .is_some_and(|value| value.eq_ignore_ascii_case("text/event-stream"))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_passthrough_call(
  s: &AppState,
  host: &str,
  method: &Method,
  path_and_query: &str,
  req_headers: &reqwest::header::HeaderMap,
  req_body: &Bytes,
  outbound_req_headers: &reqwest::header::HeaderMap,
  resp_headers: &reqwest::header::HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
) {
  let Some(db) = s.db.as_ref() else { return };
  let (prompt_tokens, completion_tokens) = parse_usage_any_json(resp_body);
  let record = passthrough_record_builder(
    db.body_max_bytes(),
    host,
    method,
    path_and_query,
    req_headers,
    req_body,
    outbound_req_headers,
    resp_headers,
    resp_body,
    status,
    started,
    None,
  )
  .with_usage(prompt_tokens, completion_tokens)
  .build();
  db.record(record);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn passthrough_streaming_response(
  state: AppState,
  host: String,
  method: Method,
  path_and_query: String,
  req_headers: HeaderMap,
  req_body: Bytes,
  outbound_req_headers: HeaderMap,
  resp: reqwest::Response,
  started: Instant,
) -> Response {
  let status = resp.status();
  let headers = resp.headers().clone();
  let max_body = state.db.as_ref().map(|db| db.body_max_bytes()).unwrap_or(0);
  let usage = SharedUsage::new();
  let body = SharedBody::new();
  let record_headers = headers.clone();
  let base_builder = passthrough_record_builder(
    max_body,
    &host,
    &method,
    &path_and_query,
    &req_headers,
    &req_body,
    &outbound_req_headers,
    &record_headers,
    &Bytes::new(),
    status.as_u16(),
    started,
    None,
  );
  let reporter = std::sync::Arc::new(PassthroughReporter { state: state.clone() });
  let recorder = RecordingObserver::new(
    reporter,
    usage.clone(),
    body.clone(),
    move |usage, captured, request_error| {
      build_stream_record(
        base_builder.with_request_error(request_error),
        usage,
        captured,
        &record_headers,
      )
    },
  );

  response_with_body(
    status,
    &headers,
    Body::from_stream(
      SsePipeline::from_response(resp)
        .with_observer(UsageObserver::new(usage))
        .with_observer(BodyCaptureObserver::new(max_body, body))
        .with_observer(recorder)
        .run(),
    ),
  )
}

#[allow(clippy::too_many_arguments)]
fn passthrough_record_builder(
  max_body: usize,
  host: &str,
  method: &Method,
  path_and_query: &str,
  req_headers: &reqwest::header::HeaderMap,
  req_body: &Bytes,
  outbound_req_headers: &reqwest::header::HeaderMap,
  resp_headers: &reqwest::header::HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
  request_error: Option<&str>,
) -> CallRecordBuilder {
  let path = path_and_query.split('?').next().unwrap_or(path_and_query);
  let logical_path = crate::proxy::rewrite_target(host, path, method).unwrap_or(path);
  let endpoint = passthrough_endpoint(method, logical_path);
  let req_body_json = serde_json::from_slice::<Value>(req_body).unwrap_or(Value::Null);
  let model = req_body_json
    .get("model")
    .and_then(|v| v.as_str())
    .unwrap_or("unknown")
    .to_string();
  let stream = req_body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  let initiator = classify_passthrough_initiator(req_headers, endpoint, &req_body_json).to_string();

  CallRecordBuilder::for_path(
    max_body,
    "passthrough",
    host,
    endpoint.map(Endpoint::as_str).unwrap_or(logical_path),
    endpoint,
    &model,
    &initiator,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: resp_headers.clone(),
      body: resp_body.clone(),
    },
    started,
    status,
    stream,
  )
  .with_ids(
    crate::server::first_header(req_headers, crate::server::SESSION_ID_HEADERS),
    crate::server::first_header(req_headers, crate::server::REQUEST_ID_HEADERS),
    request_error,
    crate::server::first_header(req_headers, crate::server::PROJECT_ID_HEADERS),
  )
  .with_request_snapshot(
    req_body.clone(),
    Some(HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: req_headers.clone(),
      body: req_body.clone(),
    }),
  )
  .with_outbound_request(Some(HttpSnapshot {
    method: Some(method.to_string()),
    url: Some(format!("https://{host}{path_and_query}")),
    status: None,
    headers: outbound_req_headers.clone(),
    body: req_body.clone(),
  }))
  .with_response_snapshot(Some(HttpSnapshot {
    method: None,
    url: None,
    status: Some(status),
    headers: resp_headers.clone(),
    body: resp_body.clone(),
  }))
}

fn response_with_body(status: reqwest::StatusCode, headers: &HeaderMap, body: Body) -> Response {
  let mut builder = Response::builder().status(status);
  for (name, value) in headers {
    builder = builder.header(name, value);
  }
  builder.body(body).unwrap_or_else(|_| Response::new(Body::empty()))
}

struct PassthroughReporter {
  state: AppState,
}

impl llm_core::pipeline::RequestReporter for PassthroughReporter {
  fn report(&self, record: crate::db::CallRecord) {
    if let Some(db) = self.state.db.as_ref() {
      db.record(record);
    }
  }
}

fn passthrough_endpoint(method: &Method, path: &str) -> Option<Endpoint> {
  match (method, path) {
    (&Method::POST, "/v1/chat/completions") => Some(Endpoint::ChatCompletions),
    (&Method::POST, "/v1/responses") => Some(Endpoint::Responses),
    (&Method::POST, "/v1/messages") => Some(Endpoint::Messages),
    _ => None,
  }
}

fn classify_passthrough_initiator(
  headers: &reqwest::header::HeaderMap,
  endpoint: Option<Endpoint>,
  body: &Value,
) -> &'static str {
  if let Some(value) = headers.get("x-initiator").and_then(|v| v.to_str().ok()) {
    match value.trim().to_ascii_lowercase().as_str() {
      "user" => return "user",
      "agent" => return "agent",
      _ => {}
    }
  }
  match endpoint {
    Some(Endpoint::Responses) => classify_initiator_responses(body),
    Some(Endpoint::ChatCompletions | Endpoint::Messages) => classify_initiator(body),
    None => "user",
  }
}
