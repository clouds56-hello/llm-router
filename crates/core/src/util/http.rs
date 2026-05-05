use anyhow::Result;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use reqwest::Method;
use serde::de::DeserializeOwned;
use snafu::ResultExt;
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
    let mut p = reqwest::Proxy::all(url).map_err(|e| anyhow::anyhow!("invalid proxy url: {url}: {e}"))?;
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

pub async fn send(
  client: &reqwest::Client,
  method: Method,
  url: &str,
  headers: HeaderMap,
  body: Option<Bytes>,
  capture: Option<&crate::provider::OutboundCapture>,
  what: &'static str,
) -> crate::provider::Result<reqwest::Response> {
  if let (Some(capture), Some(body)) = (capture, body.as_ref()) {
    let _ = capture.set(crate::db::OutboundSnapshot {
      method: Some(method.as_str().to_string()),
      url: Some(url.to_string()),
      status: None,
      headers: headers.clone(),
      body: body.clone(),
    });
  }
  let mut request = client.request(method, url).headers(headers);
  if let Some(body) = body {
    request = request.body(body);
  }
  request.send().await.context(crate::provider::error::HttpSnafu { what })
}

pub async fn read_json<T>(resp: reqwest::Response, what: &'static str) -> crate::provider::Result<T>
where
  T: DeserializeOwned,
{
  let status = resp.status();
  let body = resp.text().await.unwrap_or_default();
  if !status.is_success() {
    return crate::provider::error::HttpStatusSnafu { what, status, body }.fail();
  }
  snafu::ResultExt::context(
    serde_json::from_str(&body),
    crate::provider::error::JsonSnafu {
      what,
      body: body.clone(),
    },
  )
}

fn scheme_of(url: &str) -> &str {
  url.split("://").next().unwrap_or("?")
}
