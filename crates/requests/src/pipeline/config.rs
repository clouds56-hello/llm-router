//! Per-run config bag — caller-supplied key/value map threaded through every
//! stage via [`PipelineCtx`].
//!
//! The bag exists so secondary pipeline variants (e.g. the MITM proxy
//! passthrough) can pass transport-level hints to their custom stages
//! without bloating the [`RawInbound`] / [`Extracted`] / [`Resolved`]
//! structs with optional fields that only one variant ever reads.
//!
//! Keys are namespaced — use a dotted prefix (`"proxy.host"`,
//! `"proxy.path"`, etc.) so unrelated stages can coexist without clashes.
//! Values are stored as [`serde_json::Value`] so the bag is trivially
//! serialisable for diagnostics.
//!
//! Construct via [`RunConfig::builder`] or [`RunConfig::default`] for the
//! standard JSON pipeline that ignores the bag entirely.

use serde_json::Value;
use smol_str::SmolStr;
use std::collections::BTreeMap;

/// Caller-supplied per-run config bag. Cloned cheaply (the inner map is
/// owned, but [`PipelineCtx`] holds it behind an `Arc`).
#[derive(Clone, Default, Debug)]
pub struct RunConfig {
  inner: BTreeMap<SmolStr, Value>,
}

impl RunConfig {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn builder() -> RunConfigBuilder {
    RunConfigBuilder::default()
  }

  pub fn get(&self, key: &str) -> Option<&Value> {
    self.inner.get(key)
  }

  pub fn get_str(&self, key: &str) -> Option<&str> {
    self.inner.get(key).and_then(|v| v.as_str())
  }

  pub fn is_empty(&self) -> bool {
    self.inner.is_empty()
  }

  pub fn len(&self) -> usize {
    self.inner.len()
  }
}

#[derive(Default, Debug)]
pub struct RunConfigBuilder {
  inner: BTreeMap<SmolStr, Value>,
}

impl RunConfigBuilder {
  pub fn with(mut self, key: impl Into<SmolStr>, value: impl Into<Value>) -> Self {
    self.inner.insert(key.into(), value.into());
    self
  }

  pub fn with_str(mut self, key: impl Into<SmolStr>, value: impl Into<String>) -> Self {
    self.inner.insert(key.into(), Value::String(value.into()));
    self
  }

  pub fn with_str_opt(mut self, key: impl Into<SmolStr>, value: Option<impl Into<String>>) -> Self {
    if let Some(value) = value {
      self.inner.insert(key.into(), Value::String(value.into()));
    }
    self
  }

  pub fn build(self) -> RunConfig {
    RunConfig { inner: self.inner }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn builder_round_trip() {
    let cfg = RunConfig::builder()
      .with_str("proxy.host", "api.openai.com")
      .with_str("proxy.path", "/v1/chat/completions")
      .with("proxy.attempt", 0u64)
      .build();
    assert_eq!(cfg.get_str("proxy.host"), Some("api.openai.com"));
    assert_eq!(cfg.get_str("proxy.path"), Some("/v1/chat/completions"));
    assert_eq!(cfg.get("proxy.attempt").and_then(|v| v.as_u64()), Some(0));
    assert!(cfg.get("missing").is_none());
    assert_eq!(cfg.len(), 3);
  }

  #[test]
  fn default_is_empty() {
    let cfg = RunConfig::new();
    assert!(cfg.is_empty());
  }
}
