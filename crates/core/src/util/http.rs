use anyhow::Result;
use bytes::Bytes;
use tokn_headers::HeaderMap;
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
    .pool_idle_timeout(Some(Duration::from_secs(90)))
    // Transparent response decompression. Personas advertise
    // `Accept-Encoding: gzip, deflate, br, zstd` (from real-world captures),
    // and providers honor it (zai → gzip). Without these toggles reqwest
    // hands the raw compressed bytes to downstream `convert_response` stages,
    // which then fail with `expected value at line 1 column 1`.
    // When reqwest decompresses, it also strips the response
    // `Content-Encoding` and `Content-Length` headers so the persisted
    // body and headers are mutually consistent.
    .gzip(true)
    .brotli(true)
    .deflate(true)
    .zstd(true);

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
  mut headers: HeaderMap,
  body: Option<Bytes>,
  capture: Option<&crate::provider::OutboundCapture>,
  what: &'static str,
) -> crate::provider::Result<reqwest::Response> {
  // Strip transport-derived headers before handing off to reqwest:
  //   - `Host`     : MUST be derived from `url` (SNI + HTTP Host must agree
  //                  or upstream WAFs reject; e.g. zai returns 403 when a
  //                  stale persona-default `Host: api.deepseek.com` survives
  //                  to a request actually sent to `api.z.ai`).
  //   - `Content-Length` : reqwest computes the correct value from `body`;
  //                  a stale persona-supplied value will not match the
  //                  serialized payload.
  // Persona builders may inject these from inbound captures or from defaults
  // derived from real-world traffic; that's fine for diagnostics but must
  // not reach the wire.
  let stripped_host = headers.remove(&tokn_headers::keys::HOST);
  let stripped_clen = headers.remove(&tokn_headers::keys::CONTENT_LENGTH);
  if stripped_host > 0 || stripped_clen > 0 {
    tracing::trace!(
      what,
      stripped_host,
      stripped_clen,
      "stripped transport headers before reqwest dispatch"
    );
  }
  if let (Some(capture), Some(body)) = (capture, body.as_ref()) {
    let _ = capture.set(crate::db::OutboundSnapshot {
      method: Some(method.as_str().to_string()),
      url: Some(url.to_string()),
      status: None,
      req_headers: headers.clone(),
      req_body: body.clone(),
      resp_headers: HeaderMap::new(),
      resp_body: Bytes::new(),
    });
  }
  let mut request = client.request(method, url).headers(headers.into());
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
