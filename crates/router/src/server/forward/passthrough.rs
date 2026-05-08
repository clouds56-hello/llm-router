use super::observers::{background_stream_recorder, StreamMeta};
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
use llm_convert::sse::{observer_channel, SsePipeline};
use serde_json::Value;
use std::sync::Arc;
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
  let (prompt_tokens, completion_tokens) = parse_usage_any_json(resp_body);
  let record = passthrough_record_builder(
    s.body_max_bytes,
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
  s.events.emit(llm_core::event::Event::RequestCompleted { record });
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
  let max_body = state.body_max_bytes;
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

  // Extract metadata for progress events
  let req_body_json = serde_json::from_slice::<Value>(&req_body).unwrap_or(Value::Null);
  let progress_model = req_body_json.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
  let path = path_and_query.split('?').next().unwrap_or(&path_and_query);
  let progress_endpoint = passthrough_endpoint(&method, path)
    .map(|e| e.as_str())
    .unwrap_or("unknown")
    .to_string();

  let (tx, rx) = observer_channel();
  let reporter: Arc<dyn llm_core::pipeline::RequestReporter> =
    Arc::new(EventBusReporter(state.events.clone()));
  let meta = StreamMeta {
    request_id: crate::server::first_header(&req_headers, crate::server::REQUEST_ID_HEADERS).map(|s| s.to_string()),
    model: progress_model,
    endpoint: progress_endpoint,
    events: state.events.clone(),
  };
  tokio::spawn(background_stream_recorder(rx, base_builder, record_headers, reporter, max_body, meta));

  response_with_body(
    status,
    &headers,
    Body::from_stream(SsePipeline::from_response_with_tap(resp, tx).run()),
  )
}

/// Reporter that emits RequestCompleted events via the EventBus.
struct EventBusReporter(Arc<llm_core::event::EventBus>);

impl llm_core::pipeline::RequestReporter for EventBusReporter {
  fn report(&self, record: crate::db::CallRecord) {
    self.0.emit(llm_core::event::Event::RequestCompleted { record });
  }
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
