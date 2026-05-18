use crate::api::error::ApiError;
use crate::api::AppState;
use crate::pipeline::{build_failure_result_event, request_body_extract, request_header_extract};
use crate::relay::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response, ForwardContext};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::StatusCode;
use axum::http::{Request, Response};
use axum::response::IntoResponse;
use http::header::{HeaderValue, HOST};

pub(super) async fn proxy_passthrough(
  state: &AppState,
  http: &reqwest::Client,
  host: &str,
  peer_addr: std::net::SocketAddr,
  local_addr: std::net::SocketAddr,
  req: Request<hyper::body::Incoming>,
) -> Result<Response<Body>> {
  let started = std::time::Instant::now();
  let ts = std::time::SystemTime::now()
    .duration_since(std::time::UNIX_EPOCH)
    .unwrap_or_default()
    .as_secs() as i64;
  let path_and_query = req
    .uri()
    .path_and_query()
    .map(|v| v.as_str().to_string())
    .unwrap_or_else(|| "/".to_string());
  let url = format!("https://{host}{path_and_query}");
  let (parts, body) = req.into_parts();
  let hx = request_header_extract(&parts.headers);
  let request_id = hx.request_id.clone();
  let path = path_and_query.split('?').next().unwrap_or(&path_and_query);
  let mode = state
    .route
    .resolve_mode(hx.route_mode_hint.as_deref())
    .ok()
    .map(llm_accounts::routing::route_mode_as_str)
    .map(str::to_string);
  state.events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Started {
      request_id: request_id.clone(),
      ts,
      endpoint: path.to_string(),
      session_id: hx.session_id.clone(),
      peer_addr: Some(peer_addr.to_string()),
      local_addr: Some(local_addr.to_string()),
      method: parts.method.to_string(),
      url: Some(url.clone()),
    },
  ));
  state.events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Headers {
      request_id: request_id.clone(),
      ts,
      endpoint_hint: None,
      path: Some(path.to_string()),
      session_id: hx.session_id.clone(),
      project_id: hx.project_id.clone(),
      header_initiator: hx.header_initiator.clone(),
      local_addr: Some(local_addr.to_string()),
      mode,
      route_mode_hint: hx.route_mode_hint.clone(),
      inbound_headers: (&parts.headers).into(),
    },
  ));

  let request_body = axum::body::to_bytes(Body::new(body), usize::MAX)
    .await
    .context("read passthrough request body")?;
  let (req_body_json, inbound_decoded_body) = if request_body.is_empty() {
    (serde_json::Value::Null, bytes::Bytes::new())
  } else {
    match crate::api::codec::decode_json_request(&parts.headers, request_body.clone()) {
      Ok(decoded) => (decoded.value, decoded.decoded_body),
      Err(err) => {
        let status = err.status();
        let msg = err.to_string();
        // Synthesise terminal RequestResult so the DB row records the JSON
        // envelope returned to the client even when we never reached upstream.
        state.events.emit(build_failure_result_event(
          request_id.clone(),
          0,
          started,
          status,
          msg.clone(),
          None,
        ));
        state.events.emit(llm_core::event::Event::LegacyRequest(
          llm_core::event::LegacyRequestEvent::Completed {
            request_id: request_id.clone(),
            success: false,
            total_attempts: 1,
            final_status: Some(status.as_u16()),
            total_latency_ms: started.elapsed().as_millis() as u64,
            error: Some(msg),
          },
        ));
        return Ok(err.into_response());
      }
    }
  };

  let mut upstream = http.request(parts.method.clone(), &url).body(request_body.clone());
  let mut outbound_req_headers = reqwest::header::HeaderMap::new();
  for (name, value) in &parts.headers {
    if name != HOST && !crate::api::is_router_owned_header(name) {
      upstream = upstream.header(name, value);
      outbound_req_headers.insert(name.clone(), value.clone());
    }
  }
  upstream = upstream.header(HOST, host);
  outbound_req_headers.insert(
    HOST,
    HeaderValue::from_str(host).unwrap_or_else(|_| HeaderValue::from_static("localhost")),
  );

  let body_meta = request_body_extract(&parts.headers, &req_body_json);
  let ctx = ForwardContext::from_passthrough(&parts.method, path, &hx, &body_meta, parts.headers.clone(), started);
  let identity = state.identity.resolve(&parts.headers, &url, &state.provider_registry);
  let mut completion =
    crate::pipeline::completion::CompletionGuard::new(state.events.clone(), request_id.clone(), started);
  let stream = body_meta.stream;
  state.events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Parsed {
      request_id: request_id.clone(),
      attempt: ctx.attempt,
      account_id: identity.account_id.unwrap_or_else(|| "<unknown>".to_string()),
      provider_id: identity.provider_id.unwrap_or_else(|| host.to_string()),
      model: body_meta.model.clone(),
      stream,
      initiator: body_meta.initiator,
      behave_as: None,
      inbound_body: inbound_decoded_body.clone(),
    },
  ));

  let response = match upstream.send().await {
    Ok(response) => response,
    Err(err) => {
      let message = crate::api::error::sanitized_transport_failure_message(host, &err);
      // Synthesise terminal RequestResult: no upstream response was received,
      // so outbound_resp_body is None. RequestResponded is intentionally not
      // emitted (we never got upstream headers/status).
      state.events.emit(build_failure_result_event(
        request_id.clone(),
        ctx.attempt,
        started,
        StatusCode::BAD_GATEWAY,
        message.clone(),
        None,
      ));
      completion.failure(Some(StatusCode::BAD_GATEWAY.as_u16()), message.clone());
      return Ok(ApiError::bad_gateway(message).into_response());
    }
  };
  let status = response.status();
  state.events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Responded {
      request_id: request_id.clone(),
      attempt: ctx.attempt,
      outbound_status: status.as_u16(),
      latency_ms: started.elapsed().as_millis() as u64,
      outbound_resp_headers: response.headers().into(),
      outbound_req_method: Some(parts.method.to_string()),
      outbound_req_url: Some(url.clone()),
      outbound_req_headers: Some((&outbound_req_headers).into()),
      outbound_req_body: Some(request_body.clone()),
    },
  ));

  if is_sse_response(response.headers(), stream) {
    completion.disarm();
    let resp = passthrough_streaming_response(state.clone(), ctx, &req_body_json, response);
    return Ok(resp);
  }

  let resp = passthrough_buffered_response(state, &ctx, &req_body_json, response).await;
  completion.disarm();
  Ok(resp)
}

#[cfg(test)]
mod tests {
  use super::*;
  use axum::body::to_bytes;

  #[tokio::test]
  async fn transport_failure_response_uses_sanitized_message() {
    let err = reqwest::Client::new()
      .get("http://[::1]:1/backend-api/codex/responses?secret=1")
      .send()
      .await
      .unwrap_err();

    let resp = ApiError::bad_gateway(crate::api::error::sanitized_transport_failure_message(
      "chatgpt.com",
      &err,
    ))
    .into_response();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let message = json["error"]["message"].as_str().unwrap();
    assert!(!message.is_empty());
    assert!(message.contains("chatgpt.com"));
    assert!(!message.contains("/backend-api/codex/responses"));
    assert!(!message.contains("?secret=1"));
    assert!(!message.contains("http://[::1]"));
  }
}
