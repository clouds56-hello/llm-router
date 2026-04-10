use std::net::SocketAddr;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use crate::app_state::AppState;
use crate::auth::copilot::{
  DeviceAuthCompleteRequest, DeviceAuthCompleteResponse, DeviceAuthStartRequest, DeviceAuthStartResponse,
};

#[tauri::command]
pub async fn get_provider_status(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<Value>, String> {
  let loaded = state.config().get();
  Ok(state.providers().provider_status(&loaded))
}

#[tauri::command]
pub async fn get_model_list(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<Value>, String> {
  let loaded = state.config().get();
  let models = loaded
    .models
    .models
    .into_iter()
    .map(|m| {
      serde_json::json!({
          "name": m.openai_name,
          "provider": m.provider,
          "provider_model": m.provider_model,
          "is_default": m.is_default,
      })
    })
    .collect();
  Ok(models)
}

#[tauri::command]
pub async fn get_active_config(state: tauri::State<'_, Arc<AppState>>) -> Result<Value, String> {
  let loaded = state.config().get();
  Ok(serde_json::json!({
      "providers": loaded.providers,
      "models": loaded.models,
      "credentials": loaded.credentials,
      "last_error": state.config().last_error(),
  }))
}

#[tauri::command]
pub async fn get_request_logs(state: tauri::State<'_, Arc<AppState>>) -> Result<Value, String> {
  Ok(serde_json::json!({"logs": state.logs().list(500)}))
}

#[tauri::command]
pub async fn get_login_status(state: tauri::State<'_, Arc<AppState>>) -> Result<Value, String> {
  let status = state.copilot_auth().status().map_err(|e| e.to_string())?;

  Ok(serde_json::json!({"copilot": status}))
}

#[tauri::command]
pub async fn copilot_login(
  state: tauri::State<'_, Arc<AppState>>,
  request: DeviceAuthStartRequest,
) -> Result<DeviceAuthStartResponse, String> {
  state
    .copilot_auth()
    .start_device_authorization(request)
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn copilot_complete_login(
  state: tauri::State<'_, Arc<AppState>>,
  request: DeviceAuthCompleteRequest,
) -> Result<DeviceAuthCompleteResponse, String> {
  state
    .copilot_auth()
    .complete_device_authorization(request)
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn copilot_logout(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
  state.copilot_auth().logout().map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_router_state(state: tauri::State<'_, Arc<AppState>>) -> Result<Value, String> {
  Ok(serde_json::json!(state.router_server().state()))
}

#[derive(Debug, Deserialize)]
pub struct StartRouterRequest {
  pub host: Option<String>,
  pub port: Option<u16>,
}

#[tauri::command]
pub async fn start_router_server(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<StartRouterRequest>,
) -> Result<Value, String> {
  let req = request.unwrap_or(StartRouterRequest {
    host: Some("127.0.0.1".to_string()),
    port: Some(11434),
  });
  let host = req.host.unwrap_or_else(|| "127.0.0.1".to_string());
  let port = req.port.unwrap_or(11434);
  let addr: SocketAddr = format!("{host}:{port}")
    .parse()
    .map_err(|e| format!("invalid host/port: {e}"))?;

  state
    .router_server()
    .start(addr, Arc::clone(state.inner()))
    .await
    .map_err(|e| e.to_string())?;

  Ok(serde_json::json!(state.router_server().state()))
}

#[tauri::command]
pub async fn stop_router_server(state: tauri::State<'_, Arc<AppState>>) -> Result<Value, String> {
  state.router_server().stop().await.map_err(|e| e.to_string())?;

  Ok(serde_json::json!(state.router_server().state()))
}
