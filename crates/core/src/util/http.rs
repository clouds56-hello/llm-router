use anyhow::{Context, Result};
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct HttpClientOptions {
  pub url: Option<String>,
  pub no_proxy: Vec<String>,
  pub system: bool,
}

pub fn build_client(proxy: &HttpClientOptions) -> Result<reqwest::Client> {
  let mut b = reqwest::Client::builder()
    .connect_timeout(Duration::from_secs(15))
    .timeout(Duration::from_secs(600))
    .pool_idle_timeout(Some(Duration::from_secs(90)));

  if let Some(url) = &proxy.url {
    let mut p = reqwest::Proxy::all(url).with_context(|| format!("invalid proxy url: {url}"))?;
    if !proxy.no_proxy.is_empty() {
      let joined = proxy.no_proxy.join(",");
      if let Some(np) = reqwest::NoProxy::from_string(&joined) {
        p = p.no_proxy(Some(np));
      }
    }
    b = b.proxy(p);
    tracing::info!(scheme = %scheme_of(url), "outbound proxy enabled");
  } else if proxy.system {
    // Defer to reqwest defaults (env vars).
    tracing::info!("outbound proxy: deferring to system env vars");
  } else {
    // Explicitly disable any ambient HTTP_PROXY/HTTPS_PROXY.
    b = b.no_proxy();
  }

  Ok(b.build()?)
}

fn scheme_of(url: &str) -> &str {
  url.split("://").next().unwrap_or("?")
}
