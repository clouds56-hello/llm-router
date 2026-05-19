use super::codec::encode_event;
use super::event::SseEvent;
use crate::error::Result;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use std::io;
use serde_json::Value;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::mpsc;

type EventStream = Pin<Box<dyn Stream<Item = std::io::Result<SseEvent>> + Send>>;
type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

pub type ObserverSender = mpsc::UnboundedSender<ObserverMsg>;
pub type ObserverReceiver = mpsc::UnboundedReceiver<ObserverMsg>;

/// Messages sent to an observer channel during stream processing.
#[derive(Debug)]
pub enum ObserverMsg {
  /// Raw upstream bytes (before SSE parsing). For body accumulation.
  From(Bytes),
  /// Parsed SSE event JSON (before transformers). `None` for non-JSON events.
  Parsed(Option<Value>),
  /// Transformed SSE event JSON (after transformers). `None` for non-JSON events.
  Transformed(Option<Value>),
  /// Encoded bytes yielded to the client.
  To(Bytes),
  /// Stream completed successfully.
  Done,
  /// Stream error.
  Error(String),
}

pub fn observer_channel() -> (ObserverSender, ObserverReceiver) {
  mpsc::unbounded_channel()
}

pub trait EventTransformer: Send {
  fn transform(&mut self, event: SseEvent) -> Result<Vec<SseEvent>>;

  fn finish(&mut self) -> Result<Vec<SseEvent>> {
    Ok(Vec::new())
  }
}

pub struct SsePipeline {
  source: ByteStream,
  /// Shared tap sender used for raw-byte teeing and parsed/transformed/output observation.
  tap: Option<Arc<ObserverSender>>,
  tee_byte_streams: bool,
  transformers: Vec<Box<dyn EventTransformer>>,
}

impl SsePipeline {
  /// Create a pipeline from a byte stream without a tap.
  pub fn from_stream<S>(source: S) -> Self
  where
    S: Stream<Item = io::Result<Bytes>> + Send + 'static,
  {
    Self {
      source: Box::pin(source),
      tap: None,
      tee_byte_streams: false,
      transformers: Vec::new(),
    }
  }

  /// Create a pipeline from an HTTP response without a tap (no observer overhead).
  pub fn from_response(resp: reqwest::Response) -> Self {
    Self::from_stream(resp.bytes_stream().map(|item| item.map_err(io::Error::other)))
  }

  /// Create a pipeline from an HTTP response with a tap channel.
  /// Raw upstream bytes are sent as `From(Bytes)` before SSE parsing.
  pub fn from_response_with_tap(resp: reqwest::Response, tap: ObserverSender) -> Self {
    Self::from_response(resp).with_tee_byte_streams(tap)
  }

  pub fn with_transformer<T>(mut self, transformer: T) -> Self
  where
    T: EventTransformer + 'static,
  {
    self.transformers.push(Box::new(transformer));
    self
  }

  /// Attach a tap channel and tee raw input bytes to it before SSE parsing.
  pub fn with_tee_byte_streams(mut self, tap: ObserverSender) -> Self {
    self.tap = Some(Arc::new(tap));
    self.tee_byte_streams = true;
    self
  }

  /// Attach a tap channel for observing pipeline stages.
  /// Note: this does NOT send `From(Bytes)` — use `with_tee_byte_streams` for that.
  pub fn with_tap(mut self, tap: ObserverSender) -> Self {
    self.tap = Some(Arc::new(tap));
    self
  }

