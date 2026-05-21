use axum::body::Bytes;
use axum::http::{Method, StatusCode};
use serde_json::{json, Value};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MockEndpoint {
  Models,
  ChatCompletions,
  Responses,
  Messages,
  Custom { method: Method, path: String },
}

impl MockEndpoint {
  pub(crate) fn method(&self) -> Method {
    match self {
      Self::Models => Method::GET,
      Self::ChatCompletions => Method::POST,
      Self::Responses => Method::POST,
      Self::Messages => Method::POST,
      Self::Custom { method, .. } => method.clone(),
    }
  }

  pub(crate) fn path(&self) -> &str {
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
