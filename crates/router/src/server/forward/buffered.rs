use super::context::ForwardContext;
use super::recording::CompletedEventBuilder;
use super::usage::parse_usage_any_json;
use crate::db::HttpSnapshot;
use crate::server::error::ApiError;
use crate::server::AppState;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;

pub(crate) async fn buffered_response(
  s: AppState,
  resp: reqwest::Response,
  ctx: ForwardContext,
  req_body: &Value,
) -> Response {
  let status = resp.status();
  let resp_headers = resp.headers().clone();
  let mut request_error = None;
  let bytes = match resp.bytes().await {
    Ok(b) => b,
    Err(e) => {
      request_error = Some(format!("reading upstream body: {e}"));
      Bytes::new()
    }
  };

  let mut headers = HeaderMap::new();
  headers.insert(
    axum::http::header::CONTENT_TYPE,
    HeaderValue::from_static("application/json"),
  );
  if let Some(id) = ctx.session_id.as_deref() {
    if let Ok(value) = HeaderValue::from_str(id) {
      headers.insert(crate::server::SESSION_ID_HEADER, value);
    }
  }

  let endpoint = ctx.endpoint.unwrap_or(ctx.upstream_endpoint);
  let bytes = if ctx.upstream_endpoint == endpoint {
    bytes
  } else {
    match serde_json::from_slice::<Value>(&bytes)
      .map_err(|e| e.to_string())
      .and_then(|v| crate::convert::convert_response(ctx.upstream_endpoint, endpoint, &v).map_err(|e| e.to_string()))
      .and_then(|v| serde_json::to_vec(&v).map(Bytes::from).map_err(|e| e.to_string()))
    {
      Ok(bytes) => bytes,
      Err(e) => {
        request_error = Some(format!("response conversion failed: {e}"));
        Bytes::new()
      }
    }
  };

  let usage = parse_usage_any_json(&bytes);

  let event = CompletedEventBuilder::new(
    s.body_max_bytes,
    ctx.request_id.clone(),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: headers.clone(),
      body: bytes.clone(),
    },
    ctx.started,
    status.as_u16(),
  )
  .with_ids(ctx.session_id.as_deref(), request_error.as_deref())
  .with_attempt(ctx.attempt)
  .with_request_body(req_body, ctx.endpoint)
  .with_outbound_response(Some(&resp_headers), Some(&bytes))
  .with_usage(usage)
  .build();
  s.events.emit(event);

  if let Some(error) = request_error {
    ApiError::bad_gateway(error).into_response()
  } else {
    (status, headers, bytes).into_response()
  }
}
