use super::recording::CallRecordBuilder;
use super::usage::parse_usage_any_json;
use crate::db::CallRecord;
use crate::db::OutboundSnapshot;
use crate::pool::AccountHandle;
use crate::provider::Endpoint;
use crate::server::error::ApiError;
use crate::server::AppState;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;

#[allow(clippy::too_many_arguments)]
pub(crate) async fn buffered_response(
  s: AppState,
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
) -> (Response, CallRecord) {
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
  if let Some(id) = session_id.as_deref() {
    if let Ok(value) = HeaderValue::from_str(id) {
      headers.insert(crate::server::SESSION_ID_HEADER, value);
    }
  }

  let bytes = if upstream_endpoint == endpoint {
    bytes
  } else {
    match serde_json::from_slice::<Value>(&bytes)
      .map_err(|e| e.to_string())
      .and_then(|v| crate::convert::convert_response(upstream_endpoint, endpoint, &v).map_err(|e| e.to_string()))
      .and_then(|v| serde_json::to_vec(&v).map(Bytes::from).map_err(|e| e.to_string()))
    {
      Ok(bytes) => bytes,
      Err(e) => {
        request_error = Some(format!("response conversion failed: {e}"));
        Bytes::new()
      }
    }
  };

  let (pt, ct) = parse_usage_any_json(&bytes);
  let record = CallRecordBuilder::for_endpoint(
    s.db.as_ref().map(|db| db.body_max_bytes()).unwrap_or(0),
    &acct.id(),
    acct.provider.info().id.as_str(),
    endpoint,
    &model,
    &initiator,
    crate::db::HttpSnapshot {
      method: None,
      url: None,
      status: Some(status.as_u16()),
      headers: headers.clone(),
      body: bytes.clone(),
    },
    started,
    status.as_u16(),
    false,
  )
  .with_ids(
    session_id.as_deref(),
    request_id.as_deref(),
    request_error.as_deref(),
    project_id.as_deref(),
  )
  .with_request_json(&req_headers, &req_body)
  .with_outbound_request(outbound)
  .with_outbound_response(Some(&resp_headers), Some(&bytes))
  .with_usage(pt, ct)
  .build();

  let response = if let Some(error) = request_error {
    ApiError::bad_gateway(error).into_response()
  } else {
    (status, headers, bytes).into_response()
  };

  (response, record)
}
