//! `llm-auth` — provider-agnostic account auth orchestration.
//!
//! This crate owns:
//! * The [`ProviderAuth`] trait and its companion lifecycle types
//!   ([`DeviceFlowOutcome`], [`RefreshOutcome`], [`QuotaSnapshot`],
//!   [`AuthError`]). Provider crates implement this trait; everything
//!   above the provider layer programs against it.
//! * The [`AuthStore`] backing `auth.yaml` — the post-refactor home for
//!   account records, replacing the legacy `[[accounts]]` block in
//!   `config.toml`. During the transition the loader reads both, prefers
//!   `auth.yaml`, and emits a deprecation warning when it falls back.
//!
//! Note: the provider-id → [`ProviderAuth`] *registry* deliberately does
//! **not** live here. `llm-auth` is the bottom of the auth stack and must
//! not depend on any provider crate (cycle-free). The dispatch table
//! lives in the consumer that already pulls in every provider — currently
//! `gateway-cli::auth_registry`.

pub mod provider;
pub mod store;

pub use llm_core::account::{AccountConfig, AccountState, AccountTier};
pub use provider::{
  default_import_from, AuthError, CredentialFlavor, CredentialResult, CredentialSource,
  CredentialSourceKind, DeviceCodeHandle, DeviceFlowOutcome, MeteredBucket, ProviderAuth,
  QuotaSnapshot, RefreshOutcome, Result, UsageBucket, VerifyOutcome,
};
pub use store::AuthStore;
