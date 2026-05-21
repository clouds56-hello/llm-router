use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct MockLlmConfig {
  pub auth: Option<MockAuthConfig>,
  pub routes: Vec<MockRoute>,
  pub required_headers: Vec<HeaderExpectation>,
  pub forbidden_headers: Vec<String>,
}

impl Default for MockLlmConfig {
  fn default() -> Self {
    Self {
      auth: None,
      routes: vec![
        MockRoute::models(["mock-model"]),
        MockRoute::chat_completions(),
        MockRoute::responses(),
        MockRoute::messages(),
      ],
      required_headers: Vec::new(),
      forbidden_headers: Vec::new(),
    }
  }
}

impl MockLlmConfig {
  pub fn with_auth(mut self, auth: MockAuthConfig) -> Self {
    self.auth = Some(auth);
    self
  }

  pub fn with_route(mut self, route: MockRoute) -> Self {
    self.routes.push(route);
    self
  }

  pub fn require_header(mut self, header: HeaderExpectation) -> Self {
    self.required_headers.push(header);
    self
  }

  pub fn forbid_header(mut self, name: impl Into<String>) -> Self {
    self.forbidden_headers.push(name.into());
    self
  }
}

#[derive(Clone, Debug)]
pub struct MockAuthConfig {
  pub header_name: String,
  pub accepted_values: Vec<String>,
}

impl MockAuthConfig {
  pub fn bearer<I, S>(accepted_tokens: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    Self {
      header_name: "authorization".into(),
      accepted_values: accepted_tokens
        .into_iter()
        .map(|token| format!("Bearer {}", token.into()))
        .collect(),
    }
  }

  pub fn header<I, S>(name: impl Into<String>, accepted_values: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    Self {
      header_name: name.into(),
      accepted_values: accepted_values.into_iter().map(Into::into).collect(),
    }
  }
}

#[derive(Clone, Debug)]
pub struct HeaderExpectation {
  pub name: String,
  pub value: Option<String>,
}

impl HeaderExpectation {
  pub fn present(name: impl Into<String>) -> Self {
    Self {
      name: name.into(),
      value: None,
    }
  }

  pub fn equals(name: impl Into<String>, value: impl Into<String>) -> Self {
    Self {
      name: name.into(),
      value: Some(value.into()),
    }
  }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockEndpoint {
  Models,
  ChatCompletions,
  Responses,
  Messages,
  Custom { method: Method, path: String },
}

impl MockEndpoint {
  fn method(&self) -> Method {
    match self {
      Self::Models => Method::GET,
      Self::ChatCompletions => Method::POST,
      Self::Responses => Method::POST,
      Self::Messages => Method::POST,
      Self::Custom { method, .. } => method.clone(),
    }
  }

  fn path(&self) -> &str {
    match self {
      Self::Models => "/models",
      Self::ChatCompletions => "/chat/completions",
      Self::Responses => "/responses",
      Self::Messages => "/messages",
      Self::Custom { path, .. } => path.as_str(),
    }
  }
}

#[derive(Clone, Debug)]
pub struct MockRoute {
  pub endpoint: MockEndpoint,
  pub response: MockResponse,
}

impl MockRoute {
  pub fn new(endpoint: MockEndpoint, response: MockResponse) -> Self {
    Self { endpoint, response }
  }

