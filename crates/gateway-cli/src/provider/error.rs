//! Provider-layer errors. One enum covers every upstream concern (config
//! resolution, header construction, HTTP transport, status decoding, JSON
//! parsing, and OAuth flows) so the trait surface stays simple.
//!
//! Sub-modules return [`Result`] directly. Anyhow callers (server, cli) absorb
//! these via the blanket `impl From<E: StdError + Send + Sync> for
//! anyhow::Error` — so the migration is incremental: provider internals are
//! fully snafu, while the server/cli layers can finish migrating in later
//! steps without breaking the build.

use snafu::Snafu;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
  #[snafu(display("account '{account}' missing required credential: {what}"))]
  MissingCredential { account: String, what: &'static str },

  #[snafu(display("provider mismatch: {expected} expected, got '{got}'"))]
  ProviderMismatch { expected: &'static str, got: String },

  #[snafu(display("unknown provider '{id}' for account '{account}'"))]
  UnknownProvider { id: String, account: String },

  #[snafu(display("invalid HTTP header value for '{name}'"))]
  HeaderValue {
    name: String,
    source: reqwest::header::InvalidHeaderValue,
  },

  #[snafu(display("invalid HTTP header name '{name}'"))]
  HeaderName {
    name: String,
    source: reqwest::header::InvalidHeaderName,
  },

  #[snafu(display("{what}: HTTP request failed"))]
  Http { what: &'static str, source: reqwest::Error },

  #[snafu(display("{what}: upstream returned {status}: {body}"))]
  HttpStatus {
    what: &'static str,
    status: reqwest::StatusCode,
    body: String,
  },

  #[snafu(display("{what}: failed to parse JSON: {body}"))]
  Json {
    what: &'static str,
    body: String,
    source: serde_json::Error,
  },

  #[snafu(display("provider '{provider}' does not implement {endpoint}"))]
  UnsupportedEndpoint { provider: String, endpoint: &'static str },

  // --- OAuth / device flow ---------------------------------------------
  #[snafu(display("device code expired before authorization"))]
  DeviceCodeExpired,

  #[snafu(display("user denied authorization"))]
  AccessDenied,

  #[snafu(display("oauth error: {code}: {body}"))]
  OAuth { code: String, body: String },

  #[snafu(display("unexpected oauth token response: {body}"))]
  OAuthUnexpected { body: String },

  // --- profile loading -------------------------------------------------
  #[snafu(display("profiles: {message}"))]
  Profiles { message: String },
}
