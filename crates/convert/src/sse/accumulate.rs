use super::super::error::{ConvertError, Result};
use super::super::ir::{IrDelta, IrResponse};
use super::event::SseEvent;
use crate::provider::Endpoint;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::Value;

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
      Endpoint::ChatCompletions => crate::chat::delta_from_chat_chunk(value),
      Endpoint::Responses => crate::responses::delta_from_responses_event(value),
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
