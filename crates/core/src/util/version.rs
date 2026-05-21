pub const BASE: &str = env!("tokn_ROUTER_BASE_VERSION");
pub const COMMIT_ID: &str = env!("tokn_ROUTER_COMMIT_ID");
pub const FULL: &str = env!("tokn_ROUTER_VERSION");

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
  option_env!("tokn_ROUTER_VERSION_DIRTY") == Some("1")
}

pub fn tokn_router_user_agent() -> String {
  format!("tokn-router/{FULL}")
}
