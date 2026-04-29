use crate::config::CopilotHeaders;
use crate::provider::{error, Result};
use crate::util::redact::token_fingerprint;
use serde_json::Value;
use snafu::ResultExt;
use tracing::{debug, instrument};

#[instrument(
  name = "copilot_list_models",
  skip_all,
  fields(
    api_token_fp = %token_fingerprint(api_token),
    status = tracing::field::Empty,
    count = tracing::field::Empty,
  ),
)]
pub async fn list(client: &reqwest::Client, api_token: &str, headers: &CopilotHeaders) -> Result<Value> {
  let h = super::headers::copilot_request_headers(api_token, headers, false, "user")?;
  debug!("fetching copilot model list");
  let resp = client
    .get(format!("{}/models", super::COPILOT_API))
    .headers(h)
    .send()
    .await
    .context(error::HttpSnafu { what: "list models" })?;
  let status = resp.status();
  tracing::Span::current().record("status", status.as_u16());
  let body = resp.text().await.unwrap_or_default();
  if !status.is_success() {
    return error::HttpStatusSnafu {
      what: "list models",
      status,
      body,
    }
    .fail();
  }
  let v: Value = serde_json::from_str(&body).context(error::JsonSnafu {
    what: "list models",
    body: body.clone(),
  })?;
  let count = v.get("data").and_then(|d| d.as_array()).map(|a| a.len()).unwrap_or(0);
  tracing::Span::current().record("count", count);
  Ok(v)
}
