use super::context::ForwardContext;
use super::observers::{spawn_stream_recorder, StreamMeta};
use super::recording::CompletedEventBuilder;
use super::usage::parse_usage_any_json;
use crate::db::HttpSnapshot;
use crate::provider::Endpoint;
use crate::api::codec::maybe_compress_buffered_response;
use crate::api::AppState;
use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use bytes::Bytes;
use llm_convert::sse::SsePipeline;
use serde_json::Value;

pub(crate) fn is_sse_response(headers: &HeaderMap, fallback_stream: bool) -> bool {
  match headers
    .get(CONTENT_TYPE)
    .and_then(|value| value.to_str().ok())
    .and_then(|value| value.split(';').next())
    .map(str::trim)
  {
    Some(value) => value.eq_ignore_ascii_case("text/event-stream"),
    None => fallback_stream,
  }
}

/// Handle a non-streaming passthrough response. Reads the body, emits
/// `RequestResult` and the terminal `RequestCompleted`, and returns the forwarded response.
/// Caller is responsible for emitting `RequestStarted` and `RequestParsed`.
pub(crate) async fn passthrough_buffered_response(
  s: &AppState,
  ctx: &ForwardContext,
  req_body: &Value,
  resp: reqwest::Response,
) -> Response {
  let status = resp.status();
  let resp_headers = resp.headers().clone();
  let resp_body = resp.bytes().await.unwrap_or_default();
  let mut downstream_headers = resp_headers.clone();

  let usage = parse_usage_any_json(&resp_body);

  let event = CompletedEventBuilder::new(
    s.body_max_bytes,
    ctx.request_id.clone(),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: resp_headers.clone(),
      body: resp_body.clone(),
    },
    ctx.started,
    status.as_u16(),
  )
  .with_ids(ctx.session_id.as_deref(), None)
  .with_attempt(ctx.attempt)
  .with_request_body(req_body, ctx.endpoint)
  .with_outbound_response(Some(&resp_headers), Some(&resp_body))
  .with_usage(usage)
  .build();
  s.events.emit(event);

  // Passthrough is single-attempt; emit the terminal RequestCompleted here.
  s.events.emit(llm_core::event::Event::RequestCompleted {
    request_id: ctx.request_id.clone(),
    success: status.is_success(),
    total_attempts: ctx.attempt + 1,
    final_status: Some(status.as_u16()),
    total_latency_ms: ctx.started.elapsed().as_millis() as u64,
    error: None,
  });

  let response_body =
    maybe_compress_buffered_response(&ctx.downstream_headers, &mut downstream_headers, resp_body.clone())
      .unwrap_or(resp_body);

  response_with_body(status, &downstream_headers, Body::from(response_body))
}

/// Wrap a streaming passthrough response with SSE recording.
/// Emits `RequestResult` and the terminal `RequestCompleted` (via background recorder).
/// Caller is responsible for emitting `RequestStarted` and `RequestParsed`.
pub(crate) fn passthrough_streaming_response(
  state: AppState,
  ctx: ForwardContext,
  req_body: &Value,
  resp: reqwest::Response,
) -> Response {
  let status = resp.status();
  let headers = resp.headers().clone();
  let max_body = state.body_max_bytes;
  let record_headers = headers.clone();

  let endpoint_str = ctx.endpoint.map(|e| e.as_str()).unwrap_or("unknown").to_string();

  let builder = CompletedEventBuilder::new(
    max_body,
    ctx.request_id.clone(),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: record_headers.clone(),
      body: Bytes::new(),
    },
    ctx.started,
    status.as_u16(),
  )
  .with_ids(ctx.session_id.as_deref(), None)
  .with_attempt(ctx.attempt)
  .with_request_body(req_body, ctx.endpoint);

  let meta = StreamMeta {
    request_id: ctx.request_id,
    attempt: ctx.attempt,
    final_status: status.as_u16(),
    started: ctx.started,
    model: ctx.model,
    endpoint: endpoint_str,
    events: state.events.clone(),
  };
  let tx = spawn_stream_recorder(builder, record_headers, state.events.clone(), max_body, meta);

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

pub(crate) fn passthrough_endpoint(method: &Method, path: &str) -> Option<Endpoint> {
  match (method, path) {
    (&Method::POST, "/v1/chat/completions") => Some(Endpoint::ChatCompletions),
    (&Method::POST, "/v1/responses") => Some(Endpoint::Responses),
    (&Method::POST, "/v1/messages") => Some(Endpoint::Messages),
    _ => None,
  }
}
