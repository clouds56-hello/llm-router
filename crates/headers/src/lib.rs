//! Foundational header primitives for the LLM router workspace.
//!
//! This crate provides:
//!
//! * [`HeaderName`] — a case- and order-preserving header name backed by [`SmolStr`].
//! * [`HeaderValue`] — a header value stored as `Cow<'static, str>` for zero-cost
//!   static defaults.
//! * [`HeaderMap`] — a `Vec`-backed map that preserves insertion order and original
//!   case while supporting case-insensitive lookup and duplicate names.
//! * [`keys`] — a catalogue of static [`HeaderName`] constants for popular headers.
//! * [`TemplateVars`] — per-request correlation metadata extracted from inbound
//!   headers, shared between header rendering and provider header construction.
//! * [`HeaderSchema`] — a trait implemented by typed (provider, client) header
//!   structs to round-trip between their typed form and a [`HeaderMap`].
//! * [`schemas`] — concrete client/overlay structs implementing [`HeaderSchema`].
//! * [`agent`] — agent-specific outbound header builders.
//! * [`registry`] — runtime lookup of (`AgentKind`, `OverlayKind`) for a given
//!   `(provider_id, agent_id)` pair.
//!
//! Phase 1 is purely additive: nothing in the workspace depends on this crate
//! yet. Phase 2 will swap [`HeaderMap`] in for `reqwest::header::HeaderMap`
//! workspace-wide; Phase 3 will route provider header construction through the
//! schema registry.

pub mod agent;
pub mod error;
pub mod keys;
pub mod map;
pub mod name;
pub mod registry;
pub mod reqwest_compat;
pub mod schema;
pub mod schemas;
pub mod value;
pub mod vars;

pub use error::Error;
pub use map::HeaderMap;
pub use name::HeaderName;
pub use schema::HeaderSchema;
pub use value::HeaderValue;
pub use vars::TemplateVars;
