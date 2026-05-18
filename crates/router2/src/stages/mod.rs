//! Concrete and stub stage implementations.
//!
//! PR1 ships real implementations of [`DefaultExtract`](extract::DefaultExtract)
//! and [`PoolResolve`](resolve::PoolResolve). The other four stages are
//! provided only as `Noop*` placeholders so [`Profile`](crate::profile::Profile)
//! is always constructable.

pub mod build_headers;
pub mod convert_request;
pub mod convert_response;
pub mod extract;
pub mod resolve;
pub mod send;

pub use build_headers::{NoopBuildHeaders, PersonaBuildHeaders};
pub use convert_request::{DefaultConvertRequest, NoopConvertRequest};
pub use convert_response::NoopConvertResponse;
pub use extract::DefaultExtract;
pub use resolve::{AccountSelector, PoolAccountSelector, PoolResolve, SelectorOutcome};
pub use send::NoopSend;
