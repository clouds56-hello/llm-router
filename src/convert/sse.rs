use super::error::{ConvertError, Result};
use super::ir::{IrDelta, IrResponse};
use crate::provider::Endpoint;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use serde_json::Value;
use std::pin::Pin;

pub struct SseAccumulator {
  endpoint: Endpoint,
  response: IrResponse,
}

impl SseAccumulator {
  pub fn new(endpoint: Endpoint) -> Self {
    Self {
      endpoint,
      response: IrResponse::default(),
    }
  }

  pub fn push_value(&mut self, value: &Value) -> Vec<IrDelta> {
    let deltas = match self.endpoint {
      Endpoint::ChatCompletions => crate::convert::chat::delta_from_chat_chunk(value),
      Endpoint::Responses => crate::convert::responses::delta_from_responses_event(value),
      Endpoint::Messages => crate::convert::messages::delta_from_messages_event(value),
    };
    for delta in deltas.iter().cloned() {
      self.response.push_delta(delta);
    }
    deltas
  }

  #[allow(dead_code)]
  pub fn finish(self) -> IrResponse {
    self.response
  }
}

#[allow(dead_code)]
pub async fn accumulate(endpoint: Endpoint, resp: reqwest::Response) -> Result<IrResponse> {
  let mut acc = SseAccumulator::new(endpoint);
  let mut stream = resp.bytes_stream().eventsource();
  while let Some(item) = stream.next().await {
    let ev = item.map_err(|e| ConvertError::sse(e.to_string()))?;
    if ev.data.trim() == "[DONE]" {
      break;
    }
    let value: Value = serde_json::from_str(&ev.data)?;
    acc.push_value(&value);
  }
  Ok(acc.finish())
}

pub fn translate_stream(
  from: Endpoint,
  to: Endpoint,
  resp: reqwest::Response,
) -> Pin<Box<dyn Stream<Item = std::result::Result<Bytes, std::io::Error>> + Send>> {
  if from == to {
    return Box::pin(resp.bytes_stream().map(|r| r.map_err(std::io::Error::other)));
  }

  let mut acc = SseAccumulator::new(from);
  let mut emit = EmitState::new(to);
  let stream = resp.bytes_stream().eventsource().map(move |item| {
    let ev = item.map_err(|e| std::io::Error::other(e.to_string()))?;
    let payload = ev.data.trim();
    if payload == "[DONE]" {
      return Ok(emit.finish());
    }
    let value: Value = serde_json::from_str(payload).map_err(std::io::Error::other)?;
    let deltas = acc.push_value(&value);
    Ok(emit.emit(&deltas))
  });
  Box::pin(stream)
}

pub fn encode_sse(event: Option<&str>, data: &Value) -> Bytes {
  let mut out = String::new();
  if let Some(event) = event {
    out.push_str("event: ");
    out.push_str(event);
    out.push('\n');
  }
  out.push_str("data: ");
  out.push_str(&serde_json::to_string(data).unwrap_or_else(|_| "{}".into()));
  out.push_str("\n\n");
  Bytes::from(out)
}

fn encode_done() -> Bytes {
  Bytes::from_static(b"data: [DONE]\n\n")
}

struct EmitState {
  to: Endpoint,
  id: String,
  model: String,
  started: bool,
  finished: bool,
}

impl EmitState {
  fn new(to: Endpoint) -> Self {
    Self {
      to,
      id: match to {
        Endpoint::ChatCompletions => "chatcmpl-converted".into(),
        Endpoint::Responses => "resp_converted".into(),
        Endpoint::Messages => "msg_converted".into(),
      },
      model: String::new(),
      started: false,
      finished: false,
    }
  }

  fn emit(&mut self, deltas: &[IrDelta]) -> Bytes {
    if deltas.is_empty() {
      return Bytes::new();
    }
    let mut out = Vec::new();
    if !self.started {
      out.extend_from_slice(&self.start());
      self.started = true;
    }
    match self.to {
      Endpoint::ChatCompletions => {
        for value in crate::convert::chat::chunk_from_deltas(&self.id, &self.model, deltas, false) {
          out.extend_from_slice(&encode_sse(None, &value));
        }
      }
      Endpoint::Responses => {
        for (event, value) in crate::convert::responses::events_from_deltas(&self.id, &self.model, deltas, false) {
          out.extend_from_slice(&encode_sse(Some(&event), &value));
        }
      }
      Endpoint::Messages => {
        for (event, value) in crate::convert::messages::events_from_deltas(&self.id, &self.model, deltas, false) {
          out.extend_from_slice(&encode_sse(Some(&event), &value));
        }
      }
    }
    Bytes::from(out)
  }

  fn finish(&mut self) -> Bytes {
    if self.finished {
      return Bytes::new();
    }
    self.finished = true;
    let mut out = Vec::new();
    if !self.started {
      out.extend_from_slice(&self.start());
      self.started = true;
    }
    match self.to {
      Endpoint::ChatCompletions => out.extend_from_slice(&encode_done()),
      Endpoint::Responses => {
        let event = "response.completed";
        let value = serde_json::json!({
          "type": event,
          "response": { "id": self.id, "object": "response", "status": "completed", "model": self.model }
        });
        out.extend_from_slice(&encode_sse(Some(event), &value));
      }
      Endpoint::Messages => {
        out.extend_from_slice(&encode_sse(
          Some("content_block_stop"),
          &serde_json::json!({ "type": "content_block_stop", "index": 0 }),
        ));
        out.extend_from_slice(&encode_sse(
          Some("message_stop"),
          &serde_json::json!({ "type": "message_stop" }),
        ));
      }
    }
    Bytes::from(out)
  }

  fn start(&self) -> Bytes {
    match self.to {
      Endpoint::ChatCompletions => Bytes::new(),
      Endpoint::Responses => {
        let value = serde_json::json!({
          "type": "response.created",
          "response": { "id": self.id, "object": "response", "status": "in_progress", "model": self.model }
        });
        encode_sse(Some("response.created"), &value)
      }
      Endpoint::Messages => {
        let mut out = Vec::new();
        out.extend_from_slice(&encode_sse(
          Some("message_start"),
          &serde_json::json!({
            "type": "message_start",
            "message": { "id": self.id, "type": "message", "role": "assistant", "model": self.model, "content": [], "stop_reason": null, "stop_sequence": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }
          }),
        ));
        out.extend_from_slice(&encode_sse(
          Some("content_block_start"),
          &serde_json::json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        ));
        Bytes::from(out)
      }
    }
  }
}
