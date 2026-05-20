pub const BASE: &str = env!("LLM_ROUTER_BASE_VERSION");
pub const COMMIT_ID: &str = env!("LLM_ROUTER_COMMIT_ID");
pub const FULL: &str = env!("LLM_ROUTER_VERSION");

pub fn base() -> &'static str {
  BASE
}

pub fn commit_id() -> &'static str {
  COMMIT_ID
}

pub fn full() -> &'static str {
  FULL
}

pub fn is_dirty() -> bool {
  option_env!("LLM_ROUTER_VERSION_DIRTY") == Some("1")
}

pub fn llm_router_user_agent() -> String {
  component_version("llm-router")
}

pub fn copilot_user_agent() -> String {
  component_version("GitHubCopilotChat")
}

pub fn copilot_editor_version() -> String {
  component_version("vscode")
}

pub fn copilot_editor_plugin_version() -> String {
  component_version("copilot-chat")
}

pub fn component_version(component: &str) -> String {
  format!("{component}/{FULL}")
}
