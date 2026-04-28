//! `Secret<T>` — a transparent newtype that refuses to print its payload.
//!
//! Wrap any field that holds credentials, bearer tokens, or other material
//! that must not appear in logs, error messages, or `config show` dumps.
//!
//! `Debug` and `Display` always render `***`; the only way to read the
//! contents is the explicit [`Secret::expose`] accessor. `Serialize` also
//! renders `***`, so a round-trip through TOML/JSON cannot accidentally
//! exfiltrate the value.
//!
//! ```
//! use llm_router::util::secret::Secret;
//! let s: Secret<String> = "hunter2".to_owned().into();
//! assert_eq!(format!("{s:?}"), "Secret(***)");
//! assert_eq!(s.expose(), "hunter2");
//! ```
//!
//! Equality, ordering, hashing intentionally aren't derived: comparing
//! secrets in the clear is itself a footgun (timing channels). Use
//! [`Secret::expose`] and a constant-time comparator if you really need it.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

#[derive(Clone, Default)]
#[allow(dead_code)]
pub struct Secret<T>(T);

#[allow(dead_code)]
impl<T> Secret<T> {
    #[inline]
    pub const fn new(inner: T) -> Self {
        Self(inner)
    }

    #[inline]
    pub fn expose(&self) -> &T {
        &self.0
    }

    #[inline]
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> From<T> for Secret<T> {
    #[inline]
    fn from(v: T) -> Self {
        Self(v)
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl<T> Serialize for Secret<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str("***")
    }
}

impl<'de, T> Deserialize<'de> for Secret<T>
where
    T: Deserialize<'de>,
{
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        T::deserialize(d).map(Secret)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_and_display_redact() {
        let s: Secret<String> = "tok_abc".to_string().into();
        assert_eq!(format!("{s:?}"), "Secret(***)");
        assert_eq!(format!("{s}"), "***");
    }

    #[test]
    fn expose_returns_payload() {
        let s = Secret::new(42_u32);
        assert_eq!(*s.expose(), 42);
    }

    #[test]
    fn serialize_redacts() {
        let s: Secret<String> = "tok_abc".to_string().into();
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, "\"***\"");
    }

    #[test]
    fn deserialize_passes_through() {
        let s: Secret<String> = serde_json::from_str("\"hello\"").unwrap();
        assert_eq!(s.expose(), "hello");
    }
}
