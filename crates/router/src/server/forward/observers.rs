use super::recording::CompletedEventBuilder;
use super::usage::parse_usage_any_value;
use bytes::Bytes;
use llm_convert::sse::ObserverMsg;
use std::sync::Arc;

/// Metadata for emitting StreamProgress events.
pub(super) struct StreamMeta {
  pub request_id: Option<String>,
  pub model: String,
  pub endpoint: String,
  pub events: Arc<llm_core::event::EventBus>,
}

/// Background task that processes observer messages to build a completed event.
/// Emits periodic `StreamProgress` events (~500ms).
/// Shared between pipeline and passthrough streaming paths.
pub(super) async fn background_stream_recorder(
  mut rx: llm_convert::sse::ObserverReceiver,
  base_builder: CompletedEventBuilder,
  resp_headers: reqwest::header::HeaderMap,
  events: Arc<llm_core::event::EventBus>,
  max_body: usize,
  meta: StreamMeta,
) {
  use tokio::time::{interval, Duration};

  let mut body_buf: Vec<u8> = Vec::new();
  let mut usage: (Option<u64>, Option<u64>) = (None, None);
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
          Some(ObserverMsg::To(bytes)) => {
            bytes_streamed += bytes.len() as u64;
            chunks += 1;
            if max_body > 0 {
              let remaining = max_body.saturating_sub(body_buf.len());
              if remaining > 0 {
                body_buf.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
              }
            }
          }
          Some(ObserverMsg::Parsed(Some(value)) | ObserverMsg::Transformed(Some(value))) => {
            let (pt, ct) = parse_usage_any_value(&value);
            if pt.is_some() { usage.0 = pt; }
            if ct.is_some() { usage.1 = ct; }
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
          prompt_tokens: usage.0,
          completion_tokens: usage.1,
          bytes_streamed,
          chunks,
        });
      }
    }
  }

  let request_error = had_error.then_some("stream terminated before completion");
  let captured = Bytes::from(body_buf);
  let event = base_builder
    .with_request_error(request_error)
    .with_response_body(captured.clone())
    .with_outbound_response(Some(&resp_headers), Some(&captured))
    .with_usage(usage.0, usage.1)
    .build();
  events.emit(event);
}
