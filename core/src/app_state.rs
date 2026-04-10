use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::auth::copilot::CopilotAuthManager;
use crate::config::ConfigManager;
use crate::logging::{InMemoryLogSink, LogSink};
use crate::providers::ProviderRegistry;
use crate::router;

#[derive(Clone)]
pub struct AppState {
  config: ConfigManager,
  providers: ProviderRegistry,
  logs: InMemoryLogSink,
  copilot_auth: CopilotAuthManager,
  router_server: RouterServerManager,
}

impl AppState {
  pub async fn new(config_dir: PathBuf) -> Result<Self> {
    let logs = InMemoryLogSink::new(2_000);
    let config = ConfigManager::new(config_dir.clone(), logs.clone())?;
    let providers = ProviderRegistry::new();
    Self::new_with_registry(config_dir, config, logs, providers).await
  }

  pub async fn new_for_tests(config_dir: PathBuf, providers: ProviderRegistry) -> Result<Self> {
    let logs = InMemoryLogSink::new(2_000);
    let config = ConfigManager::new(config_dir.clone(), logs.clone())?;
    Self::new_with_registry(config_dir, config, logs, providers).await
  }

  async fn new_with_registry(
    config_dir: PathBuf,
    config: ConfigManager,
    logs: InMemoryLogSink,
    providers: ProviderRegistry,
  ) -> Result<Self> {
    let copilot_auth = CopilotAuthManager::new(config_dir);

    Ok(Self {
      config,
      providers,
      logs,
      copilot_auth,
      router_server: RouterServerManager::new(),
    })
  }

  pub fn config(&self) -> &ConfigManager {
    &self.config
  }

  pub fn providers(&self) -> &ProviderRegistry {
    &self.providers
  }

  pub fn logs(&self) -> &dyn LogSink {
    &self.logs
  }

  pub fn log_sink(&self) -> &InMemoryLogSink {
    &self.logs
  }

  pub fn copilot_auth(&self) -> &CopilotAuthManager {
    &self.copilot_auth
  }

  pub fn router_server(&self) -> &RouterServerManager {
    &self.router_server
  }

  pub async fn start_config_hot_reload(&self) -> Result<()> {
    self.config.start_hot_reload()
  }
}

#[derive(Debug, Clone, Serialize)]
pub struct RouterServerState {
  pub running: bool,
  pub addr: Option<String>,
}

#[derive(Default)]
struct RouterServerRuntime {
  addr: Option<SocketAddr>,
  shutdown_tx: Option<oneshot::Sender<()>>,
  join_handle: Option<JoinHandle<()>>,
}

#[derive(Clone, Default)]
pub struct RouterServerManager {
  inner: Arc<Mutex<RouterServerRuntime>>,
}

impl RouterServerManager {
  pub fn new() -> Self {
    Self::default()
  }

  pub async fn start(&self, addr: SocketAddr, app_state: Arc<AppState>) -> Result<()> {
    {
      let guard = self.inner.lock();
      if guard.join_handle.is_some() {
        return Ok(());
      }
    }

    let (tx, rx) = oneshot::channel::<()>();
    let app = router::build_router(app_state);
    let listener = tokio::net::TcpListener::bind(addr).await?;

    let handle = tokio::spawn(async move {
      let _ = axum::serve(listener, app)
        .with_graceful_shutdown(async {
          let _ = rx.await;
        })
        .await;
    });

    let mut guard = self.inner.lock();
    if guard.join_handle.is_some() {
      let _ = tx.send(());
      return Ok(());
    }
    guard.addr = Some(addr);
    guard.shutdown_tx = Some(tx);
    guard.join_handle = Some(handle);
    Ok(())
  }

  pub async fn stop(&self) -> Result<()> {
    let (tx, handle) = {
      let mut guard = self.inner.lock();
      let tx = guard.shutdown_tx.take();
      let handle = guard.join_handle.take();
      guard.addr = None;
      (tx, handle)
    };

    if let Some(tx) = tx {
      let _ = tx.send(());
    }

    if let Some(handle) = handle {
      let _ = handle.await;
    }

    Ok(())
  }

  pub fn state(&self) -> RouterServerState {
    let guard = self.inner.lock();
    RouterServerState {
      running: guard.join_handle.is_some(),
      addr: guard.addr.map(|a| a.to_string()),
    }
  }
}