  pub fn models<I, S>(ids: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: Into<String>,
  {
    let data: Vec<Value> = ids
      .into_iter()
      .map(|id| {
        let id = id.into();
        json!({"id": id, "object": "model"})
      })
      .collect();
    Self::new(
      MockEndpoint::Models,
      MockResponse::json(json!({"object": "list", "data": data})),
    )
  }

  pub fn chat_completions() -> Self {
    Self::new(
      MockEndpoint::ChatCompletions,
      MockResponse::json(json!({
        "id": "chatcmpl-mock",
        "object": "chat.completion",
        "choices": [{
          "index": 0,
          "message": {"role": "assistant", "content": "mock response"},
          "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
      })),
    )
  }

  pub fn responses() -> Self {
    Self::new(
      MockEndpoint::Responses,
      MockResponse::json(json!({
        "id": "resp-mock",
        "object": "response",
        "status": "completed",
        "output": [{
          "type": "message",
          "role": "assistant",
          "content": [{"type": "output_text", "text": "mock response"}]
        }]
      })),
    )
  }

  pub fn messages() -> Self {
    Self::new(
      MockEndpoint::Messages,
      MockResponse::json(json!({
        "id": "msg-mock",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "output_text", "text": "mock response"}]
      })),
    )
  }
}

#[derive(Clone, Debug)]
pub struct MockResponse {
  pub status: StatusCode,
  pub headers: Vec<(String, String)>,
  pub body: Bytes,
}

impl MockResponse {
  pub fn json(value: Value) -> Self {
    Self {
      status: StatusCode::OK,
      headers: vec![("content-type".into(), "application/json".into())],
      body: Bytes::from(serde_json::to_vec(&value).expect("serialize mock JSON response")),
    }
  }
}

#[derive(Clone, Debug)]
pub struct CapturedRequest {
  pub method: Method,
  pub path: String,
  pub query: Option<String>,
  pub headers: Vec<(String, String)>,
  pub body: Bytes,
}

impl CapturedRequest {
  pub fn header(&self, name: &str) -> Option<&str> {
    self
      .headers
      .iter()
      .find(|(header, _)| header.eq_ignore_ascii_case(name))
      .map(|(_, value)| value.as_str())
  }
}

pub struct MockLlmServer {
  base_url: String,
  requests: Arc<Mutex<Vec<CapturedRequest>>>,
  shutdown: Option<oneshot::Sender<()>>,
  task: Option<JoinHandle<()>>,
}

impl MockLlmServer {
  pub async fn start(config: MockLlmConfig) -> Self {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
      .await
      .expect("bind mock llm listener");
    let addr = listener.local_addr().expect("read mock llm listener addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let state = Arc::new(MockState {
      config,
      requests: requests.clone(),
    });
    let app = Router::new().fallback(any(handle_request)).with_state(state);
    let task = tokio::spawn(async move {
      let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
      });
      let _ = server.await;
    });
    Self {
      base_url: format!("http://{addr}"),
      requests,
      shutdown: Some(shutdown_tx),
      task: Some(task),
    }
  }

  pub fn base_url(&self) -> &str {
    &self.base_url
  }

  pub fn url(&self, path: &str) -> String {
    format!("{}{}", self.base_url, path)
  }

  pub fn requests(&self) -> Vec<CapturedRequest> {
    self.requests.lock().unwrap().clone()
  }

  pub fn last_request(&self) -> Option<CapturedRequest> {
    self.requests.lock().unwrap().last().cloned()
  }

  pub async fn shutdown(mut self) {
    if let Some(tx) = self.shutdown.take() {
      let _ = tx.send(());
    }
    if let Some(task) = self.task.take() {
      let _ = task.await;
    }
  }
}

impl Drop for MockLlmServer {
  fn drop(&mut self) {
    if let Some(tx) = self.shutdown.take() {
      let _ = tx.send(());
    }
    if let Some(task) = self.task.take() {
      task.abort();
    }
  }
}

struct MockState {
  config: MockLlmConfig,
  requests: Arc<Mutex<Vec<CapturedRequest>>>,
}

async fn handle_request(State(state): State<Arc<MockState>>, request: Request) -> Response {
  let (parts, body) = request.into_parts();
  let body = match axum::body::to_bytes(body, usize::MAX).await {
    Ok(body) => body,
    Err(err) => {
      return (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("failed to read request body: {err}"),
      )
        .into_response();
    }
  };

  state.requests.lock().unwrap().push(CapturedRequest {
    method: parts.method.clone(),
    path: parts.uri.path().to_string(),
    query: parts.uri.query().map(str::to_string),
    headers: capture_headers(&parts.headers),
    body: body.clone(),
  });

  let Some(route) = state
    .config
    .routes
    .iter()
    .find(|route| route.endpoint.method() == parts.method && route.endpoint.path() == parts.uri.path())
  else {
    return (
      StatusCode::NOT_FOUND,
      format!("mock endpoint not configured for {} {}", parts.method, parts.uri.path()),
    )
      .into_response();
  };

  if let Some(auth) = &state.config.auth {
    let Some(actual) = parts
      .headers
      .get(auth.header_name.as_str())
      .and_then(|value| value.to_str().ok())
    else {
      return (StatusCode::UNAUTHORIZED, format!("missing {} header", auth.header_name)).into_response();
    };
    if !auth.accepted_values.iter().any(|expected| expected == actual) {
      return (
        StatusCode::UNAUTHORIZED,
        format!("unexpected {} header", auth.header_name),
      )
        .into_response();
    }
  }

  for required in &state.config.required_headers {
    let Some(actual) = parts
      .headers
      .get(required.name.as_str())
      .and_then(|value| value.to_str().ok())
    else {
      return (
        StatusCode::BAD_REQUEST,
        format!("missing required header {}", required.name),
      )
        .into_response();
    };
    if let Some(expected) = &required.value {
      if actual != expected {
        return (
          StatusCode::BAD_REQUEST,
          format!("unexpected value for header {}", required.name),
        )
          .into_response();
      }
    }
  }

  for forbidden in &state.config.forbidden_headers {
    if parts.headers.contains_key(forbidden.as_str()) {
      return (
        StatusCode::BAD_REQUEST,
        format!("forbidden header present: {forbidden}"),
      )
        .into_response();
    }
  }

  build_response(&route.response)
}

