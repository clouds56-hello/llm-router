use std::net::SocketAddr;
use std::sync::Arc;

use serde::Deserialize;
use serde_json::Value;

use llm_router_core::app_state::AppState;
use llm_router_core::auth::copilot::{
  DeviceAuthCompleteRequest, DeviceAuthCompleteResponse, DeviceAuthStartRequest, DeviceAuthStartResponse,
};
use llm_router_core::config::{AccountView, ConnectAccountInput, UpdateAccountInput};
use llm_router_core::logging::LogQuery;
use llm_router_core::persistence::ConversationView;

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
    .iter()
    .map(|m| {
      serde_json::json!({
          "name": m.openai_name,
          "provider": m.provider,
          "provider_model": m.provider_model,
          "is_default": m.is_default,
          "enabled": loaded.is_model_enabled(&m.openai_name),
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
pub async fn list_accounts(state: tauri::State<'_, Arc<AppState>>) -> Result<Vec<AccountView>, String> {
  Ok(state.config().list_accounts())
}

#[tauri::command]
pub async fn connect_account(
  state: tauri::State<'_, Arc<AppState>>,
  request: ConnectAccountInput,
) -> Result<AccountView, String> {
  state.config().connect_account(request).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct DisconnectAccountRequest {
  pub provider: String,
  pub account_id: String,
}

#[tauri::command]
pub async fn disconnect_account(
  state: tauri::State<'_, Arc<AppState>>,
  request: DisconnectAccountRequest,
) -> Result<(), String> {
  state
    .config()
    .disconnect_account(&request.provider, &request.account_id)
    .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct SetDefaultAccountRequest {
  pub provider: String,
  pub account_id: String,
}

#[tauri::command]
pub async fn set_default_account(
  state: tauri::State<'_, Arc<AppState>>,
  request: SetDefaultAccountRequest,
) -> Result<(), String> {
  state
    .config()
    .set_default_account(&request.provider, &request.account_id)
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn update_account(
  state: tauri::State<'_, Arc<AppState>>,
  request: UpdateAccountInput,
) -> Result<AccountView, String> {
  state.config().update_account(request).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct SetProviderEnabledRequest {
  pub provider: String,
  pub enabled: bool,
}

#[tauri::command]
pub async fn set_provider_enabled(
  state: tauri::State<'_, Arc<AppState>>,
  request: SetProviderEnabledRequest,
) -> Result<(), String> {
  state
    .config()
    .set_provider_enabled(&request.provider, request.enabled)
    .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct SetModelEnabledRequest {
  pub openai_name: String,
  pub enabled: bool,
}

#[tauri::command]
pub async fn set_model_enabled(
  state: tauri::State<'_, Arc<AppState>>,
  request: SetModelEnabledRequest,
) -> Result<(), String> {
  state
    .config()
    .set_model_enabled(&request.openai_name, request.enabled)
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn get_request_logs(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<LogQueryRequest>,
) -> Result<Value, String> {
  let request = request.unwrap_or_default();
  let logs = state
    .logs()
    .query(LogQuery {
      limit: request.limit,
      level: request.level,
      request_id: request.request_id,
    })
    .map_err(|e| e.to_string())?;
  Ok(serde_json::json!({ "logs": logs }))
}

#[derive(Debug, Default, Deserialize)]
pub struct ConversationQueryRequest {
  pub limit: Option<usize>,
}

#[tauri::command]
pub async fn get_chat_conversations(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<ConversationQueryRequest>,
) -> Result<Vec<ConversationView>, String> {
  let request = request.unwrap_or_default();
  state
    .requests()
    .query_conversations(request.limit.unwrap_or(100))
    .map_err(|e| e.to_string())
}

#[derive(Debug, Default, Deserialize)]
pub struct LogQueryRequest {
  pub limit: Option<usize>,
  pub level: Option<String>,
  pub request_id: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct CopilotRefreshApiKeyRequest {
  pub account_id: Option<String>,
}

#[tauri::command]
pub async fn copilot_refresh_api_key(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<CopilotRefreshApiKeyRequest>,
) -> Result<Value, String> {
  let account_id = request.and_then(|r| r.account_id);
  let auth_state = state
    .copilot_auth()
    .refresh_api_key(account_id)
    .await
    .map_err(|e| e.to_string())?;
  Ok(serde_json::json!({ "copilot": auth_state }))
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
