use super::codec::encode_event;
use super::event::SseEvent;
use crate::error::Result;
use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_util::{Stream, StreamExt};
use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

type EventStream = Pin<Box<dyn Stream<Item = std::io::Result<SseEvent>> + Send>>;
type ByteStream = Pin<Box<dyn Stream<Item = std::io::Result<Bytes>> + Send>>;

pub trait EventTransformer: Send {
  fn transform(&mut self, event: SseEvent) -> Result<Vec<SseEvent>>;

  fn finish(&mut self) -> Result<Vec<SseEvent>> {
    Ok(Vec::new())
  }
}

pub trait EventObserver: Send {
  fn observe(&mut self, _event: &SseEvent, _encoded: &Bytes) {}

  fn on_error(&mut self, _err: &std::io::Error) {}

  fn finish(&mut self) {}
}

pub struct SsePipeline {
  source: EventStream,
  transformers: Vec<Box<dyn EventTransformer>>,
  observers: Vec<Box<dyn EventObserver>>,
}

impl SsePipeline {
  pub fn from_response(resp: reqwest::Response) -> Self {
    let source = resp.bytes_stream().eventsource().map(|item| match item {
      Ok(event) => Ok(SseEvent::from(event)),
      Err(err) => Err(std::io::Error::other(err.to_string())),
    });
    Self::from_stream(source)
  }

  pub fn from_stream<S>(source: S) -> Self
  where
    S: Stream<Item = std::io::Result<SseEvent>> + Send + 'static,
  {
    Self {
      source: Box::pin(source),
      transformers: Vec::new(),
      observers: Vec::new(),
    }
  }

  pub fn with_transformer<T>(mut self, transformer: T) -> Self
  where
    T: EventTransformer + 'static,
  {
    self.transformers.push(Box::new(transformer));
    self
  }

  pub fn with_observer<O>(mut self, observer: O) -> Self
  where
    O: EventObserver + 'static,
  {
    self.observers.push(Box::new(observer));
    self
  }

  pub fn run(self) -> ByteStream {
    Box::pin(StreamWithFinalizer::new(
      PipelineStream::new(self.source, self.transformers, self.observers),
      finalize_observers,
    ))
  }
}

struct PipelineStream {
  source: EventStream,
  transformers: Vec<Box<dyn EventTransformer>>,
  observers: Vec<Box<dyn EventObserver>>,
  pending: VecDeque<std::io::Result<Bytes>>,
  source_done: bool,
}

impl PipelineStream {
  fn new(
    source: EventStream,
    transformers: Vec<Box<dyn EventTransformer>>,
    observers: Vec<Box<dyn EventObserver>>,
  ) -> Self {
    Self {
      source,
      transformers,
      observers,
      pending: VecDeque::new(),
      source_done: false,
    }
  }

  fn process_event(&mut self, event: SseEvent) -> std::io::Result<()> {
    for event in self.apply_transformers(vec![event], 0)? {
      let encoded = encode_event(&event);
      for observer in &mut self.observers {
        observer.observe(&event, &encoded);
      }
      if !encoded.is_empty() {
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
        let encoded = encode_event(&event);
        for observer in &mut self.observers {
          observer.observe(&event, &encoded);
        }
        if !encoded.is_empty() {
          self.pending.push_back(Ok(encoded));
        }
      }
    }
    Ok(())
  }

  fn observe_error(&mut self, err: &std::io::Error) {
    for observer in &mut self.observers {
      observer.on_error(err);
    }
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
            self.observe_error(&err);
            self.pending.push_back(Err(err));
            self.source_done = true;
          }
        }
        Poll::Ready(Some(Err(err))) => {
          self.observe_error(&err);
          self.pending.push_back(Err(err));
          self.source_done = true;
        }
        Poll::Ready(None) => {
          if let Err(err) = self.finish_transformers() {
            self.observe_error(&err);
            self.pending.push_back(Err(err));
          }
          self.source_done = true;
        }
      }
    }
  }
}

fn finalize_observers(stream: &mut PipelineStream) {
  for observer in &mut stream.observers {
    observer.finish();
  }
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
  use bytes::{Bytes, BytesMut};
  use futures_util::{stream, StreamExt};
  use std::sync::{Arc, Mutex};

  struct AppendTransformer(&'static str);

  impl EventTransformer for AppendTransformer {
    fn transform(&mut self, mut event: SseEvent) -> Result<Vec<SseEvent>> {
      if !event.is_done() {
        event.data.push_str(self.0);
      }
      Ok(vec![event])
    }
  }

  struct CaptureObserver(Arc<Mutex<Vec<String>>>);

  impl EventObserver for CaptureObserver {
    fn observe(&mut self, event: &SseEvent, _encoded: &Bytes) {
      self.0.lock().unwrap().push(event.data.clone());
    }
  }

  #[test]
  fn pipeline_applies_transformers_in_order() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_in_pipeline = seen.clone();
    let body = futures::executor::block_on(async move {
      SsePipeline::from_stream(stream::iter(vec![
        Ok(SseEvent::raw(None, "hello".into())),
        Ok(SseEvent::done()),
      ]))
      .with_transformer(AppendTransformer("-a"))
      .with_transformer(AppendTransformer("-b"))
      .with_observer(CaptureObserver(seen_in_pipeline))
      .run()
      .collect::<Vec<_>>()
      .await
      .into_iter()
      .collect::<Result<Vec<_>, _>>()
      .unwrap()
      .into_iter()
      .fold(BytesMut::new(), |mut out, chunk| {
        out.extend_from_slice(&chunk);
        out
      })
      .freeze()
    });
    let text = std::str::from_utf8(&body).unwrap();
    assert!(text.contains("data: hello-a-b"));
    assert!(text.contains("data: [DONE]"));
    assert_eq!(seen.lock().unwrap().as_slice(), ["hello-a-b", "[DONE]"]);
  }
}
