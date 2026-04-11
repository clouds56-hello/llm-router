use futures::StreamExt;
use std::collections::HashMap;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::{ProviderError, ProviderStream, ProviderStreamResponse, UpstreamLogContext};
use crate::config::ModelRoute;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HttpErrorFormat {
  StatusOnly,
  StatusAndBody,
}

pub(crate) fn with_model(route: &ModelRoute, mut body: Value) -> Value {
  if let Some(obj) = body.as_object_mut() {
    obj.insert("model".to_string(), Value::String(route.provider_model.clone()));
    return body;
  }

  json!({
    "model": route.provider_model,
    "input": body
  })
}

pub(crate) fn with_stream(mut body: Value) -> Value {
  if let Some(obj) = body.as_object_mut() {
    obj.insert("stream".to_string(), Value::Bool(true));
  }
  body
}

pub(crate) fn apply_config_headers(headers: &mut HeaderMap, configured: &HashMap<String, Option<String>>) {
  for (key, value) in configured {
    let normalized = key.trim().to_ascii_lowercase();
    if normalized.is_empty() {
      continue;
    }
    let Ok(name) = HeaderName::from_bytes(normalized.as_bytes()) else {
      continue;
    };
    match value {
      Some(raw) => {
        let Ok(header_val) = HeaderValue::from_str(raw) else {
          continue;
        };
        headers.insert(name, header_val);
      }
      None => {
        headers.remove(name);
      }
    }
  }
}

pub(crate) async fn post_json(
  client: &reqwest::Client,
  log_ctx: UpstreamLogContext,
  url: String,
  headers: HeaderMap,
  body: Value,
  error_format: HttpErrorFormat,
) -> Result<Value, ProviderError> {
  let started = log_ctx.started(&body);
  let res = client
    .post(url)
    .headers(headers)
    .json(&body)
    .send()
    .await
    .map_err(|e| {
      log_ctx.failed(started, None, Some(&e.to_string()));
      ProviderError::http(e.to_string())
    })?;

  let status = res.status();
  if status.as_u16() == 401 {
    let details = res.text().await.unwrap_or_else(|_| "unauthorized".to_string());
    log_ctx.failed(started, Some(401), Some(&details));
    return Err(ProviderError::Unauthorized { status_code: 401 });
  }

  if !status.is_success() {
    let details = res.text().await.unwrap_or_default();
    log_ctx.failed(started, Some(status.as_u16()), Some(&details));
    return Err(ProviderError::http_with_status(
      match error_format {
        HttpErrorFormat::StatusOnly => format!("upstream returned status {status}"),
        HttpErrorFormat::StatusAndBody => format!("upstream returned status {status}: {details}"),
      },
      status.as_u16(),
    ));
  }

  let parsed = res.json::<Value>().await.map_err(|e| {
    log_ctx.failed(started, Some(status.as_u16()), Some(&e.to_string()));
    ProviderError::http_with_status(e.to_string(), status.as_u16())
  })?;
  log_ctx.completed(started, status.as_u16());
  Ok(parsed)
}

pub(crate) async fn post_stream(
  client: &reqwest::Client,
  log_ctx: UpstreamLogContext,
  url: String,
  headers: HeaderMap,
  body: Value,
) -> Result<ProviderStreamResponse, ProviderError> {
  let started = log_ctx.started(&body);
  let res = client
    .post(url)
    .headers(headers)
    .json(&body)
    .send()
    .await
    .map_err(|e| {
      log_ctx.failed(started, None, Some(&e.to_string()));
      ProviderError::http(e.to_string())
    })?;

  let status = res.status();
  if status.as_u16() == 401 {
    log_ctx.failed(started, Some(401), Some("unauthorized"));
    return Err(ProviderError::Unauthorized { status_code: 401 });
  }
  if !status.is_success() {
    let details = res.text().await.unwrap_or_default();
    log_ctx.failed(started, Some(status.as_u16()), Some(&details));
    return Err(ProviderError::http_with_status(
      format!("upstream returned status {status}"),
      status.as_u16(),
    ));
  }

  log_ctx.completed(started, status.as_u16());
  Ok(ProviderStreamResponse {
    stream: normalize_openai_sse(res),
    upstream_status: status.as_u16(),
  })
}

