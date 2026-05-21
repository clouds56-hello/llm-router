use crate::util::secret::Secret;
use crate::{HeaderPatchCtx, Result};
use tokn_headers::keys::{ACCEPT, AUTHORIZATION, CONTENT_ENCODING, CONTENT_TYPE};
use tokn_headers::{HeaderMap, HeaderValue};

pub enum Credential {
  ApiKey(Secret<String>),
  AccessToken(Secret<String>),
}

impl Credential {
  pub fn expose(&self) -> &str {
    match self {
      Credential::ApiKey(secret) | Credential::AccessToken(secret) => secret.expose(),
    }
  }
}

pub fn url(base_url: &str, path: &str) -> String {
  format!("{}{}", base_url.trim_end_matches('/'), path)
}

pub fn patch_openai_headers(headers: &mut HeaderMap, token: &str, ctx: &HeaderPatchCtx<'_>) -> Result<()> {
  headers.insert(&AUTHORIZATION, HeaderValue::from_string(format!("Bearer {token}")));
  headers.insert(
    &ACCEPT,
    HeaderValue::from_static(if ctx.stream {
      "text/event-stream"
    } else {
      "application/json"
    }),
  );
  headers.insert(&CONTENT_TYPE, HeaderValue::from_static("application/json"));
  if let Some(encoding) = ctx.content_encoding {
    headers.insert(&CONTENT_ENCODING, HeaderValue::from_string(encoding.to_string()));
  }
  Ok(())
}
