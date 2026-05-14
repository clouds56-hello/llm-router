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

  fn finish(&mut self) -> Result<Vec<SseEvent>> {
    Ok(self.emit.finish())
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

#[cfg(test)]
mod tests {
  use super::*;
  use serde_json::json;

  #[test]
  fn responses_to_chat_finishes_when_upstream_ends_without_done_sentinel() {
    let mut t = EndpointTranslator::new(Endpoint::Responses, Endpoint::ChatCompletions);

    let out = t
      .transform(SseEvent::json(
        Some("response.output_text.delta"),
        json!({"type": "response.output_text.delta", "delta": "hi"}),
      ))
      .unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].json.as_ref().unwrap()["choices"][0]["delta"]["content"], "hi");

    let out = t
      .transform(SseEvent::json(
        Some("response.completed"),
        json!({"type": "response.completed", "response": {"usage": {"input_tokens": 1, "output_tokens": 2, "total_tokens": 3}}}),
      ))
      .unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].json.as_ref().unwrap()["usage"]["prompt_tokens"], 1);
    assert_eq!(out[1].json.as_ref().unwrap()["choices"][0]["finish_reason"], "stop");

    let out = t.finish().unwrap();
    assert_eq!(out.len(), 1);
    assert!(out[0].is_done());
  }

  #[test]
  fn responses_to_chat_finish_is_idempotent() {
    let mut t = EndpointTranslator::new(Endpoint::Responses, Endpoint::ChatCompletions);

    assert_eq!(t.transform(SseEvent::done()).unwrap().len(), 1);
    assert!(t.finish().unwrap().is_empty());
  }

  #[test]
  fn responses_to_chat_translates_resp_md_style_reasoning_text_and_tool_arguments() {
    let mut t = EndpointTranslator::new(Endpoint::Responses, Endpoint::ChatCompletions);

    let reasoning = t
      .transform(SseEvent::json(
        Some("response.reasoning_text.delta"),
        json!({"content_index":0,"delta":"Let","output_index":0,"response_id":"resp_converted","type":"response.reasoning_text.delta"}),
      ))
      .unwrap();
    assert_eq!(reasoning[0].json.as_ref().unwrap()["choices"][0]["delta"]["reasoning_content"], "Let");

    let text = t
      .transform(SseEvent::json(
        Some("response.output_text.delta"),
        json!({"content_index":0,"delta":"I'll help","output_index":0,"response_id":"resp_converted","type":"response.output_text.delta"}),
      ))
      .unwrap();
    assert_eq!(text[0].json.as_ref().unwrap()["choices"][0]["delta"]["content"], "I'll help");

    let tool = t
      .transform(SseEvent::json(
        Some("response.function_call_arguments.delta"),
        json!({"delta":"{\"cmd\": \"ls -la\"}","output_index":0,"response_id":"resp_converted","type":"response.function_call_arguments.delta"}),
      ))
      .unwrap();
    let call = &tool[0].json.as_ref().unwrap()["choices"][0]["delta"]["tool_calls"][0];
    assert_eq!(call["index"], 0);
    assert_eq!(call["type"], "function");
    assert_eq!(call["function"]["arguments"], "{\"cmd\": \"ls -la\"}");

    let completed = t
      .transform(SseEvent::json(
        Some("response.completed"),
        json!({"response":{"id":"resp_converted","model":"","object":"response","status":"completed"},"type":"response.completed"}),
      ))
      .unwrap();
    assert_eq!(completed[0].json.as_ref().unwrap()["choices"][0]["finish_reason"], "stop");

    let done = t.finish().unwrap();
    assert_eq!(done.len(), 1);
    assert!(done[0].is_done());
  }
}
