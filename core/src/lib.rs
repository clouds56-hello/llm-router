pub mod app_state;
pub mod auth;
pub mod config;
pub mod db;
pub mod providers;
pub mod router;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use app_state::AppState;

pub async fn build_state(
  config_dir: PathBuf,
  log_retention_days: i64,
  request_retention_days: i64,
) -> anyhow::Result<Arc<AppState>> {
  let state = AppState::new(config_dir, log_retention_days, request_retention_days)
    .await
    .context("failed to initialize application state")?;
  Ok(Arc::new(state))
}
