use super::super::error::{ConvertError, Result};
use super::super::ir::{IrDelta, IrResponse};
use super::event::SseEvent;
use crate::provider::Endpoint;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Default)]
struct ResponsesState {
  output_items: BTreeMap<usize, ResponseOutputItem>,
}

#[derive(Default)]
struct ResponseOutputItem {
  id: Option<String>,
  name: Option<String>,
}

pub struct SseAccumulator {
  endpoint: Endpoint,
  response: IrResponse,
  responses: ResponsesState,
}

impl SseAccumulator {
  pub fn new(endpoint: Endpoint) -> Self {
    Self {
      endpoint,
      response: IrResponse::default(),
      responses: ResponsesState::default(),
    }
  }

  pub fn push_value(&mut self, value: &Value) -> Vec<IrDelta> {
    let deltas = match self.endpoint {
      Endpoint::ChatCompletions => crate::chat::delta_from_chat_chunk(value),
      Endpoint::Responses => self.delta_from_responses_event(value),
      Endpoint::Messages => crate::messages::delta_from_messages_event(value),
    };
    for delta in deltas.iter().cloned() {
      self.response.push_delta(delta);
    }
    deltas
  }

  pub fn finish(self) -> IrResponse {
    self.response
  }

  fn delta_from_responses_event(&mut self, value: &Value) -> Vec<IrDelta> {
    self.observe_responses_output_item(value);
    let mut deltas = crate::responses::delta_from_responses_event(value);
    for delta in &mut deltas {
      if let IrDelta::ToolCall { index, id, name, .. } = delta {
        if let Some(item) = self.responses.output_items.get(index) {
          if id.is_none() {
            *id = item.id.clone();
          }
          if name.is_none() {
            *name = item.name.clone();
          }
        }
      }
    }
    deltas
  }

  fn observe_responses_output_item(&mut self, value: &Value) {
    match value.get("type").and_then(Value::as_str) {
      Some("response.output_item.added") | Some("response.output_item.done") => {}
      _ => return,
    }
    let Some(index) = value.get("output_index").and_then(Value::as_u64).map(|v| v as usize) else {
      return;
    };
    let Some(item) = value.get("item") else {
      return;
    };
    if !matches!(
      item.get("type").and_then(Value::as_str),
      Some("function_call" | "custom_tool_call")
    ) {
      return;
    }
    let entry = self.responses.output_items.entry(index).or_default();
    if entry.id.is_none() {
      entry.id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .or_else(|| value.get("item_id"))
        .and_then(Value::as_str)
        .map(str::to_string);
    }
    if entry.name.is_none() {
      entry.name = item.get("name").and_then(Value::as_str).map(str::to_string);
    }
  }
}

pub async fn accumulate(endpoint: Endpoint, resp: reqwest::Response) -> Result<IrResponse> {
  let mut acc = SseAccumulator::new(endpoint);
  let mut stream = resp.bytes_stream().eventsource();
  while let Some(item) = stream.next().await {
    let ev = item.map_err(|e| ConvertError::sse(e.to_string()))?;
    let event = SseEvent::from(ev);
    if event.is_done() {
      break;
    }
    let value = event
      .json
      .as_ref()
      .ok_or_else(|| ConvertError::sse("expected JSON SSE payload"))?;
    acc.push_value(value);
  }
  Ok(acc.finish())
}
