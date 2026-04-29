//! Logging-safe redactors / fingerprints.
//!
//! Logs and span fields must never expose raw credentials. Use these helpers
//! whenever a log site needs to identify *which* token / key was in play
//! without revealing the secret itself:
//!
//! - [`token_fingerprint`] — first 8 hex chars of SHA-256, prefix-tagged.
//! - [`BehaveAs`] — `Display` adapter for `Option<&str>` persona fields that
//!   prints the bare name or `none` instead of `Some("…")`/`None`.
//!
//! A header-preview helper was deliberately omitted: per project policy we
//! never log inbound or upstream headers, request bodies, or response bodies
//! — even truncated, even at trace.

use sha2::{Digest, Sha256};
use std::fmt;

/// Return `sha256[..8]` of `secret` as a lowercase hex string with a
/// stable prefix so log greppers can spot fingerprints at a glance.
///
/// Two refreshes of the same secret yield identical fingerprints, which is
/// useful for verifying that token caching / rotation actually rotated.
pub fn token_fingerprint(secret: &str) -> String {
  if secret.is_empty() {
    return "fp:<empty>".into();
  }
  let mut h = Sha256::new();
  h.update(secret.as_bytes());
  let digest = h.finalize();
  let mut s = String::with_capacity(3 + 16);
  s.push_str("fp:");
  for b in digest.iter().take(8) {
    use std::fmt::Write as _;
    let _ = write!(s, "{b:02x}");
  }
  s
}

/// `Display` adapter for an optional persona name. Renders the name verbatim
/// or the literal `none` so log records stay grep-friendly.
pub struct BehaveAs<'a>(pub Option<&'a str>);

impl fmt::Display for BehaveAs<'_> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self.0 {
      Some(s) if !s.is_empty() => f.write_str(s),
      _ => f.write_str("none"),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn fingerprint_is_stable_and_short() {
    let a = token_fingerprint("hunter2");
    let b = token_fingerprint("hunter2");
    let c = token_fingerprint("hunter3");
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert!(a.starts_with("fp:"));
    assert_eq!(a.len(), 3 + 16);
  }

  #[test]
  fn fingerprint_tags_empty() {
    assert_eq!(token_fingerprint(""), "fp:<empty>");
  }

  #[test]
  fn behave_as_display() {
    assert_eq!(format!("{}", BehaveAs(None)), "none");
    assert_eq!(format!("{}", BehaveAs(Some(""))), "none");
    assert_eq!(format!("{}", BehaveAs(Some("opencode"))), "opencode");
  }
}
