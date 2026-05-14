use async_trait::async_trait;
use llm_auth::{AuthError, DeviceCodeHandle, ProviderAuth, QuotaSnapshot, RefreshOutcome, Result, VerifyOutcome};
use llm_core::account::AccountConfig;

pub struct OpenAiAuth {
  id: &'static str,
}

impl OpenAiAuth {
  pub const fn new(id: &'static str) -> Self {
    Self { id }
  }
}

static OPENAI: OpenAiAuth = OpenAiAuth::new(crate::ID_OPENAI);
static CODEX: OpenAiAuth = OpenAiAuth::new(crate::ID_CODEX);

pub fn openai_auth() -> &'static dyn ProviderAuth {
  &OPENAI
}

pub fn codex_auth() -> &'static dyn ProviderAuth {
  &CODEX
}

#[async_trait]
impl ProviderAuth for OpenAiAuth {
  fn id(&self) -> &'static str {
    self.id
  }

  fn supports_device_flow(&self) -> bool {
    self.id == crate::ID_CODEX
  }

  fn supports_static_key(&self) -> bool {
    self.id == crate::ID_OPENAI
  }

  fn default_account_id(&self) -> &'static str {
    self.id
  }

  fn default_base_url(&self) -> Option<&'static str> {
    match self.id {
      crate::ID_OPENAI => Some(crate::openai::OPENAI_BASE_URL),
      crate::ID_CODEX => Some(crate::openai::CODEX_BASE_URL),
      _ => None,
    }
  }

  async fn request_device_code(&self, _client: &reqwest::Client) -> Result<DeviceCodeHandle> {
    Err(AuthError::Unsupported(
      "codex OAuth login is not implemented yet: ChatGPT OAuth client/endpoints are not configured".into(),
    ))
  }

  async fn refresh_credential(&self, _client: &reqwest::Client, _account: &AccountConfig) -> Result<RefreshOutcome> {
    if self.id == crate::ID_CODEX {
      return Err(AuthError::Unsupported(
        "codex OAuth refresh is not implemented yet: ChatGPT OAuth token endpoint is not configured".into(),
      ));
    }
    Ok(RefreshOutcome::NotApplicable)
  }

  async fn verify_credential(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<VerifyOutcome> {
    match self.id {
      crate::ID_OPENAI => {
        verify_bearer(
          client,
          account,
          account.api_key.as_ref().map(|s| s.expose().as_str()),
          "api_key",
        )
        .await
      }
      crate::ID_CODEX => {
        let token = account
          .access_token
          .as_ref()
          .or(account.api_key.as_ref())
          .map(|s| s.expose().as_str());
        verify_bearer(client, account, token, "access_token").await
      }
      _ => Err(AuthError::Unsupported(self.id.to_string())),
    }
  }

  async fn probe_quota(&self, _client: &reqwest::Client, _account: &AccountConfig) -> Result<QuotaSnapshot> {
    Ok(QuotaSnapshot::default())
  }
}

async fn verify_bearer(
  client: &reqwest::Client,
  account: &AccountConfig,
  token: Option<&str>,
  field: &'static str,
) -> Result<VerifyOutcome> {
  let token = token.ok_or(AuthError::MissingCredential {
    account: account.id.clone(),
    field,
  })?;
  let base = account
    .base_url
    .clone()
    .unwrap_or_else(|| match account.provider.as_str() {
      crate::ID_CODEX => crate::openai::CODEX_BASE_URL.to_string(),
      _ => crate::openai::OPENAI_BASE_URL.to_string(),
    });
  let url = if account.provider == crate::ID_CODEX {
    format!("{}/responses", base.trim_end_matches('/'))
  } else {
    format!("{}/models", base.trim_end_matches('/'))
  };
  let resp = client
    .get(url)
    .header("authorization", format!("Bearer {token}"))
    .header("accept", "application/json")
    .send()
    .await
    .map_err(|e| AuthError::Network(e.to_string()))?;
  if resp.status().is_success() || resp.status().as_u16() == 405 {
    Ok(VerifyOutcome::default())
  } else {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    Err(AuthError::Upstream(format!(
      "{} rejected the credential (HTTP {status}): {}",
      account.provider,
      body.chars().take(200).collect::<String>()
    )))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[tokio::test]
  async fn codex_login_is_clear_placeholder() {
    let err = codex_auth()
      .request_device_code(&reqwest::Client::new())
      .await
      .unwrap_err();
    assert!(err.to_string().contains("codex OAuth login is not implemented"));
  }
}
