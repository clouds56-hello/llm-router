pub mod http {
  use crate::config::ProxyConfig;
  use anyhow::Result;

  pub fn build_client(proxy: &ProxyConfig) -> Result<reqwest::Client> {
    tokn_core::util::http::build_client(&proxy.to_http_options())
  }
}

pub use tokn_core::util::{secret, timefmt};
