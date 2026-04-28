use crate::config::CopilotHeaders;
use crate::provider::{error, Result};
use serde_json::Value;
use snafu::ResultExt;

pub async fn list(client: &reqwest::Client, api_token: &str, headers: &CopilotHeaders) -> Result<Value> {
  let h = super::headers::copilot_request_headers(api_token, headers, false, "user")?;
  let resp = client
    .get(format!("{}/models", super::COPILOT_API))
    .headers(h)
    .send()
    .await
    .context(error::HttpSnafu { what: "list models" })?;
  let status = resp.status();
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
  Ok(v)
}
