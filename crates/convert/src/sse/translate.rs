use super::accumulate::SseAccumulator;
use super::event::SseEvent;
use super::pipeline::EventTransformer;
use crate::error::{ConvertError, Result};
use crate::ir::IrDelta;
use crate::provider::Endpoint;

pub struct EndpointTranslator {
  acc: SseAccumulator,
  emit: EmitState,
}

impl EndpointTranslator {
  pub fn new(from: Endpoint, to: Endpoint) -> Self {
    Self {
      acc: SseAccumulator::new(from),
      emit: EmitState::new(to),
    }
  }
}

impl EventTransformer for EndpointTranslator {
  fn transform(&mut self, event: SseEvent) -> Result<Vec<SseEvent>> {
    if event.is_done() {
      return Ok(self.emit.finish());
    }
    let value = event
      .json
      .as_ref()
      .ok_or_else(|| ConvertError::sse("expected JSON SSE payload"))?;
    let deltas = self.acc.push_value(value);
    Ok(self.emit.emit(&deltas))
  }
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

  fn emit(&mut self, deltas: &[IrDelta]) -> Vec<SseEvent> {
    if deltas.is_empty() {
      return Vec::new();
    }
    let mut out = Vec::new();
    if !self.started {
      out.extend(self.start());
      self.started = true;
    }
    match self.to {
      Endpoint::ChatCompletions => {
        for value in crate::chat::chunk_from_deltas(&self.id, &self.model, deltas, false) {
          out.push(SseEvent::json(None, value));
        }
      }
      Endpoint::Responses => {
        for (event, value) in crate::responses::events_from_deltas(&self.id, &self.model, deltas, false) {
          out.push(SseEvent::json(Some(&event), value));
        }
      }
      Endpoint::Messages => {
        for (event, value) in crate::messages::events_from_deltas(&self.id, &self.model, deltas, false) {
          out.push(SseEvent::json(Some(&event), value));
        }
      }
    }
    out
  }

  fn finish(&mut self) -> Vec<SseEvent> {
    if self.finished {
      return Vec::new();
    }
    self.finished = true;
    let mut out = Vec::new();
    if !self.started {
      out.extend(self.start());
      self.started = true;
    }
    match self.to {
      Endpoint::ChatCompletions => out.push(SseEvent::done()),
      Endpoint::Responses => {
        let event = "response.completed";
        out.push(SseEvent::json(
          Some(event),
          serde_json::json!({
            "type": event,
            "response": { "id": self.id, "object": "response", "status": "completed", "model": self.model }
          }),
        ));
      }
      Endpoint::Messages => {
        out.push(SseEvent::json(
          Some("content_block_stop"),
          serde_json::json!({ "type": "content_block_stop", "index": 0 }),
        ));
        out.push(SseEvent::json(
          Some("message_stop"),
          serde_json::json!({ "type": "message_stop" }),
        ));
      }
    }
    out
  }

  fn start(&self) -> Vec<SseEvent> {
    match self.to {
      Endpoint::ChatCompletions => Vec::new(),
      Endpoint::Responses => vec![SseEvent::json(
        Some("response.created"),
        serde_json::json!({
          "type": "response.created",
          "response": { "id": self.id, "object": "response", "status": "in_progress", "model": self.model }
        }),
      )],
      Endpoint::Messages => vec![
        SseEvent::json(
          Some("message_start"),
          serde_json::json!({
            "type": "message_start",
            "message": { "id": self.id, "type": "message", "role": "assistant", "model": self.model, "content": [], "stop_reason": null, "stop_sequence": null, "usage": { "input_tokens": 0, "output_tokens": 0 } }
          }),
        ),
        SseEvent::json(
          Some("content_block_start"),
          serde_json::json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        ),
      ],
    }
  }
}