  pub fn run(self) -> ByteStream {
    let source: EventStream = if self.tee_byte_streams {
      let tap = self.tap.clone();
      Box::pin(self.source.map(move |result| match result {
        Ok(bytes) => {
          if let Some(ref tap) = tap {
            let _ = tap.send(ObserverMsg::From(bytes.clone()));
          }
          Ok(bytes)
        }
        Err(err) => Err(err),
      }))
      .eventsource()
      .map(|item| match item {
        Ok(event) => Ok(SseEvent::from(event)),
        Err(err) => Err(io::Error::other(err.to_string())),
      })
      .boxed()
    } else {
      self
        .source
        .eventsource()
        .map(|item| match item {
          Ok(event) => Ok(SseEvent::from(event)),
          Err(err) => Err(io::Error::other(err.to_string())),
        })
        .boxed()
    };
    Box::pin(StreamWithFinalizer::new(
      PipelineStream::new(source, self.transformers, self.tap),
      finalize_tap,
    ))
  }
}

struct PipelineStream {
  source: EventStream,
  transformers: Vec<Box<dyn EventTransformer>>,
  tap: Option<Arc<ObserverSender>>,
  pending: VecDeque<std::io::Result<Bytes>>,
  source_done: bool,
}

impl PipelineStream {
  fn new(source: EventStream, transformers: Vec<Box<dyn EventTransformer>>, tap: Option<Arc<ObserverSender>>) -> Self {
    Self {
      source,
      transformers,
      tap,
      pending: VecDeque::new(),
      source_done: false,
    }
  }

  #[inline]
  fn send_tap(&self, msg: ObserverMsg) {
    if let Some(ref tap) = self.tap {
      let _ = tap.send(msg);
    }
  }

  fn process_event(&mut self, event: SseEvent) -> std::io::Result<()> {
    // Parsed: before transformers
    self.send_tap(ObserverMsg::Parsed(event.json.clone()));

    let transformed = self.apply_transformers(vec![event], 0)?;
    for event in transformed {
      // Transformed: after transformers
      self.send_tap(ObserverMsg::Transformed(event.json.clone()));

      let encoded = encode_event(&event);
      if !encoded.is_empty() {
        // To: encoded bytes to client
        self.send_tap(ObserverMsg::To(encoded.clone()));
        self.pending.push_back(Ok(encoded));
      }
    }
    Ok(())
  }

  fn apply_transformers(&mut self, mut events: Vec<SseEvent>, start: usize) -> std::io::Result<Vec<SseEvent>> {
    for idx in start..self.transformers.len() {
      let mut next = Vec::new();
      for event in events {
        next.extend(self.transformers[idx].transform(event).map_err(std::io::Error::other)?);
      }
      events = next;
    }
    Ok(events)
  }

  fn finish_transformers(&mut self) -> std::io::Result<()> {
    for idx in 0..self.transformers.len() {
      let events = self.transformers[idx].finish().map_err(std::io::Error::other)?;
      for event in self.apply_transformers(events, idx + 1)? {
        self.send_tap(ObserverMsg::Transformed(event.json.clone()));
        let encoded = encode_event(&event);
        if !encoded.is_empty() {
          self.send_tap(ObserverMsg::To(encoded.clone()));
          self.pending.push_back(Ok(encoded));
        }
      }
    }
    Ok(())
  }

  fn signal_error(&self, err: &std::io::Error) {
    self.send_tap(ObserverMsg::Error(err.to_string()));
  }
}

impl Stream for PipelineStream {
  type Item = std::io::Result<Bytes>;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    loop {
      if let Some(item) = self.pending.pop_front() {
        return Poll::Ready(Some(item));
      }
      if self.source_done {
        return Poll::Ready(None);
      }

      match self.source.as_mut().poll_next(cx) {
        Poll::Pending => return Poll::Pending,
        Poll::Ready(Some(Ok(event))) => {
          if let Err(err) = self.process_event(event) {
            self.signal_error(&err);
            self.pending.push_back(Err(err));
            self.source_done = true;
          }
        }
        Poll::Ready(Some(Err(err))) => {
          self.signal_error(&err);
          self.pending.push_back(Err(err));
          self.source_done = true;
        }
        Poll::Ready(None) => {
          if let Err(err) = self.finish_transformers() {
            self.signal_error(&err);
            self.pending.push_back(Err(err));
          }
          self.source_done = true;
        }
      }
    }
  }
}

