#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::net::SocketAddr;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use llm_router_core as core;
use serde::Deserialize;
use tauri::Manager;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;
use url::Url;

mod tauri_api;

#[derive(Debug, Clone)]
struct AppConfig {
  default_port: u16,
  log_level_filter: String,
  retention_days: i64,
  request_retention_days: i64,
  https_proxy: Option<String>,
}

impl Default for AppConfig {
  fn default() -> Self {
    Self {
      default_port: 11434,
      log_level_filter: "info".to_string(),
      retention_days: 7,
      request_retention_days: 30,
      https_proxy: None,
    }
  }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct AppConfigFile {
  default_port: Option<u16>,
  log_level_filter: Option<String>,
  retention_days: Option<i64>,
  request_retention_days: Option<i64>,
  https_proxy: Option<String>,
}

fn load_app_config(config_dir: &Path) -> AppConfig {
  let mut cfg = AppConfig::default();
  let path = config_dir.join("config.yaml");
  let content = match std::fs::read_to_string(&path) {
    Ok(content) => content,
    Err(err) => {
      if err.kind() != std::io::ErrorKind::NotFound {
        eprintln!("failed to read {}: {err}", path.display());
      }
      return cfg;
    }
  };

  match serde_yaml::from_str::<AppConfigFile>(&content) {
    Ok(file_cfg) => {
      if let Some(v) = file_cfg.default_port {
        cfg.default_port = v;
      }
      if let Some(v) = file_cfg.log_level_filter {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
          cfg.log_level_filter = trimmed.to_string();
        }
      }
      if let Some(v) = file_cfg.retention_days {
        cfg.retention_days = v.max(1);
      }
      if let Some(v) = file_cfg.request_retention_days {
        cfg.request_retention_days = v.max(1);
      }
      if let Some(v) = file_cfg.https_proxy {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
          cfg.https_proxy = Some(trimmed.to_string());
        }
      }
    }
    Err(err) => {
      eprintln!("failed to parse {}: {err}", path.display());
    }
  }

  cfg
}

#[tokio::main]
async fn main() {
  let config_dir = PathBuf::from("config");
  let app_config = load_app_config(&config_dir);
  let configured_proxy = apply_proxy_env(&app_config);
  let state = core::build_state(config_dir, app_config.retention_days, app_config.request_retention_days)
    .await
    .expect("state initialization failed");
  let env_filter = match std::env::var("RUST_LOG") {
    Ok(raw) => EnvFilter::new(raw),
    Err(_) => EnvFilter::new(app_config.log_level_filter.clone()),
  };
  tracing_subscriber::registry()
    .with(env_filter)
    .with(tracing_subscriber::fmt::layer())
    .with(state.log_layer())
    .init();
  if let Some(proxy) = configured_proxy {
    tracing::info!(target: "config", proxy = %proxy, "configured HTTPS proxy from config.yaml");
  }

  if let Err(err) = start_router_with_fallback(Arc::clone(&state), app_config.default_port).await {
    tracing::error!("failed to start embedded router server: {err}");
  }

  tauri::Builder::default()
    .manage(Arc::clone(&state))
    .invoke_handler(tauri::generate_handler![
      tauri_api::get_provider_status,
      tauri_api::get_model_list,
      tauri_api::get_active_config,
      tauri_api::set_app_config,
      tauri_api::list_accounts,
      tauri_api::connect_account,
      tauri_api::disconnect_account,
      tauri_api::set_default_account,
      tauri_api::update_account,
      tauri_api::set_provider_enabled,
      tauri_api::set_model_enabled,
      tauri_api::get_request_logs,
      tauri_api::get_chat_conversations,
      tauri_api::get_login_status,
      tauri_api::copilot_login,
      tauri_api::copilot_complete_login,
      tauri_api::copilot_refresh_api_key,
      tauri_api::copilot_logout,
      tauri_api::codex_login,
      tauri_api::codex_complete_login,
      tauri_api::codex_refresh_api_key,
      tauri_api::codex_logout,
      tauri_api::get_account_information,
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

fn apply_proxy_env(app_config: &AppConfig) -> Option<String> {
  let Some(proxy) = app_config.https_proxy.as_ref() else {
    return None;
  };
  // SAFETY: This runs during startup before background tasks and request clients are spawned.
  unsafe {
    std::env::set_var("HTTPS_PROXY", proxy);
    std::env::set_var("https_proxy", proxy);
  }
  Some(redact_proxy_for_log(proxy))
}

fn redact_proxy_for_log(input: &str) -> String {
  match Url::parse(input.trim()) {
    Ok(parsed) => {
      let scheme = parsed.scheme();
      let host = parsed.host_str().unwrap_or("unknown");
      let port = parsed.port().map(|v| format!(":{v}")).unwrap_or_default();
      format!("{scheme}://{host}{port}")
    }
    Err(_) => "invalid_proxy_url".to_string(),
  }
}

async fn start_router_with_fallback(
  state: Arc<core::app_state::AppState>,
  default_port: u16,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
  let primary = SocketAddr::from(([127, 0, 0, 1], default_port));
  match state.router_server().start(primary, Arc::clone(&state)).await {
    Ok(_) => {
      tracing::info!("embedded router listening on {primary}");
      return Ok(());
    }
    Err(err) => {
      if let Some(io_err) = err.downcast_ref::<std::io::Error>() {
        if io_err.kind() == std::io::ErrorKind::AddrInUse {
          let fallback_port = default_port.saturating_add(1);
          let fallback = SocketAddr::from(([127, 0, 0, 1], fallback_port));
          tracing::warn!(
            "router port {} already in use, trying fallback port {}",
            default_port,
            fallback_port
          );
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
