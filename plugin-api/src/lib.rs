use std::sync::Arc;

use async_trait::async_trait;
use router_core::{OpenAiChatChunk, OpenAiChatCompletionRequest, RequestContext, RouterError};
use tokio::sync::mpsc;
use tracing::warn;

#[derive(Debug, Clone)]
pub enum EventRecord {
  RequestStart {
    ctx: RequestContext,
    req: OpenAiChatCompletionRequest,
  },
  StreamChunk {
    ctx: RequestContext,
    chunk: OpenAiChatChunk,
  },
  RequestEnd {
    ctx: RequestContext,
    status_code: u16,
    latency_ms: u128,
  },
  RequestError {
    ctx: RequestContext,
    error: RouterError,
    latency_ms: u128,
  },
}

#[async_trait]
pub trait Plugin: Send + Sync {
  fn name(&self) -> &'static str;
  async fn on_event(&self, event: EventRecord);
}

#[derive(Clone)]
pub struct PluginManager {
  sender: mpsc::Sender<EventRecord>,
}

impl PluginManager {
  pub fn new(plugins: Vec<Arc<dyn Plugin>>, queue_capacity: usize) -> Self {
    let (tx, mut rx) = mpsc::channel::<EventRecord>(queue_capacity);
    tokio::spawn(async move {
      while let Some(event) = rx.recv().await {
        for plugin in &plugins {
          plugin.on_event(event.clone()).await;
        }
      }
    });
    Self { sender: tx }
  }

  pub fn emit(&self, event: EventRecord) {
    if let Err(err) = self.sender.try_send(event) {
      warn!("plugin queue full or closed: {}", err);
    }
  }
}
