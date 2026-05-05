use crate::db::{CallRecord, HttpSnapshot, MessageRecord, OutboundSnapshot, PartRecord, SessionSource};
use crate::provider::Endpoint;
use crate::server::AppState;
use bytes::Bytes;
use serde_json::Value;
use std::time::Instant;
use uuid::Uuid;

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_call(
  s: &AppState,
  account_id: &str,
  provider_id: &str,
  endpoint: Endpoint,
  model: &str,
  initiator: &str,
  session_id: Option<&str>,
  request_id: Option<&str>,
  project_id: Option<&str>,
  req_headers: &reqwest::header::HeaderMap,
  req_body: &Value,
  resp_headers: Option<&reqwest::header::HeaderMap>,
  resp_body: Option<&bytes::Bytes>,
  inbound_resp_headers: &reqwest::header::HeaderMap,
  outbound: Option<OutboundSnapshot>,
  pt: Option<u64>,
  ct: Option<u64>,
  started: Instant,
  status: u16,
  stream: bool,
) {
  if s.db.is_none() {
    return;
  }
  let req_body_bytes = serde_json::to_vec(req_body).unwrap_or_default();
  record_call_with_snapshots(
    s,
    account_id,
    provider_id,
    endpoint.as_str(),
    model,
    initiator,
    session_id,
    request_id,
    project_id,
    &req_body_bytes,
    Some(endpoint),
    Some(HttpSnapshot {
      method: None,
      url: None,
      status: None,
      headers: req_headers.clone(),
      body: Bytes::from(req_body_bytes.clone()),
    }),
    outbound,
    resp_headers.map(|headers| HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: headers.clone(),
      body: resp_body.cloned().unwrap_or_default(),
    }),
    HttpSnapshot {
      method: None,
      url: None,
      status: Some(status),
      headers: inbound_resp_headers.clone(),
      body: resp_body.cloned().unwrap_or_default(),
    },
    (pt, ct),
    started,
    status,
    stream,
  );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn record_call_with_snapshots(
  s: &AppState,
  account_id: &str,
  provider_id: &str,
  endpoint: &str,
  model: &str,
  initiator: &str,
  session_id: Option<&str>,
  request_id: Option<&str>,
  project_id: Option<&str>,
  req_body: &[u8],
  message_endpoint: Option<Endpoint>,
  inbound_req: Option<HttpSnapshot>,
  outbound_req: Option<OutboundSnapshot>,
  outbound_resp: Option<HttpSnapshot>,
  inbound_resp: HttpSnapshot,
  usage: (Option<u64>, Option<u64>),
  started: Instant,
  status: u16,
  stream: bool,
) {
  let Some(db) = s.db.as_ref() else { return };
  let latency_ms = started.elapsed().as_millis() as u64;
  let max = db.body_max_bytes();
  let req_body_json = serde_json::from_slice::<Value>(req_body).unwrap_or(Value::Null);
  let mut messages = message_endpoint
    .map(|endpoint| extract_request_messages(&req_body_json, endpoint, max))
    .unwrap_or_default();
  if !inbound_resp.body.is_empty() && message_endpoint.is_some() {
    messages.push(MessageRecord {
      role: "assistant".into(),
      status: Some(status),
      parts: vec![PartRecord {
        part_type: "raw".into(),
        content: clip_body(&inbound_resp.body, max),
      }],
    });
  }
  let (effective_id, source) = match session_id {
    Some(id) => (id.to_string(), SessionSource::Header),
    None => (Uuid::new_v4().to_string(), SessionSource::Auto),
  };
  db.record(CallRecord {
    ts: time::OffsetDateTime::now_utc().unix_timestamp(),
    session_id: effective_id,
    session_source: source,
    request_id: request_id.map(str::to_string),
    project_id: project_id.map(str::to_string),
    endpoint: endpoint.to_string(),
    account_id: account_id.to_string(),
    provider_id: provider_id.to_string(),
    model: model.to_string(),
    initiator: initiator.to_string(),
    status,
    stream,
    latency_ms,
    prompt_tokens: usage.0,
    completion_tokens: usage.1,
    inbound_req: inbound_req
      .map(|mut snap| {
        snap.body = clip_body(snap.body.as_ref(), max);
        snap
      })
      .unwrap_or_default(),
    outbound_req: outbound_req.map(|mut snap| {
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    }),
    outbound_resp: outbound_resp.map(|mut snap| {
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    }),
    inbound_resp: {
      let mut snap = inbound_resp;
      snap.body = clip_body(snap.body.as_ref(), max);
      snap
    },
    messages,
  });
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
