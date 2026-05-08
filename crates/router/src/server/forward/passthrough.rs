use super::observers::{background_stream_recorder, StreamMeta};
use super::recording::CompletedEventBuilder;
use super::usage::parse_usage_any_json;
use crate::db::HttpSnapshot;
use crate::provider::Endpoint;
use crate::server::AppState;
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use llm_convert::sse::{observer_channel, SsePipeline};
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
  _outbound_req_headers: &reqwest::header::HeaderMap,
  resp_headers: &reqwest::header::HeaderMap,
  resp_body: &Bytes,
  status: u16,
  started: Instant,
) {
  let (prompt_tokens, completion_tokens) = parse_usage_any_json(resp_body);
  let path = path_and_query.split('?').next().unwrap_or(path_and_query);
  let endpoint = passthrough_endpoint(method, path);
  let req_body_json = serde_json::from_slice::<Value>(req_body).unwrap_or(Value::Null);
  let model = req_body_json.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
  let stream = req_body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  let request_id = crate::server::first_header(req_headers, crate::server::REQUEST_ID_HEADERS)
    .map(|s| s.to_string())
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
  let session_id = crate::server::first_header(req_headers, crate::server::SESSION_ID_HEADERS).map(|s| s.to_string());
  let project_id = crate::server::first_header(req_headers, crate::server::PROJECT_ID_HEADERS).map(|s| s.to_string());

  // Emit full lifecycle
  s.events.emit(llm_core::event::Event::RequestStarted {
    request_id: request_id.clone(),
    ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64,
    endpoint: endpoint.map(|e| e.as_str()).unwrap_or(path).to_string(),
    model: model.clone(),
    initiator: "user".to_string(),
    stream,
    session_id: session_id.clone(),
    project_id: project_id.clone(),
    inbound_req: HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: req_headers.clone(),
      body: req_body.clone(),
    },
  });
  s.events.emit(llm_core::event::Event::RequestParsed {
    request_id: request_id.clone(),
    account_id: "passthrough".to_string(),
    provider_id: host.to_string(),
    outbound_req: Some(HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: req_headers.clone(),
      body: req_body.clone(),
    }),
  });

  let event = CompletedEventBuilder::new(
    s.body_max_bytes,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: resp_headers.clone(),
      body: resp_body.clone(),
    },
    started,
    status,
  )
  .with_ids(session_id.as_deref(), Some(&request_id), None)
  .with_request_body(&req_body_json, endpoint)
  .with_outbound_response(Some(resp_headers), Some(resp_body))
  .with_usage(prompt_tokens, completion_tokens)
  .build();
  s.events.emit(event);
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn passthrough_streaming_response(
  state: AppState,
  host: String,
  method: Method,
  path_and_query: String,
  req_headers: HeaderMap,
  req_body: Bytes,
  _outbound_req_headers: HeaderMap,
  resp: reqwest::Response,
  started: Instant,
) -> Response {
  let status = resp.status();
  let headers = resp.headers().clone();
  let max_body = state.body_max_bytes;
  let record_headers = headers.clone();

  let path = path_and_query.split('?').next().unwrap_or(&path_and_query);
  let endpoint = passthrough_endpoint(&method, path);
  let req_body_json = serde_json::from_slice::<Value>(&req_body).unwrap_or(Value::Null);
  let model = req_body_json.get("model").and_then(|v| v.as_str()).unwrap_or("unknown").to_string();
  let stream_flag = req_body_json.get("stream").and_then(|v| v.as_bool()).unwrap_or(true);
  let request_id = crate::server::first_header(&req_headers, crate::server::REQUEST_ID_HEADERS)
    .map(|s| s.to_string())
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
  let session_id = crate::server::first_header(&req_headers, crate::server::SESSION_ID_HEADERS).map(|s| s.to_string());

  // Emit lifecycle events
  state.events.emit(llm_core::event::Event::RequestStarted {
    request_id: request_id.clone(),
    ts: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs() as i64,
    endpoint: endpoint.map(|e| e.as_str()).unwrap_or(path).to_string(),
    model: model.clone(),
    initiator: "user".to_string(),
    stream: stream_flag,
    session_id: session_id.clone(),
    project_id: crate::server::first_header(&req_headers, crate::server::PROJECT_ID_HEADERS).map(|s| s.to_string()),
    inbound_req: HttpSnapshot {
      method: Some(method.to_string()),
      url: Some(format!("https://{host}{path_and_query}")),
      status: None,
      headers: req_headers.clone(),
      body: req_body.clone(),
    },
  });
  state.events.emit(llm_core::event::Event::RequestParsed {
    request_id: request_id.clone(),
    account_id: "passthrough".to_string(),
    provider_id: host.to_string(),
    outbound_req: None,
  });

  let base_builder = CompletedEventBuilder::new(
    max_body,
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: record_headers.clone(),
      body: Bytes::new(),
    },
    started,
    status.as_u16(),
  )
  .with_ids(session_id.as_deref(), Some(&request_id), None)
  .with_request_body(&req_body_json, endpoint);

  // Extract metadata for progress events
  let progress_endpoint = endpoint
    .map(|e| e.as_str())
    .unwrap_or("unknown")
    .to_string();

  let (tx, rx) = observer_channel();
  let meta = StreamMeta {
    request_id: Some(request_id),
    model,
    endpoint: progress_endpoint,
    events: state.events.clone(),
  };
  tokio::spawn(background_stream_recorder(rx, base_builder, record_headers, state.events.clone(), max_body, meta));

  response_with_body(
    status,
    &headers,
    Body::from_stream(SsePipeline::from_response_with_tap(resp, tx).run()),
  )
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
