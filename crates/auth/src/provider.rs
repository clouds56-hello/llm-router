//! Provider-agnostic authentication contracts.
//!
//! `llm-auth` orchestrates account lifecycle (login, import, refresh,
//! status) but holds zero provider-specific HTTP code. Each provider crate
//! implements [`ProviderAuth`] and exposes a `provider_auth()` accessor;
//! `llm-auth` looks up the impl by `AccountConfig::provider` and dispatches.
//!
//! Keeping the trait here (rather than in `llm-auth`) avoids a circular
//! dep: provider crates already depend on `llm-core`, and `llm-auth` will
//! depend on both.

use llm_core::account::AccountConfig;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Outcome of a successful device-flow login (currently only used by
/// github-copilot). The caller is responsible for assembling these fields
/// into an [`AccountConfig`].
#[derive(Debug, Clone)]
pub struct DeviceFlowOutcome {
  /// Long-lived OAuth refresh token obtained from the upstream OAuth dance.
  pub refresh_token: String,
  /// Short-lived access token already exchanged from the refresh token.
  pub access_token: String,
  /// Unix timestamp at which `access_token` expires.
  pub access_token_expires_at: i64,
  /// Optional upstream username (used to suggest an account id).
  pub username: Option<String>,
}

/// Opaque handle returned by [`ProviderAuth::request_device_code`] and
/// consumed by [`ProviderAuth::poll_device_code`]. The CLI uses
/// `verification_uri` + `user_code` to instruct the user, then hands the
/// whole struct back to the provider for polling.
///
/// `device_code` is the provider-internal identifier (opaque to the CLI);
/// it is sent back to the upstream authorization server during polling.
#[derive(Debug, Clone)]
pub struct DeviceCodeHandle {
  /// Provider-internal device code string. Opaque to the caller — but
  /// the CLI never needs to read it; the field is only public so that
  /// providers can construct the handle in their own crates.
  pub device_code: String,
  /// Short user-facing code to type at `verification_uri`.
  pub user_code: String,
  /// URL the user should visit in a browser.
  pub verification_uri: String,
  /// Seconds until the device code expires (display only).
  pub expires_in: u64,
  /// Minimum interval (seconds) between poll attempts.
  pub interval: u64,
}

/// Credential acquisition strategies offered by `account add` and
/// `account login`. Providers advertise which sources they accept via
/// [`ProviderAuth::supports_credential_source`].
///
/// Variants intentionally carry their inputs (refresh token, env var
/// name) so the trait surface stays narrow: the CLI gathers the user
/// input, packs it here, and the provider does the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialSource {
  /// Run the provider's interactive flow (device-flow OAuth or static-key
  /// prompt).
  Login,
  /// Shell out to `gh auth token` (Copilot-only).
  Gh,
  /// Scrape `~/.config/github-copilot/{apps,hosts}.json` (Copilot-only).
  CopilotPlugin,
  /// Provide a long-lived refresh token directly (Copilot-only).
  RefreshToken { token: String },
  /// Read a static API key from an environment variable (static-key
  /// providers only).
  Env { env_var: String },
}

/// Outcome of a refresh-credential call. For OAuth providers this is a
/// fresh access token; for static-key providers it is a no-op (and
/// [`ProviderAuth::refresh_credential`] returns
/// [`RefreshOutcome::NotApplicable`]).
#[derive(Debug, Clone)]
pub enum RefreshOutcome {
  /// A new short-lived access token was issued.
  Refreshed {
    access_token: String,
    expires_at: i64,
  },
  /// The provider uses a static credential; nothing to refresh.
  NotApplicable,
}

