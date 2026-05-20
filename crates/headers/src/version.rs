pub(crate) const FULL: &str = env!("LLM_ROUTER_VERSION");

pub(crate) fn component_version(component: &str) -> String {
  format!("{component}/{FULL}")
}
