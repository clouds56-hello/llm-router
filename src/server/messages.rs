//! `POST /v1/messages` — Anthropic Messages API passthrough.
//!
//! Forwarded verbatim to whichever provider in the pool natively speaks
//! `/v1/messages` for the requested model (today: GitHub Copilot for the
//! Claude family). No translation in this version.

use super::dispatch::{dispatch, DispatchOk};
use super::error::ApiError;
use super::forward::{buffered_response, stream_response};
use super::AppState;
use crate::provider::github_copilot::headers::classify_initiator;
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
  name = "messages",
  skip_all,
  fields(
    endpoint = %Endpoint::Messages,
    model = tracing::field::Empty,
    stream = tracing::field::Empty,
    initiator = tracing::field::Empty,
    behave_as = tracing::field::Empty,
  ),
)]
pub async fn messages(
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

  // Anthropic body shape uses `messages: [{role, content}]`, so the
  // existing chat-style classifier walks it correctly.
  let initiator: String = match inbound.get("x-initiator").and_then(|v| v.to_str().ok()) {
    Some(v) => {
      let lv = v.trim().to_ascii_lowercase();
      if lv == "user" || lv == "agent" {
        lv
      } else {
        classify_initiator(&body).into()
      }
    }
    None => classify_initiator(&body).into(),
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
  debug!("dispatching messages");

  let started = Instant::now();

  let body = Arc::new(body);
  let inbound = Arc::new(inbound);
  let initiator_arc = Arc::new(initiator.clone());

  let DispatchOk { acct, resp } = {
    let s_for_closure = s.clone();
    dispatch(&s, &model, Endpoint::Messages, move |acct| {
      let body = body.clone();
      let inbound = inbound.clone();
      let initiator_arc = initiator_arc.clone();
      let behave_as = behave_as_inbound.clone();
      let http = s_for_closure.http.clone();
      async move {
        let ctx = RequestCtx {
          endpoint: Endpoint::Messages,
          http: &http,
          body: &body,
          stream,
          initiator: initiator_arc.as_str(),
          inbound_headers: &inbound,
          behave_as: behave_as.as_deref().map(|s| s.as_str()),
        };
        acct.provider.messages(ctx).await
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
