use crate::db::{CallRecord, HttpSnapshot, MessageRecord, OutboundSnapshot, PartRecord, SessionSource};
use crate::provider::Endpoint;
use bytes::Bytes;
use serde_json::Value;
use std::time::Instant;
use uuid::Uuid;

pub(super) struct CallRecordBuilder {
  max: usize,
  account_id: String,
  provider_id: String,
  endpoint: String,
  model: String,
  initiator: String,
  session_id: Option<String>,
  request_id: Option<String>,
  request_error: Option<String>,
  project_id: Option<String>,
  req_body: Bytes,
  message_endpoint: Option<Endpoint>,
  inbound_req: Option<HttpSnapshot>,
  outbound_req: Option<OutboundSnapshot>,
  outbound_resp: Option<HttpSnapshot>,
  inbound_resp: HttpSnapshot,
  prompt_tokens: Option<u64>,
  completion_tokens: Option<u64>,
  started: Instant,
  status: u16,
  stream: bool,
}

impl CallRecordBuilder {
  pub(crate) fn for_endpoint(
    max: usize,
    account_id: &str,
    provider_id: &str,
    endpoint: Endpoint,
    model: &str,
    initiator: &str,
    inbound_resp: HttpSnapshot,
    started: Instant,
    status: u16,
    stream: bool,
  ) -> Self {
    Self::new(
      max,
      account_id,
      provider_id,
      endpoint.as_str(),
      Some(endpoint),
      model,
      initiator,
      inbound_resp,
      started,
      status,
      stream,
    )
  }

  pub(crate) fn for_path(
    max: usize,
    account_id: &str,
    provider_id: &str,
    endpoint: &str,
    message_endpoint: Option<Endpoint>,
    model: &str,
    initiator: &str,
    inbound_resp: HttpSnapshot,
    started: Instant,
    status: u16,
    stream: bool,
  ) -> Self {
    Self::new(
      max,
      account_id,
      provider_id,
      endpoint,
      message_endpoint,
      model,
      initiator,
      inbound_resp,
      started,
      status,
      stream,
    )
  }

  fn new(
    max: usize,
    account_id: &str,
    provider_id: &str,
    endpoint: &str,
    message_endpoint: Option<Endpoint>,
    model: &str,
    initiator: &str,
    inbound_resp: HttpSnapshot,
    started: Instant,
    status: u16,
    stream: bool,
  ) -> Self {
    Self {
      max,
      account_id: account_id.to_string(),
      provider_id: provider_id.to_string(),
      endpoint: endpoint.to_string(),
      model: model.to_string(),
      initiator: initiator.to_string(),
      session_id: None,
      request_id: None,
      request_error: None,
      project_id: None,
      req_body: Bytes::new(),
      message_endpoint,
      inbound_req: None,
      outbound_req: None,
      outbound_resp: None,
      inbound_resp,
      prompt_tokens: None,
      completion_tokens: None,
      started,
      status,
      stream,
    }
  }

  pub(crate) fn with_ids(
    mut self,
    session_id: Option<&str>,
    request_id: Option<&str>,
    request_error: Option<&str>,
    project_id: Option<&str>,
  ) -> Self {
    self.session_id = session_id.map(str::to_string);
    self.request_id = request_id.map(str::to_string);
    self.request_error = request_error.map(str::to_string);
    self.project_id = project_id.map(str::to_string);
    self
  }

  pub(crate) fn with_request_json(mut self, headers: &reqwest::header::HeaderMap, body: &Value) -> Self {
    let req_body = serde_json::to_vec(body).unwrap_or_default();
    self.req_body = Bytes::from(req_body.clone());
    self.inbound_req = Some(HttpSnapshot {
      method: None,
      url: None,
      status: None,
      headers: headers.clone(),
      body: Bytes::from(req_body),
    });
    self
  }

  pub(crate) fn with_request_snapshot(mut self, req_body: impl Into<Bytes>, inbound_req: Option<HttpSnapshot>) -> Self {
    self.req_body = req_body.into();
    self.inbound_req = inbound_req;
    self
  }

  pub(crate) fn with_outbound_request(mut self, outbound_req: Option<OutboundSnapshot>) -> Self {
    self.outbound_req = outbound_req;
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

  pub(crate) fn with_response_snapshot(mut self, snapshot: Option<HttpSnapshot>) -> Self {
    self.outbound_resp = snapshot;
    self
  }

  pub(crate) fn with_request_error(mut self, request_error: Option<&str>) -> Self {
    self.request_error = request_error.map(str::to_string);
    self
  }

  pub(crate) fn with_response_body(mut self, body: Bytes) -> Self {
    self.inbound_resp.body = body;
    self
  }

  pub(crate) fn with_usage(mut self, prompt_tokens: Option<u64>, completion_tokens: Option<u64>) -> Self {
    self.prompt_tokens = prompt_tokens;
    self.completion_tokens = completion_tokens;
    self
  }

  pub(crate) fn build(self) -> CallRecord {
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
    let (effective_id, source) = match self.session_id {
      Some(id) => (id, SessionSource::Header),
      None => (Uuid::new_v4().to_string(), SessionSource::Auto),
    };
    CallRecord {
      ts: time::OffsetDateTime::now_utc().unix_timestamp(),
      session_id: effective_id,
      session_source: source,
      request_id: self.request_id,
      request_error: self.request_error,
      project_id: self.project_id,
      endpoint: self.endpoint,
      account_id: self.account_id,
      provider_id: self.provider_id,
      model: self.model,
      initiator: self.initiator,
      status: self.status,
      stream: self.stream,
      latency_ms,
      prompt_tokens: self.prompt_tokens,
      completion_tokens: self.completion_tokens,
      inbound_req: self
        .inbound_req
        .map(|mut snap| {
          snap.body = clip_body(snap.body.as_ref(), self.max);
          snap
        })
        .unwrap_or_default(),
      outbound_req: self.outbound_req.map(|mut snap| {
        snap.body = clip_body(snap.body.as_ref(), self.max);
        snap
      }),
      outbound_resp: self.outbound_resp.map(|mut snap| {
        snap.body = clip_body(snap.body.as_ref(), self.max);
        snap
      }),
      inbound_resp: {
        let mut snap = self.inbound_resp;
        snap.body = clip_body(snap.body.as_ref(), self.max);
        snap
      },
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
