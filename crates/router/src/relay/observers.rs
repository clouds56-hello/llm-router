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
  resp_headers: reqwest::header::HeaderMap,
  events: Arc<llm_core::event::EventBus>,
  max_body: usize,
  meta: StreamMeta,
) -> ObserverSender {
  let (tx, rx) = observer_channel();
  tokio::spawn(background_stream_recorder(
    rx,
    builder,
    resp_headers,
    events,
    max_body,
    meta,
  ));
  tx
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
  let captured = Bytes::from(body_buf);
  let event = base_builder
    .with_request_error(request_error)
    .with_response_body(captured.clone())
    .with_outbound_response(Some(&resp_headers), Some(&captured))
    .with_usage(usage)
    .build();
  events.emit(event);

  // Emit terminal RequestCompleted with success / total_attempts / final_status.
  // Streaming is only entered after a successful upstream response (no retries after stream begins),
  // so total_attempts == meta.attempt + 1 and final_status == meta.final_status.
  events.emit(llm_core::event::Event::RequestCompleted {
    request_id: meta.request_id,
    success: !had_error,
    total_attempts: meta.attempt + 1,
    final_status: Some(meta.final_status),
    total_latency_ms: meta.started.elapsed().as_millis() as u64,
    error: had_error.then(|| "stream terminated before completion".to_string()),
  });
}