pub(crate) fn normalize_openai_sse(res: reqwest::Response) -> ProviderStream {
  let (tx, rx) = mpsc::channel::<Result<String, ProviderError>>(32);
  tokio::spawn(async move {
    let mut upstream = res.bytes_stream();
    let mut buffer = String::new();
    while let Some(chunk) = upstream.next().await {
      let bytes = match chunk {
        Ok(bytes) => bytes,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::http(err.to_string()))).await;
          break;
        }
      };
      let chunk_str = match String::from_utf8(bytes.to_vec()) {
        Ok(v) => v,
        Err(err) => {
          let _ = tx.send(Err(ProviderError::internal(err.to_string()))).await;
          break;
        }
      };
      buffer.push_str(&chunk_str);
      while let Some(idx) = buffer.find("\n\n") {
        let frame = buffer[..idx].to_string();
        buffer = buffer[idx + 2..].to_string();
        for payload in parse_sse_frame_payloads(&frame) {
          if tx.send(Ok(payload)).await.is_err() {
            return;
          }
        }
      }
    }
    if !buffer.trim().is_empty() {
      for payload in parse_sse_frame_payloads(&buffer) {
        if tx.send(Ok(payload)).await.is_err() {
          return;
        }
      }
    }
  });
  Box::pin(ReceiverStream::new(rx))
}

pub(crate) fn parse_sse_frame_payloads(frame: &str) -> Vec<String> {
  let mut data_lines: Vec<String> = Vec::new();
  for raw in frame.lines() {
    let line = raw.trim_end_matches('\r');
    if let Some(data) = line.strip_prefix("data:") {
      data_lines.push(data.trim_start().to_string());
    }
  }
  if data_lines.is_empty() {
    return Vec::new();
  }
  let payload = data_lines.join("\n").trim().to_string();
  if payload.is_empty() {
    Vec::new()
  } else {
    vec![payload]
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  use axum::extract::State;
  use axum::http::StatusCode;
  use axum::routing::post;
  use axum::Router;
  use reqwest::header::CONTENT_TYPE;
  use tokio::sync::oneshot;

  #[derive(Clone)]
  struct UpstreamStub {
    status: StatusCode,
    body: &'static str,
  }

  async fn stub_handler(State(stub): State<UpstreamStub>) -> (StatusCode, [(String, String); 1], &'static str) {
    (
      stub.status,
      [("content-type".to_string(), "application/json".to_string())],
      stub.body,
    )
  }

  async fn start_stub_server(stub: UpstreamStub) -> (std::net::SocketAddr, oneshot::Sender<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/x", post(stub_handler)).with_state(stub);
    let (tx, rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
      let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async {
          let _ = rx.await;
        })
        .await;
    });
    (addr, tx)
  }

  fn log_ctx() -> UpstreamLogContext {
    UpstreamLogContext {
      provider: "p".to_string(),
      adapter: "a".to_string(),
      upstream_path: "/x".to_string(),
      method: "POST",
      model: Some("m".to_string()),
      stream: false,
    }
  }

  #[test]
  fn sse_parser_preserves_multiline_and_crlf() {
    let frame = "event: message\r\ndata: {\"a\":1}\r\ndata: {\"b\":2}\r\n";
    assert_eq!(
      parse_sse_frame_payloads(frame),
      vec!["{\"a\":1}\n{\"b\":2}".to_string()]
    );
  }

  #[test]
  fn sse_parser_ignores_empty_data() {
    assert!(parse_sse_frame_payloads("event: ping").is_empty());
    assert!(parse_sse_frame_payloads("data:   ").is_empty());
  }

  #[test]
  fn apply_config_headers_overrides_and_removes() {
    let mut headers = HeaderMap::new();
    headers.insert("x-a", "base".parse().expect("header"));
    headers.insert("x-b", "keep".parse().expect("header"));

    let mut configured = HashMap::new();
    configured.insert("X-A".to_string(), Some("override".to_string()));
    configured.insert("x-b".to_string(), None);
    configured.insert("bad header".to_string(), Some("ignored".to_string()));

    apply_config_headers(&mut headers, &configured);

    assert_eq!(headers.get("x-a").and_then(|v| v.to_str().ok()), Some("override"));
    assert!(headers.get("x-b").is_none());
  }

  #[tokio::test]
  async fn post_json_maps_401_to_unauthorized() {
    let client = reqwest::Client::new();
    let (addr, shutdown) = start_stub_server(UpstreamStub {
      status: StatusCode::UNAUTHORIZED,
      body: r#"{"error":"unauthorized"}"#,
    })
    .await;

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, "application/json".parse().expect("header"));
    let err = post_json(
      &client,
      log_ctx(),
      format!("http://{addr}/x"),
      headers,
      json!({"model":"x"}),
      HttpErrorFormat::StatusOnly,
    )
    .await
    .expect_err("expected unauthorized");
    assert!(matches!(err, ProviderError::Unauthorized { .. }));
    let _ = shutdown.send(());
  }
}
