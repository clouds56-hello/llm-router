//! Crate-wide error type.
//!
//! Each subsystem owns its own `snafu::Snafu` enum (see `pool::Error`,
//! `usage::Error`, `catalogue::Error`, `config::Error`, `provider::Error`,
//! `server::Error`, `cli::Error`). Those compose into the top-level
//! [`Error`] via `#[snafu(source)]`.
//!
//! Two high-level rules govern this hierarchy:
//!
//! 1. **`Display` is for humans, `source` is for chains.** Each variant's
//!    message describes *what we were doing*, not *what went wrong* — the
//!    underlying cause is reachable via [`std::error::Error::source`] and
//!    rendered separately by the CLI reporter / log layer.
//! 2. **Public surfaces never leak internals.** The HTTP layer's
//!    [`crate::server::Error::IntoResponse`] mapping decides per-variant
//!    which fields cross the wire; the source chain stays log-only. (The
//!    historical `From<anyhow::Error> for ApiError` flattener has been
//!    removed.)
//!
//! Known limitation: upstream HTTP response bodies are still interpolated
//! verbatim into a few provider-level error messages
//! (`provider::github_copilot::oauth.rs`, `token.rs`, `user.rs`,
//! `provider::zai::quota`, etc.). Those strings can contain arbitrary
//! upstream content; do not echo them to users without scrubbing. A
//! follow-up should hash or truncate these.

use snafu::Snafu;

/// Crate-wide convenience alias.
#[allow(dead_code)]
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
#[allow(dead_code)]
pub enum Error {
    #[snafu(display("pool"), context(false))]
    Pool { source: crate::pool::Error },

    #[snafu(display("usage"), context(false))]
    Usage { source: crate::usage::Error },

    #[snafu(display("catalogue"), context(false))]
    Catalogue { source: crate::catalogue::loader::Error },

    #[snafu(display("config"), context(false))]
    Config { source: crate::config::Error },

    #[snafu(display("provider"), context(false))]
    Provider { source: crate::provider::Error },

    #[snafu(display("cli"), context(false))]
    Cli { source: crate::cli::Error },

    // Subsystem variants are added as each module migrates off anyhow.
    // During the transition we keep an `Other` adapter so partially-migrated
    // call paths still compile.
    #[snafu(display("{message}"))]
    Other { message: String },
}

impl From<anyhow::Error> for Error {
    fn from(e: anyhow::Error) -> Self {
        Error::Other { message: format!("{e:#}") }
    }
}
