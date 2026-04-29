//! `POST /v1/responses` — OpenAI Responses API surface.
//!
//! Native `/responses` providers are preferred. Otherwise dispatch can route
//! through another supported chat-like endpoint and convert request/response
//! bodies via `crate::convert`.

use super::dispatch::{dispatch, DispatchOk};
use super::error::ApiError;
use super::forward::{buffered_response, stream_response};
use super::AppState;
use crate::provider::{Endpoint, RequestCtx};
use crate::util::initiator::classify_initiator_responses;
use crate::util::redact::BehaveAs;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Json;
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, instrument};

#[instrument(
  name = "responses",
  skip_all,
  fields(
    endpoint = %Endpoint::Responses,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn responses(
  State(s): State<AppState>,
  inbound: HeaderMap,
  Json(body): Json<Value>,
) -> Result<Response, ApiError> {
  let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
  let model = body
    .get("model")
    .and_then(|v| v.as_str())
    .unwrap_or("unknown")
    .to_string();
  let span = tracing::Span::current();
  span.record("model", model.as_str());
  span.record("stream", stream);
  let session_id = inbound
    .get(super::SESSION_ID_HEADER)
    .and_then(|v| v.to_str().ok())
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(str::to_string);

  // Responses-bodies use `input` not `messages`; honour an explicit
  // X-Initiator override before falling back to the input-aware classifier.
  let initiator: String = match inbound.get("x-initiator").and_then(|v| v.to_str().ok()) {
    Some(v) => {
      let lv = v.trim().to_ascii_lowercase();
      if lv == "user" || lv == "agent" {
        lv
      } else {
        classify_initiator_responses(&body).into()
      }
    }
    None => classify_initiator_responses(&body).into(),
  };

  let behave_as_inbound: Option<Arc<String>> = inbound
    .get("x-behave-as")
    .and_then(|v| v.to_str().ok())
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
    .map(Arc::new);

  span.record("initiator", initiator.as_str());
  span.record(
    "behave_as",
    tracing::field::display(BehaveAs(behave_as_inbound.as_deref().map(|s| s.as_str()))),
  );
  debug!("dispatching responses");

  let started = Instant::now();

  let req_body = body.clone();
  let req_headers = inbound.clone();
  let body = Arc::new(body);
  let inbound = Arc::new(inbound);
  let initiator_arc = Arc::new(initiator.clone());

  let DispatchOk {
    acct,
    resp,
    upstream_endpoint,
    outbound,
  } = {
    let s_for_closure = s.clone();
    dispatch(
      &s,
      session_id.as_deref(),
      &model,
      Endpoint::Responses,
      body.clone(),
      move |acct, upstream_endpoint, upstream_body, capture| {
        let inbound = inbound.clone();
        let initiator_arc = initiator_arc.clone();
        let behave_as = behave_as_inbound.clone();
        let http = s_for_closure.http.clone();
        async move {
          let ctx = RequestCtx {
            endpoint: upstream_endpoint,
            http: &http,
            body: &upstream_body,
            stream,
            initiator: initiator_arc.as_str(),
            inbound_headers: &inbound,
            behave_as: behave_as.as_deref().map(|s| s.as_str()),
            outbound: Some(capture),
          };
          match upstream_endpoint {
            Endpoint::ChatCompletions => acct.provider.chat(ctx).await,
            Endpoint::Responses => acct.provider.responses(ctx).await,
            Endpoint::Messages => acct.provider.messages(ctx).await,
          }
        }
      },
    )
    .await?
  };

  if stream {
    Ok(
      stream_response(
        s.clone(),
        acct,
        resp,
        Endpoint::Responses,
        upstream_endpoint,
        model,
        initiator,
        session_id,
        req_headers,
        req_body,
        outbound,
        started,
      )
      .await,
    )
  } else {
    Ok(
      buffered_response(
        s.clone(),
        acct,
        resp,
        Endpoint::Responses,
        upstream_endpoint,
        model,
        initiator,
        session_id,
        req_headers,
        req_body,
        outbound,
        started,
      )
      .await,
    )
  }
}
