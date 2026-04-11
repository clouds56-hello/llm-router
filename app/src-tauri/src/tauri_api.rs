use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

use llm_router_core::app_state::AppState;
use llm_router_core::auth::codex::{
  DeviceAuthCompleteRequest as CodexDeviceAuthCompleteRequest,
  DeviceAuthCompleteResponse as CodexDeviceAuthCompleteResponse, DeviceAuthStartRequest as CodexDeviceAuthStartRequest,
  DeviceAuthStartResponse as CodexDeviceAuthStartResponse,
};
use llm_router_core::auth::copilot::{
  DeviceAuthCompleteRequest, DeviceAuthCompleteResponse, DeviceAuthStartRequest, DeviceAuthStartResponse,
};
use llm_router_core::config::{AccountView, ConnectAccountInput, UpdateAccountInput};
use llm_router_core::db::logging::LogQuery;
use llm_router_core::db::AccountInformationView;
use llm_router_core::db::ConversationView;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct RuntimeAppConfigFile {
  default_port: Option<u16>,
  log_level_filter: Option<String>,
  retention_days: Option<i64>,
  request_retention_days: Option<i64>,
  https_proxy: Option<String>,
}

fn runtime_config_path() -> PathBuf {
  PathBuf::from("config").join("config.yaml")
}

fn read_runtime_app_config() -> Result<RuntimeAppConfigFile, String> {
  let path = runtime_config_path();
  match std::fs::read_to_string(&path) {
    Ok(content) => serde_yaml::from_str::<RuntimeAppConfigFile>(&content)
      .map_err(|e| format!("failed to parse {}: {e}", path.display())),
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(RuntimeAppConfigFile::default()),
    Err(err) => Err(format!("failed to read {}: {err}", path.display())),
  }
}

fn write_runtime_app_config(cfg: &RuntimeAppConfigFile) -> Result<(), String> {
  let path = runtime_config_path();
  let bytes = serde_yaml::to_string(cfg).map_err(|e| format!("failed to serialize runtime config: {e}"))?;
  std::fs::write(&path, bytes).map_err(|e| format!("failed to write {}: {e}", path.display()))
}

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
  let app_config = read_runtime_app_config()?;
  Ok(serde_json::json!({
      "providers": loaded.providers,
      "models": loaded.models,
      "credentials": loaded.credentials,
      "app_config": app_config,
      "last_error": state.config().last_error(),
  }))
}

#[derive(Debug, Deserialize, Default)]
pub struct SetAppConfigRequest {
  pub default_port: Option<u16>,
  pub log_level_filter: Option<String>,
  pub retention_days: Option<i64>,
  pub request_retention_days: Option<i64>,
  pub https_proxy: Option<String>,
}

#[tauri::command]
pub async fn set_app_config(
  _state: tauri::State<'_, Arc<AppState>>,
  request: SetAppConfigRequest,
) -> Result<Value, String> {
  let mut cfg = read_runtime_app_config()?;

  if let Some(v) = request.default_port {
    cfg.default_port = Some(v);
  }

  if let Some(v) = request.log_level_filter {
    let value = v.trim();
    if value.is_empty() {
      return Err("log_level_filter cannot be empty".to_string());
    }
    cfg.log_level_filter = Some(value.to_string());
  }

  if let Some(v) = request.retention_days {
    cfg.retention_days = Some(v.max(1));
  }

  if let Some(v) = request.request_retention_days {
    cfg.request_retention_days = Some(v.max(1));
  }

  if let Some(v) = request.https_proxy {
    let trimmed = v.trim().to_string();
    cfg.https_proxy = if trimmed.is_empty() { None } else { Some(trimmed) };
  }

  write_runtime_app_config(&cfg)?;
  tracing::info!(
    target: "config",
    default_port = cfg.default_port.unwrap_or(11434),
    log_level_filter = cfg.log_level_filter.as_deref().unwrap_or("info"),
    retention_days = cfg.retention_days.unwrap_or(7),
    request_retention_days = cfg.request_retention_days.unwrap_or(30),
    has_https_proxy = cfg.https_proxy.is_some(),
    "updated app config in config.yaml"
  );
  Ok(serde_json::json!({
    "ok": true,
    "app_config": cfg,
    "note": "restart app to ensure all runtime components pick up config changes"
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
    .map_err(|e| e.to_string())?;
  state
    .requests()
    .mark_account_information_disconnected(&request.provider, &request.account_id)
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
  let codex = state.codex_auth().status().map_err(|e| e.to_string())?;

  Ok(serde_json::json!({"copilot": status, "codex": codex}))
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
pub async fn codex_login(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<CodexDeviceAuthStartRequest>,
) -> Result<CodexDeviceAuthStartResponse, String> {
  state
    .codex_auth()
    .start_device_authorization(request.unwrap_or_default())
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn codex_complete_login(
  state: tauri::State<'_, Arc<AppState>>,
  request: CodexDeviceAuthCompleteRequest,
) -> Result<CodexDeviceAuthCompleteResponse, String> {
  state
    .codex_auth()
    .complete_device_authorization(request)
    .await
    .map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize)]
pub struct CodexRefreshApiKeyRequest {
  pub account_id: Option<String>,
}

#[tauri::command]
pub async fn codex_refresh_api_key(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<CodexRefreshApiKeyRequest>,
) -> Result<Value, String> {
  let account_id = request.and_then(|r| r.account_id);
  let auth_state = state
    .codex_auth()
    .refresh_api_key(account_id)
    .await
    .map_err(|e| e.to_string())?;
  Ok(serde_json::json!({ "codex": auth_state }))
}

#[tauri::command]
pub async fn codex_logout(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
  state.codex_auth().logout().map_err(|e| e.to_string())
}

#[derive(Debug, Default, Deserialize)]
pub struct AccountInformationQueryRequest {
  pub provider: Option<String>,
  pub account_id: Option<String>,
}

#[tauri::command]
pub async fn get_account_information(
  state: tauri::State<'_, Arc<AppState>>,
  request: Option<AccountInformationQueryRequest>,
) -> Result<Vec<AccountInformationView>, String> {
  let req = request.unwrap_or_default();
  state
    .requests()
    .list_account_information(req.provider.as_deref(), req.account_id.as_deref())
    .map_err(|e| e.to_string())
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
