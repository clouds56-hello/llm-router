use crate::db::{HttpSnapshot, MessageRecord, PartRecord, SessionSource, Usage};
use crate::provider::Endpoint;
use bytes::Bytes;
use llm_core::event::Event;
use serde_json::Value;
use std::time::Instant;

/// Builds a `RequestResult` event from accumulated request/response data.
pub(super) struct CompletedEventBuilder {
  max: usize,
  request_id: String,
  attempt: u32,
  session_id: Option<String>,
  request_error: Option<String>,
  req_body: Bytes,
  message_endpoint: Option<Endpoint>,
  outbound_resp: Option<HttpSnapshot>,
  inbound_resp: HttpSnapshot,
  usage: Usage,
  started: Instant,
  status: u16,
}

impl CompletedEventBuilder {
  pub(crate) fn new(max: usize, request_id: String, inbound_resp: HttpSnapshot, started: Instant, status: u16) -> Self {
    Self {
      max,
      request_id,
      attempt: 0,
      session_id: None,
      request_error: None,
      req_body: Bytes::new(),
      message_endpoint: None,
      outbound_resp: None,
      inbound_resp,
      usage: Usage::default(),
      started,
      status,
    }
  }

  pub(crate) fn with_ids(mut self, session_id: Option<&str>, request_error: Option<&str>) -> Self {
    self.session_id = session_id.map(str::to_string);
    self.request_error = request_error.map(str::to_string);
    self
  }

  pub(crate) fn with_attempt(mut self, attempt: u32) -> Self {
    self.attempt = attempt;
    self
  }

  pub(crate) fn with_request_body(mut self, body: &Value, endpoint: Option<Endpoint>) -> Self {
    self.req_body = Bytes::from(serde_json::to_vec(body).unwrap_or_default());
    self.message_endpoint = endpoint;
    self
  }

  pub(crate) fn with_outbound_response(
    mut self,
    headers: Option<&reqwest::header::HeaderMap>,
    body: Option<&Bytes>,
  ) -> Self {
    self.outbound_resp = headers.map(|headers| HttpSnapshot {
      method: None,
      url: None,
      status: Some(self.status),
      headers: headers.clone(),
      body: body.cloned().unwrap_or_default(),
    });
    self
  }

  pub(crate) fn with_response_body(mut self, body: Bytes) -> Self {
    self.inbound_resp.body = body;
    self
  }

  pub(crate) fn with_request_error(mut self, error: Option<&str>) -> Self {
    self.request_error = error.map(str::to_string);
    self
  }

  pub(crate) fn with_usage(mut self, usage: Usage) -> Self {
    self.usage = usage;
    self
  }

  pub(crate) fn build(self) -> Event {
    let latency_ms = self.started.elapsed().as_millis() as u64;
    let req_body_json = serde_json::from_slice::<Value>(&self.req_body).unwrap_or(Value::Null);
    let mut messages = self
      .message_endpoint
      .map(|endpoint| extract_request_messages(&req_body_json, endpoint, self.max))
      .unwrap_or_default();
    if !self.inbound_resp.body.is_empty() && self.message_endpoint.is_some() {
      messages.push(MessageRecord {
        role: "assistant".into(),
        status: Some(self.status),
        parts: vec![PartRecord {
          part_type: "raw".into(),
          content: clip_body(&self.inbound_resp.body, self.max),
        }],
      });
    }
    let session_source = if self.session_id.is_some() {
      SessionSource::Header
    } else {
      SessionSource::Auto
    };

    Event::RequestResult {
      request_id: self.request_id,
      attempt: self.attempt,
      session_source,
      latency_ms,
      status: self.status,
      usage: self.usage,
      request_error: self.request_error,
      inbound_resp: {
        let mut snap = self.inbound_resp;
        snap.body = clip_body(snap.body.as_ref(), self.max);
        snap
      },
      outbound_resp: self.outbound_resp.map(|mut snap| {
        snap.body = clip_body(snap.body.as_ref(), self.max);
        snap
      }),
      messages,
    }
  }
}

fn clip_body(body: &[u8], max: usize) -> bytes::Bytes {
  if body.len() <= max {
    return bytes::Bytes::copy_from_slice(body);
  }
  serde_json::json!({ "_truncated": true, "size": body.len() })
    .to_string()
    .into_bytes()
    .into()
}

pub(crate) fn extract_request_messages(body: &Value, endpoint: Endpoint, max: usize) -> Vec<MessageRecord> {
  let mut out = Vec::new();
  match endpoint {
    Endpoint::ChatCompletions | Endpoint::Messages => {
      if endpoint == Endpoint::Messages {
        if let Some(system) = body.get("system") {
          out.push(MessageRecord {
            role: "system".into(),
            status: None,
            parts: parts_from_content(system, max),
          });
        }
      }
      if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
          let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user").to_string();
          let parts = match msg.get("content") {
            Some(content) => parts_from_content(content, max),
            None => vec![PartRecord {
              part_type: "raw".into(),
              content: clip_body(&serde_json::to_vec(msg).unwrap_or_default(), max),
            }],
          };
          out.push(MessageRecord {
            role,
            status: None,
            parts,
          });
        }
      }
    }
    Endpoint::Responses => {
      let input = body.get("input").unwrap_or(body);
      if let Some(items) = input.as_array() {
        for item in items {
          let role = item.get("role").and_then(|v| v.as_str()).unwrap_or("user").to_string();
          let parts = match item.get("content") {
            Some(content) => parts_from_content(content, max),
            None => vec![PartRecord {
              part_type: "raw".into(),
              content: clip_body(&serde_json::to_vec(item).unwrap_or_default(), max),
            }],
          };
          out.push(MessageRecord {
            role,
            status: None,
            parts,
          });
        }
      } else if let Some(text) = input.as_str() {
        out.push(MessageRecord {
          role: "user".into(),
          status: None,
          parts: vec![PartRecord {
            part_type: "text".into(),
            content: clip_body(text.as_bytes(), max),
          }],
        });
      } else {
        out.push(MessageRecord {
          role: "user".into(),
          status: None,
          parts: vec![PartRecord {
            part_type: "raw".into(),
            content: clip_body(&serde_json::to_vec(input).unwrap_or_default(), max),
          }],
        });
      }
    }
  }
  out
}

fn parts_from_content(content: &Value, max: usize) -> Vec<PartRecord> {
  if let Some(text) = content.as_str() {
    return vec![PartRecord {
      part_type: "text".into(),
      content: clip_body(text.as_bytes(), max),
    }];
  }
  if let Some(items) = content.as_array() {
    if items.is_empty() {
      return vec![PartRecord {
        part_type: "raw".into(),
        content: Bytes::from_static(b"[]"),
      }];
    }
    return items
      .iter()
      .map(|item| {
        let part_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("raw").to_string();
        let content_bytes = if matches!(part_type.as_str(), "text" | "input_text" | "output_text") {
          if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
            clip_body(text.as_bytes(), max)
          } else {
            clip_body(&serde_json::to_vec(item).unwrap_or_default(), max)
          }
        } else {
          clip_body(&serde_json::to_vec(item).unwrap_or_default(), max)
        };
        PartRecord {
          part_type,
          content: content_bytes,
        }
      })
      .collect();
  }
  vec![PartRecord {
    part_type: "raw".into(),
    content: clip_body(&serde_json::to_vec(content).unwrap_or_default(), max),
  }]
}