fn capture_headers(headers: &HeaderMap) -> Vec<(String, String)> {
  headers
    .iter()
    .map(|(name, value)| {
      (
        name.as_str().to_string(),
        value.to_str().unwrap_or_default().to_string(),
      )
    })
    .collect()
}

fn build_response(response: &MockResponse) -> Response {
  let mut out = Response::new(axum::body::Body::from(response.body.clone()));
  *out.status_mut() = response.status;
  for (name, value) in &response.headers {
    let Ok(name) = HeaderName::try_from(name.as_str()) else {
      continue;
    };
    let Ok(value) = HeaderValue::from_str(value) else {
      continue;
    };
    out.headers_mut().insert(name, value);
  }
  out
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn rejects_unconfigured_endpoints() {
    let server = MockLlmServer::start(MockLlmConfig {
      routes: vec![MockRoute::models(["gpt-4o-mini"])],
      ..Default::default()
    })
    .await;

    let http = reqwest::Client::new();
    let ok = http.get(server.url("/models")).send().await.unwrap();
    let missing = http.post(server.url("/responses")).send().await.unwrap();

    assert_eq!(ok.status(), reqwest::StatusCode::OK);
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
  }

  #[tokio::test]
  async fn enforces_auth_and_header_rules() {
    let server = MockLlmServer::start(
      MockLlmConfig {
        routes: vec![MockRoute::chat_completions()],
        ..Default::default()
      }
      .with_auth(MockAuthConfig::bearer(["sk-good", "sk-backup"]))
      .require_header(HeaderExpectation::equals("content-type", "application/json"))
      .require_header(HeaderExpectation::present("x-trace-id"))
      .forbid_header("x-blocked"),
    )
    .await;

    let http = reqwest::Client::new();
    let unauthorized = http
      .post(server.url("/chat/completions"))
      .header("content-type", "application/json")
      .header("x-trace-id", "trace-1")
      .send()
      .await
      .unwrap();
    let blocked = http
      .post(server.url("/chat/completions"))
      .header("authorization", "Bearer sk-good")
      .header("content-type", "application/json")
      .header("x-trace-id", "trace-2")
      .header("x-blocked", "true")
      .send()
      .await
      .unwrap();
    let ok = http
      .post(server.url("/chat/completions"))
      .header("authorization", "Bearer sk-backup")
      .header("content-type", "application/json")
      .header("x-trace-id", "trace-3")
      .body(r#"{"model":"gpt-4o-mini"}"#)
      .send()
      .await
      .unwrap();

    assert_eq!(unauthorized.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(blocked.status(), reqwest::StatusCode::BAD_REQUEST);
    assert_eq!(ok.status(), reqwest::StatusCode::OK);
  }

  #[tokio::test]
  async fn captures_requests_for_assertions() {
    let server = MockLlmServer::start(MockLlmConfig {
      routes: vec![MockRoute::responses()],
      ..Default::default()
    })
    .await;

    let http = reqwest::Client::new();
    let response = http
      .post(server.url("/responses?view=full"))
      .header("x-test", "yes")
      .body(r#"{"input":"hello"}"#)
      .send()
      .await
      .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let captured = server.last_request().expect("captured request");
    assert_eq!(captured.method, Method::POST);
    assert_eq!(captured.path, "/responses");
    assert_eq!(captured.query.as_deref(), Some("view=full"));
    assert_eq!(captured.header("x-test"), Some("yes"));
    assert_eq!(captured.body, Bytes::from_static(br#"{"input":"hello"}"#));
  }
}
