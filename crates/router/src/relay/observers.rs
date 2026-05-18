use super::recording::CompletedEventBuilder;
use super::usage::parse_usage_any_value;
use bytes::Bytes;
use llm_convert::sse::{observer_channel, ObserverMsg, ObserverSender};
use llm_core::db::Usage;
use std::sync::Arc;

/// Metadata for emitting StreamProgress and the terminal RequestCompleted event.
pub(super) struct StreamMeta {
  /// Base request ID (no retry suffix).
  pub request_id: String,
  /// Final attempt number (0-indexed). total_attempts == attempt + 1.
  pub attempt: u32,
  /// Upstream HTTP status (always 2xx since streaming begins after a successful response).
  pub final_status: u16,
  /// Time at which the overall request started (for total_latency_ms).
  pub started: std::time::Instant,
  pub model: String,
  pub endpoint: String,
  pub events: Arc<llm_core::event::EventBus>,
}

/// Creates an observer channel, spawns the background stream recorder task,
/// and returns the sender half for feeding into an SsePipeline.
pub(super) fn spawn_stream_recorder(
  builder: CompletedEventBuilder,
  events: Arc<llm_core::event::EventBus>,
  max_body: usize,
  meta: StreamMeta,
) -> ObserverSender {
  let (tx, rx) = observer_channel();
  tokio::spawn(background_stream_recorder(rx, builder, events, max_body, meta));
  tx
}

/// Background task that processes observer messages to build a completed event.
/// Emits periodic `StreamProgress` events (~500ms).
/// Shared between pipeline and passthrough streaming paths.
pub(super) async fn background_stream_recorder(
  mut rx: llm_convert::sse::ObserverReceiver,
  base_builder: CompletedEventBuilder,
  events: Arc<llm_core::event::EventBus>,
  max_body: usize,
  meta: StreamMeta,
) {
  use tokio::time::{interval, Duration};

  let mut inbound_body_buf: Vec<u8> = Vec::new();
  let mut outbound_body_buf: Vec<u8> = Vec::new();
  let mut usage = Usage::default();
  let mut had_error = false;
  let mut bytes_streamed: u64 = 0;
  let mut chunks: u64 = 0;

  let mut tick = interval(Duration::from_millis(500));
  tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
  tick.tick().await; // skip first immediate tick

  loop {
    tokio::select! {
      msg = rx.recv() => {
        match msg {
          Some(ObserverMsg::From(bytes)) => {
            if max_body > 0 {
              let remaining = max_body.saturating_sub(outbound_body_buf.len());
              if remaining > 0 {
                outbound_body_buf.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
              }
            }
          }
          Some(ObserverMsg::To(bytes)) => {
            bytes_streamed += bytes.len() as u64;
            chunks += 1;
            if max_body > 0 {
              let remaining = max_body.saturating_sub(inbound_body_buf.len());
              if remaining > 0 {
                inbound_body_buf.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
              }
            }
          }
          Some(ObserverMsg::Parsed(Some(value)) | ObserverMsg::Transformed(Some(value))) => {
            let parsed = parse_usage_any_value(&value);
            if parsed.input_tokens.is_some() { usage.input_tokens = parsed.input_tokens; }
            if parsed.output_tokens.is_some() { usage.output_tokens = parsed.output_tokens; }
            if parsed.details.cache_read.is_some() { usage.details.cache_read = parsed.details.cache_read; }
            if parsed.details.reasoning.is_some() { usage.details.reasoning = parsed.details.reasoning; }
          }
          Some(ObserverMsg::Done) => break,
          Some(ObserverMsg::Error(_)) => { had_error = true; break; }
          Some(_) => {}
          None => break,
        }
      }
      _ = tick.tick() => {
        meta.events.emit(llm_core::event::Event::StreamProgress {
          request_id: meta.request_id.clone(),
          model: meta.model.clone(),
          endpoint: meta.endpoint.clone(),
          usage: usage.clone(),
          bytes_streamed,
          chunks,
        });
      }
    }
  }

  let request_error = had_error.then_some("stream terminated before completion");
  let inbound_captured = Bytes::from(inbound_body_buf);
  let outbound_captured = Bytes::from(outbound_body_buf);
  let event = base_builder
    .with_request_error(request_error)
    .with_response_body(inbound_captured)
    .with_outbound_response_body((!outbound_captured.is_empty()).then_some(&outbound_captured))
    .with_usage(usage)
    .build();
  events.emit(event);

  // Emit terminal RequestCompleted with success / total_attempts / final_status.
  // Streaming is only entered after a successful upstream response (no retries after stream begins),
  // so total_attempts == meta.attempt + 1 and final_status == meta.final_status.
  events.emit(llm_core::event::Event::LegacyRequest(
    llm_core::event::LegacyRequestEvent::Completed {
      request_id: meta.request_id,
      success: !had_error,
      total_attempts: meta.attempt + 1,
      final_status: Some(meta.final_status),
      total_latency_ms: meta.started.elapsed().as_millis() as u64,
      error: had_error.then(|| "stream terminated before completion".to_string()),
    },
  ));
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::relay::recording::CompletedEventBuilder;
  use bytes::Bytes;
  use llm_core::event::{Event, EventBus, EventHandler, LegacyRequestEvent};
  use std::sync::{Arc, Mutex};
  use std::time::Instant;

  struct CollectingHandler(Arc<Mutex<Vec<(Bytes, Option<Bytes>)>>>);

  impl EventHandler for CollectingHandler {
    fn handle(&mut self, event: &Event) {
      if let Event::LegacyRequest(LegacyRequestEvent::Result {
        inbound_resp_body,
        outbound_resp_body,
        ..
      }) = event
      {
        self
          .0
          .lock()
          .unwrap()
          .push((inbound_resp_body.clone(), outbound_resp_body.clone()));
      }
    }
  }

  #[tokio::test]
  async fn stream_recorder_keeps_upstream_and_downstream_bodies_separate() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (bus, receiver) = {
      let bus = EventBus::new(16);
      let rx = bus.subscribe();
      (bus, rx)
    };
    llm_core::event::spawn_event_loop(receiver, vec![Box::new(CollectingHandler(captured.clone()))]);
    let events = Arc::new(bus);

    let builder = CompletedEventBuilder::new(
      1024,
      "req_test".into(),
      Default::default(),
      Bytes::new(),
      Instant::now(),
      200,
    );
    let meta = StreamMeta {
      request_id: "req_test".into(),
      attempt: 0,
      final_status: 200,
      started: Instant::now(),
      model: "model".into(),
      endpoint: "chat_completions".into(),
      events: events.clone(),
    };
    let (tx, rx) = llm_convert::sse::observer_channel();
    let handle = tokio::spawn(background_stream_recorder(rx, builder, events.clone(), 1024, meta));

    tx.send(ObserverMsg::From(Bytes::from_static(b"upstream"))).unwrap();
    tx.send(ObserverMsg::To(Bytes::from_static(b"downstream"))).unwrap();
    tx.send(ObserverMsg::Done).unwrap();
    handle.await.unwrap();
    events.shutdown().await;

    let events = captured.lock().unwrap();
    let result = events.first().expect("request result event");

    assert_eq!(result.0.as_ref(), b"downstream");
    assert_eq!(result.1.as_ref().unwrap().as_ref(), b"upstream");
  }
}