fn finalize_tap(stream: &mut PipelineStream) {
  stream.send_tap(ObserverMsg::Done);
}

struct StreamWithFinalizer<S, F>
where
  S: Stream + Unpin,
  F: FnOnce(&mut S) + Send + 'static,
{
  inner: S,
  fin: Option<F>,
}

impl<S, F> StreamWithFinalizer<S, F>
where
  S: Stream + Unpin,
  F: FnOnce(&mut S) + Send + 'static,
{
  fn new(inner: S, fin: F) -> Self {
    Self { inner, fin: Some(fin) }
  }
}

impl<S, F> Stream for StreamWithFinalizer<S, F>
where
  S: Stream + Unpin,
  F: FnOnce(&mut S) + Send + 'static + Unpin,
{
  type Item = S::Item;

  fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
    let poll = Pin::new(&mut self.inner).poll_next(cx);
    if let Poll::Ready(None) = &poll {
      if let Some(fin) = self.fin.take() {
        fin(&mut self.inner);
      }
    }
    poll
  }
}

impl<S, F> Drop for StreamWithFinalizer<S, F>
where
  S: Stream + Unpin,
  F: FnOnce(&mut S) + Send + 'static,
{
  fn drop(&mut self) {
    if let Some(fin) = self.fin.take() {
      fin(&mut self.inner);
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::error::Result;
  use bytes::BytesMut;
  use futures_util::{stream, StreamExt};

  struct AppendTransformer(&'static str);

  impl EventTransformer for AppendTransformer {
    fn transform(&mut self, mut event: SseEvent) -> Result<Vec<SseEvent>> {
      if !event.is_done() {
        event.data.push_str(self.0);
      }
      Ok(vec![event])
    }
  }

  #[test]
  fn pipeline_applies_transformers_in_order() {
    let (tx, mut rx) = observer_channel();
    let body = futures::executor::block_on(async move {
      let body = SsePipeline::from_stream(stream::iter(vec![
        Ok(Bytes::from_static(b"data: hello\n\n")),
        Ok(Bytes::from_static(b"data: [DONE]\n\n")),
      ]))
      .with_transformer(AppendTransformer("-a"))
      .with_transformer(AppendTransformer("-b"))
      .with_tap(tx)
      .run()
      .collect::<Vec<_>>()
      .await
      .into_iter()
      .collect::<std::result::Result<Vec<_>, _>>()
      .unwrap()
      .into_iter()
      .fold(BytesMut::new(), |mut out, chunk| {
        out.extend_from_slice(&chunk);
        out
      })
      .freeze();

      // Verify observer messages received
      let mut msgs = Vec::new();
      while let Ok(msg) = rx.try_recv() {
        msgs.push(msg);
      }
      // Should have Parsed+Transformed+To for each event, then Done
      assert!(msgs.iter().any(|m| matches!(m, ObserverMsg::Done)));
      let to_count = msgs.iter().filter(|m| matches!(m, ObserverMsg::To(_))).count();
      assert_eq!(to_count, 2); // "hello-a-b" + "[DONE]"

      body
    });
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("data: hello-a-b"));
    assert!(text.contains("data: [DONE]"));
  }

  #[test]
  fn pipeline_tees_raw_bytes_when_enabled() {
    let (tx, mut rx) = observer_channel();
    futures::executor::block_on(async move {
      let _ = SsePipeline::from_stream(stream::iter(vec![
        Ok(Bytes::from_static(b"data: hello\n\n")),
        Ok(Bytes::from_static(b"data: [DONE]\n\n")),
      ]))
      .with_tee_byte_streams(tx)
      .run()
      .collect::<Vec<_>>()
      .await;
    });

    let mut from_count = 0;
    while let Ok(msg) = rx.try_recv() {
      if matches!(msg, ObserverMsg::From(_)) {
        from_count += 1;
      }
    }
    assert_eq!(from_count, 2);
  }
}
