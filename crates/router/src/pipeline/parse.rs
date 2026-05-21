use crate::api::{first_header, PROJECT_ID_HEADERS, REQUEST_ID_HEADERS, SESSION_ID_HEADERS};
use crate::provider::Endpoint;
use axum::http::header::ACCEPT;
use axum::http::HeaderMap;
use serde_json::Value;
use tokn_core::pipeline::{ParsedRequest, RequestMeta};

#[derive(Clone, Debug)]
pub(crate) struct HeaderExtract {
  pub request_id: String,
  pub route_mode_hint: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct BodyExtract {
  pub model: String,
  pub stream: bool,
  pub initiator: String,
  pub header_initiator: Option<String>,
}

pub(crate) fn request_header_extract(headers: &HeaderMap) -> HeaderExtract {
  let request_id = first_header(headers, REQUEST_ID_HEADERS)
    .map(str::to_string)
    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
  let route_mode_hint = headers
    .get("x-route-mode")
    .and_then(|v| v.to_str().ok())
    .map(str::trim)
    .filter(|v| !v.is_empty())
    .map(str::to_string);
  HeaderExtract {
    request_id,
    route_mode_hint,
  }
}

pub(crate) fn request_body_extract(headers: &HeaderMap, body: &Value) -> BodyExtract {
  let header_initiator = headers
    .get("x-initiator")
    .and_then(|v| v.to_str().ok())
    .map(|v| v.trim().to_ascii_lowercase())
    .filter(|v| v == "user" || v == "agent");
  let initiator = header_initiator
    .clone()
    .unwrap_or_else(|| classify_initiator(body).to_string());
  BodyExtract {
    model: body
      .get("model")
      .and_then(|v| v.as_str())
      .unwrap_or("unknown")
      .to_string(),
    stream: infer_stream_request(headers, body),
    initiator,
    header_initiator,
  }
}

pub(crate) fn infer_stream_request(headers: &HeaderMap, body: &Value) -> bool {
  if let Some(stream) = body.get("stream").and_then(|v| v.as_bool()) {
    return stream;
  }
  headers
    .get(ACCEPT)
    .and_then(|v| v.to_str().ok())
    .map(|v| {
      v.split(',')
        .any(|part| part.split(';').next().map(str::trim) == Some("text/event-stream"))
    })
    .unwrap_or(false)
}

pub(crate) trait RequestParser: Send + Sync {
  fn endpoint(&self) -> Endpoint;

  fn parse(&self, headers: HeaderMap, body: Value) -> ParsedRequest {
    let body_meta = request_body_extract(&headers, &body);
    let session_id = first_header(&headers, SESSION_ID_HEADERS).map(str::to_string);
    let request_id = first_header(&headers, REQUEST_ID_HEADERS).map(str::to_string);
    let project_id = first_header(&headers, PROJECT_ID_HEADERS).map(str::to_string);

    ParsedRequest {
      meta: RequestMeta {
        endpoint: self.endpoint(),
        upstream_endpoint: self.endpoint(),
        model: body_meta.model.clone(),
        upstream_model: body_meta.model,
        stream: body_meta.stream,
        session_id,
        request_id,
        attempt: 0,
        project_id,
        initiator: body_meta.initiator,
        header_initiator: body_meta.header_initiator,
        inbound_headers: (&headers).into(),
      },
      body,
    }
  }
}

pub(crate) struct ChatParser;
pub(crate) struct ResponsesParser;
pub(crate) struct MessagesParser;

impl RequestParser for ChatParser {
  fn endpoint(&self) -> Endpoint {
    Endpoint::ChatCompletions
  }
}

impl RequestParser for ResponsesParser {
  fn endpoint(&self) -> Endpoint {
    Endpoint::Responses
  }
}

impl RequestParser for MessagesParser {
  fn endpoint(&self) -> Endpoint {
    Endpoint::Messages
  }
}

fn classify_initiator(body: &Value) -> &'static str {
  if body.get("input").is_some() {
    crate::util::initiator::classify_initiator_responses(body)
  } else {
    crate::util::initiator::classify_initiator(body)
  }
}
