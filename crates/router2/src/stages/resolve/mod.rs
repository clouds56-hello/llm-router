//! Resolve stage — picks the upstream account and upstream model for an
//! extracted request.
//!
//! * [`stage`] — the generic [`PoolResolve`] stage and the
//!   [`AccountSelector`] trait it consults.
//! * [`pool`] — [`PoolAccountSelector`], the production [`AccountSelector`]
//!   implementation backed by [`llm_accounts::AccountPool`] +
//!   [`llm_accounts::RouteResolver`].

mod pool;
mod stage;

pub use pool::PoolAccountSelector;
pub use stage::{AccountSelector, PoolResolve, SelectorOutcome};
