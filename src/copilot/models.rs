use crate::config::CopilotHeaders;
use anyhow::{anyhow, Context, Result};
use serde_json::Value;

pub async fn list(
    client: &reqwest::Client,
    api_token: &str,
    headers: &CopilotHeaders,
) -> Result<Value> {
    let h = super::headers::copilot_request_headers(api_token, headers, false)?;
    let resp = client
        .get(format!("{}/models", super::COPILOT_API))
        .headers(h)
        .send()
        .await
        .context("models request failed")?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("models returned {status}: {body}"));
    }
    let v: Value = serde_json::from_str(&body).context("parse /models JSON")?;
    Ok(v)
}
