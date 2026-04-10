#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use llm_router_core as core;
use tauri::Manager;
use tracing_subscriber::EnvFilter;

mod tauri_api;

#[tokio::main]
async fn main() {
  tracing_subscriber::fmt()
    .with_env_filter(EnvFilter::from_default_env())
    .init();

  let config_dir = PathBuf::from("config");
  let state = core::build_state(config_dir)
    .await
    .expect("state initialization failed");

  if let Err(err) = start_router_with_fallback(Arc::clone(&state)).await {
    tracing::error!("failed to start embedded router server: {err}");
  }

  tauri::Builder::default()
    .manage(Arc::clone(&state))
    .invoke_handler(tauri::generate_handler![
      tauri_api::get_provider_status,
      tauri_api::get_model_list,
      tauri_api::get_active_config,
      tauri_api::list_accounts,
      tauri_api::connect_account,
      tauri_api::disconnect_account,
      tauri_api::set_default_account,
      tauri_api::update_account,
      tauri_api::set_provider_enabled,
      tauri_api::set_model_enabled,
      tauri_api::get_request_logs,
      tauri_api::get_login_status,
      tauri_api::copilot_login,
      tauri_api::copilot_complete_login,
      tauri_api::copilot_logout,
      tauri_api::get_router_state,
      tauri_api::start_router_server,
      tauri_api::stop_router_server,
    ])
    .setup(|app| {
      let state: tauri::State<Arc<core::app_state::AppState>> = app.state();
      let app_state: Arc<core::app_state::AppState> = Arc::clone(state.inner());
      tauri::async_runtime::spawn(async move {
        if let Err(err) = app_state.start_config_hot_reload().await {
          tracing::error!("hot reload failed to start: {err}");
        }
      });
      Ok(())
    })
    .run(tauri::generate_context!())
    .expect("error while running tauri application");
}

async fn start_router_with_fallback(
  state: Arc<core::app_state::AppState>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let primary = SocketAddr::from(([127, 0, 0, 1], 11434));
  match state.router_server().start(primary, Arc::clone(&state)).await {
    Ok(_) => {
      tracing::info!("embedded router listening on {primary}");
      return Ok(());
    }
    Err(err) => {
      if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        if io_err.kind() == std::io::ErrorKind::AddrInUse {
          let fallback = SocketAddr::from(([127, 0, 0, 1], 11435));
          tracing::warn!("router port 11434 already in use, trying fallback port 11435");
          state
            .router_server()
            .start(fallback, Arc::clone(&state))
            .await
            .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
          tracing::info!("embedded router listening on {fallback}");
          return Ok(());
        }
      }
      return Err(err.into());
    }
  }
}
