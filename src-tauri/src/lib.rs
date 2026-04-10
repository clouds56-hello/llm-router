pub mod app_state;
pub mod auth;
pub mod config;
pub mod logging;
pub mod providers;
pub mod router;
pub mod tauri_api;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use app_state::AppState;

pub async fn build_state(config_dir: PathBuf) -> anyhow::Result<Arc<AppState>> {
  let state = AppState::new(config_dir)
    .await
    .context("failed to initialize application state")?;
  Ok(Arc::new(state))
}