/// Provider-agnostic snapshot of remote quota / plan state, returned by
/// [`ProviderAuth::probe_quota`]. Renderers (CLI status) interpret the
/// `provider_extra` blob for provider-specific detail.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuotaSnapshot {
  /// Human-readable plan name (e.g. `"copilot_pro"`, `"GLM Coding Plan"`).
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub plan: Option<String>,
  /// Short one-line headline for compact rendering (e.g. `"premium_interactions: 12/300"`).
  /// Used by `account status`; `account list` prefers `metered` for richer formatting.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub headline: Option<String>,
  /// ISO-8601 reset date if the upstream advertises one.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reset_date: Option<String>,
  /// The primary metered request bucket — typically the visible
  /// "premium" / "headline" allowance the user cares about. Renderers
  /// display this with a percent gauge.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub metered: Option<MeteredBucket>,
  /// Additional usage buckets (e.g. Z.ai 5h tokens, weekly tokens, MCP
  /// monthly). Rendered as one row each by the CLI list command.
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub secondary: Vec<UsageBucket>,
  /// Provider-specific blob for extras the generic shape can't capture.
  #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
  pub provider_extra: serde_json::Value,
}

/// A metered request bucket — counts down as the user spends premium
/// requests. `entitlement = None` means the bucket is unlimited (some
/// Copilot plans expose `chat` as unmetered).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeteredBucket {
  /// Display label, e.g. `"premium_interactions"`.
  pub label: String,
  /// Remaining count in the bucket.
  pub remaining: u64,
  /// Total entitlement; `None` = unlimited.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub entitlement: Option<u64>,
}

/// A used/total token (or request) bucket — counts up as usage accrues.
/// Z.ai exposes several of these (5-hour window, weekly, MCP monthly).
///
/// Either `used`/`total` or `percent_used` (or both) may be populated;
/// renderers should fall back gracefully. `total = 0` is treated as
/// "unknown total" for renderers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageBucket {
  /// Display label, e.g. `"5h tokens"`.
  pub label: String,
  /// Amount already used, when the upstream reports a discrete count.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub used: Option<u64>,
  /// Total cap for this window, when known.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub total: Option<u64>,
  /// Percent used (0.0–100.0), when the upstream only exposes a ratio.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub percent_used: Option<f64>,
  /// Optional epoch-ms timestamp at which the bucket resets.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub reset_at_ms: Option<i64>,
}

/// Errors surfaced by the auth layer. Kept lightweight (string payload)
/// because this trait crosses many crate boundaries; consumers can wrap
/// with `anyhow::Context` as needed.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
  #[error("provider '{0}' does not support this operation")]
  Unsupported(String),
  #[error("missing credential field '{field}' on account '{account}'")]
  MissingCredential { account: String, field: &'static str },
  #[error("upstream HTTP error: {0}")]
  Upstream(String),
  #[error("network error: {0}")]
  Network(String),
  #[error("malformed response: {0}")]
  Decode(String),
  #[error("{0}")]
  Other(String),
}

pub type Result<T> = std::result::Result<T, AuthError>;

/// All authentication-flow capabilities a provider can implement.
///
/// Static-key providers (e.g. Z.ai) leave [`Self::supports_device_flow`]
/// as `false` and return [`RefreshOutcome::NotApplicable`] from
/// [`Self::refresh_credential`]. OAuth providers (e.g. github-copilot)
/// implement everything.
///
/// Implementations must be cheap to construct (typically zero-sized) and
/// hold no state — all per-call inputs are passed as arguments. Each
/// provider crate exposes a `provider_auth() -> &'static dyn ProviderAuth`
/// accessor; `llm-auth` builds a static dispatch table at startup.
#[async_trait]
pub trait ProviderAuth: Send + Sync {
  /// Provider id this impl handles (e.g. `"github-copilot"`). Must match
  /// [`AccountConfig::provider`] exactly.
  fn id(&self) -> &'static str;

  /// True if [`Self::request_device_code`] / [`Self::poll_device_code`]
  /// (and the convenience [`Self::device_flow_login`] wrapper) are
  /// implemented.
  fn supports_device_flow(&self) -> bool {
    false
  }

  /// True if this provider authenticates with a static API key (no OAuth
  /// dance). Used to gate `--from env` / interactive key-paste prompts.
  fn supports_static_key(&self) -> bool {
    false
  }

