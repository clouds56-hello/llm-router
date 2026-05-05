use anyhow::{anyhow, Result};

pub mod pool;
pub mod proxy;
pub mod registry;
pub mod route;
pub mod server;

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
