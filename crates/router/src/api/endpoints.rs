use super::error::ApiError;
use super::AppState;
use crate::pipeline::{
  handle_endpoint, request_header_extract, ChatParser, MessagesParser, RequestParser, ResponsesParser,
};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use llm_core::event::Event as CoreEvent;
use llm_accounts::routing::route_mode_as_str;
use llm_core::provider::Endpoint;
use llm_core::request_event::{RecordEvent, RequestEvent, RequestEventPayload};
use smol_str::SmolStr;
use std::time::Instant;
use tracing::instrument;

async fn handle(
  state: AppState,
  parser: &dyn RequestParser,
  inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  let started = Instant::now();
  let ts = unix_ts();
  let hx = request_header_extract(&inbound);
  let endpoint_hint = parser.endpoint().as_str().to_string();
  let local_addr = inbound
    .get("x-llm-router-local-addr")
    .and_then(|v| v.to_str().ok())
    .map(str::to_string)
    .or_else(|| {
      inbound
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
    });
  // POC fast-path: when the env-var-gated chat pipeline is configured and
  // this request targets /chat/completions, route through llm-requests and
  // skip ALL legacy event emissions (Started/Headers/Completed). The
  // pipeline emits its own StageEvent/RecordEvent stream which
  // RequestEventHandler consumes; emitting LegacyRequestEvent::Started here
  // would cause DbEventHandler to insert a row first, then
  // RequestEventHandler's INSERT on StageEvent::Started would hit a UNIQUE
  // constraint and orphan every subsequent stage UPDATE.
  let use_pipeline = parser.endpoint() == Endpoint::ChatCompletions && state.chat_pipeline.is_some();
  let mode = state.route.resolve_mode(hx.route_mode_hint.as_deref()).ok();

  if use_pipeline {
    state.events.emit(CoreEvent::Requests(RequestEvent {
      request_id: SmolStr::new(&hx.request_id),
      attempt: 0,
      ts: llm_core::util::now_unix_ms(),
      payload: RequestEventPayload::Record(RecordEvent::InboundConnection {
        local_addr: local_addr.clone().map(SmolStr::from),
        peer_addr: None,
        mode: SmolStr::new(request_record_mode(mode)),
        method: SmolStr::new("requests"),
        inbound_method: SmolStr::new("POST"),
        url: None,
      }),
    }));
    let decoded = super::codec::decode_json_request(&inbound, body)?;
    let pipeline = state.chat_pipeline.clone().expect("checked is_some above");
    let raw = llm_requests::RawInbound {
      endpoint: Endpoint::ChatCompletions,
      headers: (&inbound).into(),
      raw_body: decoded.raw_body.clone(),
      decoded_body: decoded.decoded_body.clone(),
      body_json: decoded.value.clone(),
      request_id: Some(SmolStr::new(&hx.request_id)),
    };
    return match pipeline.run(raw).await {
      Ok(converted) => Ok(super::response::converted_to_axum(converted)),
      Err(err) => Err(ApiError::bad_gateway(err.to_string())),
    };
  }

  let mode = mode
    .map(route_mode_as_str)
    .map(str::to_string);

  state.events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Started {
      request_id: hx.request_id.clone(),
      ts,
      endpoint: endpoint_hint.clone(),
      session_id: hx.session_id.clone(),
      peer_addr: None,
      local_addr: local_addr.clone(),
      method: "requests".to_string(),
      inbound_method: "POST".to_string(),
      url: None,
    },
  ));
  state.events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Headers {
      request_id: hx.request_id.clone(),
      ts,
      endpoint_hint: Some(endpoint_hint),
      path: None,
      session_id: hx.session_id.clone(),
      project_id: hx.project_id.clone(),
      header_initiator: hx.header_initiator.clone(),
      local_addr,
      mode,
      route_mode_hint: hx.route_mode_hint.clone(),
      inbound_headers: (&inbound).into(),
    },
  ));

  let decoded = match super::codec::decode_json_request(&inbound, body) {
    Ok(decoded) => decoded,
    Err(err) => {
      state.events.emit(llm_core::event::Event::LegacyRequest(
        llm_core::event::LegacyRequestEvent::Completed {
          request_id: hx.request_id.clone(),
          success: false,
          total_attempts: 1,
          final_status: Some(err.status().as_u16()),
          total_latency_ms: started.elapsed().as_millis() as u64,
          error: Some(err.to_string()),
        },
      ));
      return Err(err);
    }
  };
  let parsed = parser.parse(inbound, decoded.value.clone());
  handle_endpoint(state, parsed, decoded, hx.request_id, started).await
}

fn unix_ts() -> i64 {
  std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs() as i64
}

fn request_record_mode(mode: Option<llm_config::RouteMode>) -> &'static str {
  match mode {
    Some(mode) => route_mode_as_str(mode),
    None => "route",
  }
}

/// Inject route mode from path prefix into headers, overriding any existing value.
fn inject_mode(mode: &str, headers: &mut HeaderMap) -> Result<(), ApiError> {
  super::validate_path_mode(mode)?;
  headers.insert(
    axum::http::HeaderName::from_static("x-route-mode"),
    axum::http::HeaderValue::from_str(mode).unwrap(),
  );
  Ok(())
}

#[instrument(
  name = "chat_completions",
  skip_all,
  fields(
    endpoint = %crate::provider::Endpoint::ChatCompletions,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn chat_completions(
  State(state): State<AppState>,
  inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  handle(state, &ChatParser, inbound, body).await
}

#[instrument(
  name = "responses",
  skip_all,
  fields(
    endpoint = %crate::provider::Endpoint::Responses,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn responses(State(state): State<AppState>, inbound: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
  handle(state, &ResponsesParser, inbound, body).await
}

#[instrument(
  name = "messages",
  skip_all,
  fields(
    endpoint = %crate::provider::Endpoint::Messages,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn messages(State(state): State<AppState>, inbound: HeaderMap, body: Bytes) -> Result<Response, ApiError> {
  handle(state, &MessagesParser, inbound, body).await
}

// --- Mode-prefixed variants ---

pub async fn chat_completions_with_mode(
  State(state): State<AppState>,
  Path(mode): Path<String>,
  mut inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  inject_mode(&mode, &mut inbound)?;
  handle(state, &ChatParser, inbound, body).await
}

pub async fn responses_with_mode(
  State(state): State<AppState>,
  Path(mode): Path<String>,
  mut inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  inject_mode(&mode, &mut inbound)?;
  handle(state, &ResponsesParser, inbound, body).await
}

pub async fn messages_with_mode(
  State(state): State<AppState>,
  Path(mode): Path<String>,
  mut inbound: HeaderMap,
  body: Bytes,
) -> Result<Response, ApiError> {
  inject_mode(&mode, &mut inbound)?;
  handle(state, &MessagesParser, inbound, body).await
}
