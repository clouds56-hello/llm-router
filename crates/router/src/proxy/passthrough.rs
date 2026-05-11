use crate::api::AppState;
use crate::pipeline::{request_body_extract, request_header_extract};
use crate::relay::{is_sse_response, passthrough_buffered_response, passthrough_streaming_response, ForwardContext};
use anyhow::{Context, Result};
use axum::body::Body;
use axum::http::{Request, Response};
use axum::response::IntoResponse;
use http::header::{HeaderValue, HOST};

pub(super) async fn proxy_passthrough(
  state: &AppState,
  http: &reqwest::Client,
  host: &str,
  source: std::net::SocketAddr,
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
  state.events.emit(llm_core::event::Event::RequestStarted {
    request_id: request_id.clone(),
    ts,
    endpoint: path.to_string(),
    session_id: hx.session_id.clone(),
    ip: Some(source.ip().to_string()),
    port: Some(source.port()),
    method: parts.method.to_string(),
    url: Some(url.clone()),
  });
  state.events.emit(llm_core::event::Event::RequestHeaders {
    request_id: request_id.clone(),
    ts,
    endpoint_hint: None,
    path: Some(path.to_string()),
    session_id: hx.session_id.clone(),
    project_id: hx.project_id.clone(),
    header_initiator: hx.header_initiator.clone(),
    route_mode_hint: hx.route_mode_hint.clone(),
    inbound_headers: parts.headers.clone(),
  });

  let request_body = axum::body::to_bytes(Body::new(body), usize::MAX)
    .await
    .context("read passthrough request body")?;
  let req_body_json = if request_body.is_empty() {
    serde_json::Value::Null
  } else {
    match crate::api::codec::decode_json_request(&parts.headers, request_body.clone()) {
      Ok(decoded) => decoded.value,
      Err(err) => {
        state.events.emit(llm_core::event::Event::RequestCompleted {
          request_id: request_id.clone(),
          success: false,
          total_attempts: 1,
          final_status: Some(err.status().as_u16()),
          total_latency_ms: started.elapsed().as_millis() as u64,
          error: Some(err.to_string()),
        });
        return Ok(err.into_response());
      }
    }
  };

  let mut upstream = http.request(parts.method.clone(), &url).body(request_body.clone());
  let mut outbound_req_headers = parts.headers.clone();
  for (name, value) in &parts.headers {
    if name != HOST {
      upstream = upstream.header(name, value);
    }
  }
  upstream = upstream.header(HOST, host);
  outbound_req_headers.insert(
    HOST,
    HeaderValue::from_str(host).unwrap_or_else(|_| HeaderValue::from_static("localhost")),
  );

  let ctx = ForwardContext::from_passthrough(&parts.method, path, &parts.headers, &req_body_json, started);
  let mut completion = crate::pipeline::completion::CompletionGuard::new(state.events.clone(), request_id.clone(), started);
  let body_meta = request_body_extract(&parts.headers, &req_body_json);
  let stream = body_meta.stream;
  state.events.emit(llm_core::event::Event::RequestParsed {
    request_id: request_id.clone(),
    attempt: ctx.attempt,
    account_id: "passthrough".to_string(),
    provider_id: host.to_string(),
    model: body_meta.model.clone(),
    stream,
    initiator: body_meta.initiator,
    outbound_req: Some(crate::db::HttpSnapshot {
      method: Some(parts.method.to_string()),
      url: Some(url.clone()),
      status: None,
      headers: outbound_req_headers.clone(),
      body: request_body.clone(),
    }),
  });

  let response = match upstream.send().await {
    Ok(response) => response,
    Err(err) => {
      completion.failure(None, err.to_string());
      return Err(err).context("send passthrough upstream request");
    }
  };
  let status = response.status();
  state.events.emit(llm_core::event::Event::RequestResponded {
    request_id: request_id.clone(),
    attempt: ctx.attempt,
    status: status.as_u16(),
    latency_ms: started.elapsed().as_millis() as u64,
    resp_headers: response.headers().clone(),
  });

  if is_sse_response(response.headers(), stream) {
    completion.disarm();
    let resp = passthrough_streaming_response(state.clone(), ctx, &req_body_json, response);
    return Ok(resp);
  }

  let resp = passthrough_buffered_response(state, &ctx, &req_body_json, response).await;
  completion.disarm();
  Ok(resp)
}
