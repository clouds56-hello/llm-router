use super::recording::CallRecordBuilder;
use super::usage::parse_usage_any_value;
use crate::db::CallRecord;
use bytes::Bytes;
use llm_convert::sse::{EventObserver, SseEvent};
use llm_core::pipeline::RequestReporter;
use std::sync::Arc;

#[derive(Clone)]
pub(super) struct SharedUsage(pub Arc<parking_lot::Mutex<(Option<u64>, Option<u64>)>>);

impl SharedUsage {
  pub(super) fn new() -> Self {
    Self(Arc::new(parking_lot::Mutex::new((None, None))))
  }

  pub(super) fn get(&self) -> (Option<u64>, Option<u64>) {
    *self.0.lock()
  }
}

pub(super) struct UsageObserver {
  usage: SharedUsage,
}

impl UsageObserver {
  pub(super) fn new(usage: SharedUsage) -> Self {
    Self { usage }
  }
}

impl EventObserver for UsageObserver {
  fn observe(&mut self, event: &SseEvent, _encoded: &Bytes) {
    let Some(value) = event.json.as_ref() else { return };
    let (prompt_tokens, completion_tokens) = parse_usage_any_value(value);
    if prompt_tokens.is_none() && completion_tokens.is_none() {
      return;
    }
    let mut usage = self.usage.0.lock();
    if prompt_tokens.is_some() {
      usage.0 = prompt_tokens;
    }
    if completion_tokens.is_some() {
      usage.1 = completion_tokens;
    }
  }
}

#[derive(Clone)]
pub(super) struct SharedBody(pub Arc<parking_lot::Mutex<Vec<u8>>>);

impl SharedBody {
  pub(super) fn new() -> Self {
    Self(Arc::new(parking_lot::Mutex::new(Vec::new())))
  }

  pub(super) fn bytes(&self) -> Bytes {
    Bytes::from(self.0.lock().clone())
  }
}

pub(super) struct BodyCaptureObserver {
  max_body: usize,
  body: SharedBody,
}

impl BodyCaptureObserver {
  pub(super) fn new(max_body: usize, body: SharedBody) -> Self {
    Self { max_body, body }
  }
}

impl EventObserver for BodyCaptureObserver {
  fn observe(&mut self, _event: &SseEvent, encoded: &Bytes) {
    if self.max_body == 0 {
      return;
    }
    let mut captured = self.body.0.lock();
    let remaining = self.max_body.saturating_sub(captured.len());
    if remaining > 0 {
      captured.extend_from_slice(&encoded[..encoded.len().min(remaining)]);
    }
  }
}

pub(super) struct RecordingObserver<F>
where
  F: FnOnce((Option<u64>, Option<u64>), Bytes, Option<&str>) -> CallRecord + Send + 'static,
{
  reporter: Arc<dyn RequestReporter>,
  usage: SharedUsage,
  body: SharedBody,
  failed: bool,
  record: Option<F>,
}

impl<F> RecordingObserver<F>
where
  F: FnOnce((Option<u64>, Option<u64>), Bytes, Option<&str>) -> CallRecord + Send + 'static,
{
  pub(super) fn new(reporter: Arc<dyn RequestReporter>, usage: SharedUsage, body: SharedBody, record: F) -> Self {
    Self {
      reporter,
      usage,
      body,
      failed: false,
      record: Some(record),
    }
  }

  fn report_once(&mut self) {
    let Some(record) = self.record.take() else { return };
    let request_error = self.failed.then_some("stream terminated before completion");
    let record = record(self.usage.get(), self.body.bytes(), request_error);
    self.reporter.report(record);
  }
}

impl<F> EventObserver for RecordingObserver<F>
where
  F: FnOnce((Option<u64>, Option<u64>), Bytes, Option<&str>) -> CallRecord + Send + 'static,
{
  fn on_error(&mut self, _err: &std::io::Error) {
    self.failed = true;
  }

  fn finish(&mut self) {
    self.report_once();
  }
}

pub(super) fn build_stream_record(
  builder: CallRecordBuilder,
  usage: (Option<u64>, Option<u64>),
  captured: Bytes,
  resp_headers: &reqwest::header::HeaderMap,
) -> CallRecord {
  builder
    .with_response_body(captured.clone())
    .with_outbound_response(Some(resp_headers), Some(&captured))
    .with_usage(usage.0, usage.1)
    .build()
}