  /// True if the CLI's [`CredentialSource`] is supported by this
  /// provider. Default impl uses [`Self::supports_device_flow`] and
  /// [`Self::supports_static_key`] to derive a sensible answer; providers
  /// can override for finer control.
  fn supports_credential_source(&self, src: &CredentialSource) -> bool {
    match src {
      CredentialSource::Login => self.supports_device_flow() || self.supports_static_key(),
      CredentialSource::Gh | CredentialSource::CopilotPlugin | CredentialSource::RefreshToken { .. } => {
        // These are all "I already have a long-lived OAuth token" paths;
        // only OAuth providers accept them.
        self.supports_device_flow()
      }
      CredentialSource::Env { .. } => self.supports_static_key(),
    }
  }

  /// Suggested account id when the caller hasn't picked one and the
  /// flow can't infer one (e.g. failed username lookup, env-var import).
  /// Defaults to the provider id, which is fine for static-key providers.
  fn default_account_id(&self) -> &'static str {
    self.id()
  }

  /// Default upstream base URL to seed `AccountConfig::base_url` when
  /// onboarding a new account. `None` means "no override; let the
  /// provider's runtime choose".
  fn default_base_url(&self) -> Option<&'static str> {
    None
  }

  /// Default OAuth token-exchange URL to seed
  /// `AccountConfig::refresh_url`. Only meaningful for OAuth providers.
  fn default_refresh_url(&self) -> Option<&'static str> {
    None
  }

  /// Step 1 of device-flow OAuth: request a fresh device code from the
  /// upstream authorization server. Returns a handle to be passed back
  /// to [`Self::poll_device_code`].
  ///
  /// Default impl returns `Unsupported`; OAuth providers override.
  async fn request_device_code(&self, _client: &reqwest::Client) -> Result<DeviceCodeHandle> {
    Err(AuthError::Unsupported(self.id().to_string()))
  }

  /// Step 2 of device-flow OAuth: poll the upstream until the user has
  /// approved the device code in their browser, then exchange the
  /// resulting long-lived token for a short-lived access token and
  /// (best-effort) look up a username for id suggestion.
  ///
  /// Default impl returns `Unsupported`; OAuth providers override.
  async fn poll_device_code(
    &self,
    _client: &reqwest::Client,
    _handle: DeviceCodeHandle,
  ) -> Result<DeviceFlowOutcome> {
    Err(AuthError::Unsupported(self.id().to_string()))
  }

  /// Convenience wrapper that calls [`Self::request_device_code`] then
  /// [`Self::poll_device_code`] back-to-back. Callers that want to
  /// display the user code between request and poll (e.g. an interactive
  /// CLI) should call the two methods directly.
  async fn device_flow_login(&self, client: &reqwest::Client) -> Result<DeviceFlowOutcome> {
    let handle = self.request_device_code(client).await?;
    self.poll_device_code(client, handle).await
  }

  /// Refresh the account's short-lived credential (e.g. exchange a refresh
  /// token for a new access token). Static-key providers return
  /// [`RefreshOutcome::NotApplicable`].
  async fn refresh_credential(
    &self,
    client: &reqwest::Client,
    account: &AccountConfig,
  ) -> Result<RefreshOutcome>;

  /// Verify the account's stored credential is currently usable, without
  /// mutating it. Used by `account status` and the CLI smoke test.
  ///
  /// For OAuth providers this typically runs a token exchange to confirm
  /// the refresh token is still good; for static-key providers it hits a
  /// cheap upstream endpoint (e.g. `GET /models`).
  async fn verify_credential(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<()>;

  /// Fetch a [`QuotaSnapshot`] for status display. May be a no-op
  /// (returning `Default::default()`) when the upstream offers no quota
  /// API.
  async fn probe_quota(&self, client: &reqwest::Client, account: &AccountConfig) -> Result<QuotaSnapshot>;

  /// Default outer-timeout to apply when running [`Self::probe_quota`]
  /// from the status command. Providers can shorten this for slow
  /// endpoints.
  fn quota_timeout(&self) -> Duration {
    Duration::from_secs(5)
  }
}
