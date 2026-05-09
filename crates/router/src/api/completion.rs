use llm_core::event::{Event, EventBus};
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct CompletionGuard {
  events: Arc<EventBus>,
  request_id: String,
  started: Instant,
  total_attempts: u32,
  final_status: Option<u16>,
  error: Option<String>,
  armed: bool,
}

impl CompletionGuard {
  pub(crate) fn new(events: Arc<EventBus>, request_id: String, started: Instant) -> Self {
    Self {
      events,
      request_id,
      started,
      total_attempts: 1,
      final_status: None,
      error: Some("request failed before completion".to_string()),
      armed: true,
    }
  }

  pub(crate) fn attempt(&mut self, attempt: u32) {
    self.total_attempts = self.total_attempts.max(attempt + 1);
  }

  pub(crate) fn failure(&mut self, status: Option<u16>, error: impl Into<String>) {
    self.final_status = status;
    self.error = Some(error.into());
  }

  pub(crate) fn disarm(&mut self) {
    self.armed = false;
  }
}

impl Drop for CompletionGuard {
  fn drop(&mut self) {
    if !self.armed {
      return;
    }
    self.events.emit(Event::RequestCompleted {
      request_id: self.request_id.clone(),
      success: false,
      total_attempts: self.total_attempts,
      final_status: self.final_status,
      total_latency_ms: self.started.elapsed().as_millis() as u64,
      error: self.error.clone(),
    });
  }
}
