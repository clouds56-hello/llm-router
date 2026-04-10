use std::collections::VecDeque;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
  pub id: String,
  pub ts: DateTime<Utc>,
  pub level: String,
  pub target: String,
  pub message: String,
}

pub trait LogSink: Send + Sync {
  fn push(&self, entry: LogEntry);
  fn list(&self, limit: usize) -> Vec<LogEntry>;
}

#[derive(Clone)]
pub struct InMemoryLogSink {
  max: usize,
  inner: Arc<Mutex<VecDeque<LogEntry>>>,
}

impl InMemoryLogSink {
  pub fn new(max: usize) -> Self {
    Self {
      max,
      inner: Arc::new(Mutex::new(VecDeque::with_capacity(max))),
    }
  }

  pub fn info(&self, target: impl Into<String>, message: impl Into<String>) {
    self.push(LogEntry {
      id: uuid::Uuid::new_v4().to_string(),
      ts: Utc::now(),
      level: "INFO".to_string(),
      target: target.into(),
      message: message.into(),
    });
  }

  pub fn error(&self, target: impl Into<String>, message: impl Into<String>) {
    self.push(LogEntry {
      id: uuid::Uuid::new_v4().to_string(),
      ts: Utc::now(),
      level: "ERROR".to_string(),
      target: target.into(),
      message: message.into(),
    });
  }
}

impl LogSink for InMemoryLogSink {
  fn push(&self, entry: LogEntry) {
    let mut guard = self.inner.lock();
    guard.push_front(entry);
    while guard.len() > self.max {
      guard.pop_back();
    }
  }

  fn list(&self, limit: usize) -> Vec<LogEntry> {
    self.inner.lock().iter().take(limit).cloned().collect()
  }
}
