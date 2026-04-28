//! `POST /v1/responses` — OpenAI Responses API passthrough.
//!
//! No translation: the inbound JSON body is forwarded verbatim to whichever
//! provider in the pool natively speaks `/responses` for the requested model
//! (today: GitHub Copilot for the gpt-5 / o-series families). If no
//! configured account supports the endpoint, the dispatcher returns 501.

use super::dispatch::{dispatch, DispatchOk};
use super::error::ApiError;
use super::forward::{buffered_response, stream_response};
use super::AppState;
use crate::provider::github_copilot::headers::classify_initiator_responses;
use crate::provider::{Endpoint, RequestCtx};
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

  let body = Arc::new(body);
  let inbound = Arc::new(inbound);
  let initiator_arc = Arc::new(initiator.clone());

  let DispatchOk { acct, resp } = {
    let s_for_closure = s.clone();
    dispatch(&s, &model, Endpoint::Responses, move |acct| {
      let body = body.clone();
      let inbound = inbound.clone();
      let initiator_arc = initiator_arc.clone();
      let behave_as = behave_as_inbound.clone();
      let http = s_for_closure.http.clone();
      async move {
        let ctx = RequestCtx {
          endpoint: Endpoint::Responses,
          http: &http,
          body: &body,
          stream,
          initiator: initiator_arc.as_str(),
          inbound_headers: &inbound,
          behave_as: behave_as.as_deref().map(|s| s.as_str()),
        };
        acct.provider.responses(ctx).await
      }
    })
    .await?
  };

  if stream {
    Ok(stream_response(s.clone(), acct, resp, model, initiator, started).await)
  } else {
    Ok(buffered_response(s.clone(), acct, resp, model, initiator, started).await)
  }
}
