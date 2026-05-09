use anyhow::{anyhow, Result};

pub mod accounts;
pub mod api;
pub mod pipeline;
pub mod proxy;
pub mod relay;
pub mod routing;

pub use llm_config as config;
pub use llm_config::profiles;
pub use llm_convert as convert;
pub use llm_core::{db, provider, util};

pub fn install_rustls_crypto_provider() -> Result<()> {
  rustls::crypto::ring::default_provider()
    .install_default()
    .map_err(|_| anyhow!("failed to install rustls ring crypto provider"))?;
  Ok(())
}
