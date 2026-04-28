//! Session → account affinity map used by [`super::AccountPool`] to keep
//! multi-turn conversations (tool-call follow-ups, OpenAI Responses
//! `previous_response_id`, Anthropic extended-thinking continuations) on the
//! same upstream credential.
//!
//! Lookup semantics are tri-state via [`Lookup`]:
//! - `Hit(account_id)` — session known and still within TTL.
//! - `Expired` — session was known but eviction has elapsed; surfaces to the
//!   client as HTTP 410 so they replay rather than silently switching account.
//! - `Unknown` — first-use; the dispatcher allocates an account and records.
//!
//! Tombstones distinguish `Expired` from `Unknown`. They live for
//! `tombstone_ttl` (≥ `session_ttl`); after that the entry is fully forgotten
//! and a future request with the same id is treated as a brand new session.
//!
//! In-memory only — by design. Cross-restart affinity would need durable
//! state, which we explicitly chose not to keep here.
//!
//! Concurrency: a single `RwLock<HashMap<…>>` covers both lookup and write
//! paths. Write rate is one per request (record on success / on retry); read
//! rate is the same. No contention concern at expected throughput.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub enum Lookup {
  Hit(String),
  Expired,
  Unknown,
}

#[derive(Debug)]
struct Entry {
  /// Empty string ⇒ tombstone (the session was evicted).
  account_id: String,
  /// For live entries: when this binding was last touched.
  /// For tombstones: when the entry was tombstoned.
  stamped_at: Instant,
}

pub struct Affinity {
  map: RwLock<HashMap<String, Entry>>,
  ttl: Duration,
  tombstone_ttl: Duration,
}

impl Affinity {
  pub fn new(ttl: Duration, tombstone_ttl: Duration) -> Self {
    Self {
      map: RwLock::new(HashMap::new()),
      ttl,
      tombstone_ttl,
    }
  }

  /// Look up `key`. Side-effect: stale live entries are converted to
  /// tombstones; expired tombstones are removed.
  pub fn lookup(&self, key: &str) -> Lookup {
    // Fast path: read-only check.
    {
      let g = self.map.read();
      if let Some(e) = g.get(key) {
        let age = e.stamped_at.elapsed();
        if e.account_id.is_empty() {
          // Tombstone.
          if age < self.tombstone_ttl {
            return Lookup::Expired;
          }
        } else if age < self.ttl {
          return Lookup::Hit(e.account_id.clone());
        }
      } else {
        return Lookup::Unknown;
      }
    }
    // Slow path: state transition needed.
    let mut g = self.map.write();
    let now = Instant::now();
    match g.get(key) {
      Some(e) if e.account_id.is_empty() => {
        if now.duration_since(e.stamped_at) < self.tombstone_ttl {
          Lookup::Expired
        } else {
          g.remove(key);
          Lookup::Unknown
        }
      }
      Some(e) => {
        if now.duration_since(e.stamped_at) < self.ttl {
          Lookup::Hit(e.account_id.clone())
        } else {
          // Convert to tombstone.
          g.insert(
            key.to_string(),
            Entry {
              account_id: String::new(),
              stamped_at: now,
            },
          );
          Lookup::Expired
        }
      }
      None => Lookup::Unknown,
    }
  }

  /// Bind `key` to `account_id` (sliding-window refresh on repeat calls).
  /// Clears any tombstone for `key`.
  pub fn record(&self, key: &str, account_id: &str) {
    let mut g = self.map.write();
    g.insert(
      key.to_string(),
      Entry {
        account_id: account_id.to_string(),
        stamped_at: Instant::now(),
      },
    );
  }

}

#[cfg(test)]
mod tests {
  use super::*;
  use std::thread::sleep;

  #[test]
  fn unknown_then_hit_then_expired() {
    let a = Affinity::new(Duration::from_millis(60), Duration::from_millis(200));
    assert_eq!(a.lookup("k1"), Lookup::Unknown);
    a.record("k1", "acct-a");
    assert_eq!(a.lookup("k1"), Lookup::Hit("acct-a".into()));
    sleep(Duration::from_millis(80));
    assert_eq!(a.lookup("k1"), Lookup::Expired);
  }

  #[test]
  fn record_clears_tombstone() {
    let a = Affinity::new(Duration::from_millis(40), Duration::from_millis(400));
    a.record("k", "old");
    sleep(Duration::from_millis(60));
    assert_eq!(a.lookup("k"), Lookup::Expired);
    a.record("k", "new");
    assert_eq!(a.lookup("k"), Lookup::Hit("new".into()));
  }

  #[test]
  fn tombstone_eventually_forgotten() {
    let a = Affinity::new(Duration::from_millis(20), Duration::from_millis(60));
    a.record("k", "x");
    sleep(Duration::from_millis(30)); // > ttl
    assert_eq!(a.lookup("k"), Lookup::Expired); // tombstoned
    sleep(Duration::from_millis(80)); // > tombstone_ttl
    assert_eq!(a.lookup("k"), Lookup::Unknown);
  }

  #[test]
  fn sliding_window_keeps_session_alive() {
    let a = Affinity::new(Duration::from_millis(60), Duration::from_millis(300));
    a.record("k", "acct");
    for _ in 0..4 {
      sleep(Duration::from_millis(30));
      assert_eq!(a.lookup("k"), Lookup::Hit("acct".into()));
      a.record("k", "acct"); // refresh
    }
  }
}
