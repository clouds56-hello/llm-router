use anyhow::Result;
use std::time::Duration;

pub fn build_client() -> Result<reqwest::Client> {
    let c = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(600))
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .build()?;
    Ok(c)
}
