use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::Utc;
use parking_lot::Mutex;
use rusqlite::Connection;

mod chat;
pub mod logging;
mod requests;
mod usage;

use chat::ChatTable;
use requests::RequestsTable;
use usage::UsageTable;

pub use chat::{ChatHistoryRecord, ChatMessageRecord, ConversationMessageView, ConversationView};
pub use requests::{RequestRecordCompleted, RequestRecordFailed, RequestRecordStart};
pub use usage::{TokenUsage, UsageRecord};

pub(super) type SharedConn = Arc<Mutex<Connection>>;

#[derive(Clone)]
pub struct RequestStore {
  requests: Arc<RequestsTable>,
  chat: Arc<ChatTable>,
  usage: Arc<UsageTable>,
}

impl RequestStore {
  pub fn new(db_path: &Path) -> Result<Self> {
    let conn: SharedConn = Arc::new(Mutex::new(Connection::open(db_path)?));

    Ok(Self {
      requests: Arc::new(RequestsTable::new(conn.clone())?),
      chat: Arc::new(ChatTable::new(conn.clone())?),
      usage: Arc::new(UsageTable::new(conn)?),
    })
  }

  pub fn record_request_started(&self, input: RequestRecordStart) -> Result<()> {
    self.requests.record_request_started(input)
  }

  pub fn record_request_completed(&self, input: RequestRecordCompleted) -> Result<()> {
    self.requests.record_request_completed(input)
  }

  pub fn record_request_failed(&self, input: RequestRecordFailed) -> Result<()> {
    self.requests.record_request_failed(input)
  }

  pub fn record_chat_history(&self, input: ChatHistoryRecord) -> Result<()> {
    self.chat.record_chat_history(input)
  }

  pub fn append_chat_message(
    &self,
    conversation_id: &str,
    created_at: chrono::DateTime<Utc>,
    role: &str,
    content_text: &str,
    raw_json: &str,
  ) -> Result<()> {
    self
      .chat
      .append_chat_message(conversation_id, created_at, role, content_text, raw_json)
  }

  pub fn apply_usage(&self, input: UsageRecord) -> Result<()> {
    self.usage.apply_usage(input)
  }

  pub fn prune_older_than_days(&self, days: i64) -> Result<()> {
    let cutoff = Utc::now() - chrono::Duration::days(days);
    let cutoff_ts = cutoff.to_rfc3339();
    self.requests.prune_older_than(&cutoff_ts)?;
    self.chat.prune_older_than(&cutoff_ts)
  }

  pub fn query_conversations(&self, limit: usize) -> Result<Vec<ConversationView>> {
    self.chat.query_conversations(limit)
  }

  pub fn start_retention_task(&self, days: i64, every: Duration) {
    let this = self.clone();
    tokio::spawn(async move {
      let mut timer = tokio::time::interval(every);
      loop {
        timer.tick().await;
        if let Err(err) = this.prune_older_than_days(days) {
          tracing::warn!(
            target: "persistence",
            error = %err,
            "failed to prune old request archive rows"
          );
        }
      }
    });
  }
}
