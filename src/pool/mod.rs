//! Generic account pool. Each account holds an `Arc<dyn Provider>`; the pool
//! itself only manages health/cooldown state and round-robin selection.

use crate::config::Config;
use crate::provider::{self, Endpoint, Provider};
use parking_lot::RwLock;
use snafu::{ResultExt, Snafu};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Errors that can occur while constructing or querying an [`AccountPool`].
///
/// Runtime acquisition (`AccountPool::acquire`) signals "no supporting
/// account" via `Option::None` rather than an error variant — that case is
/// load-bearing for the dispatcher's 501 mapping and not a failure.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
  #[snafu(display(
    "no accounts configured. Run `llm-router login` or `llm-router import` first."
  ))]
  NoAccounts,

  #[snafu(display("failed to build provider for account `{id}`"))]
  BuildAccount {
    id: String,
    source: crate::provider::Error,
  },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Account {
  pub id: String,
  pub provider: Arc<dyn Provider>,
  inner: RwLock<AccountInner>,
}

struct AccountInner {
  cooldown_until: Option<Instant>,
  consecutive_failures: u32,
}

impl Account {
  fn is_healthy(&self) -> bool {
    match self.inner.read().cooldown_until {
      None => true,
      Some(t) => Instant::now() >= t,
    }
  }

  pub fn mark_failure(&self, cooldown_base: Duration) {
    let mut g = self.inner.write();
    g.consecutive_failures = g.consecutive_failures.saturating_add(1);
    let mult = 1u32 << (g.consecutive_failures.min(5) - 1);
    let cd = cooldown_base.saturating_mul(mult);
    g.cooldown_until = Some(Instant::now() + cd);
    tracing::warn!(account = %self.id, retry_in_secs = cd.as_secs(), "account in cooldown");
  }

  pub fn mark_success(&self) {
    let mut g = self.inner.write();
    g.consecutive_failures = 0;
    g.cooldown_until = None;
  }

  /// Notify the underlying provider that an upstream 401 happened so it can
  /// drop any cached short-lived credential.
  pub fn invalidate_credentials(&self) {
    self.provider.on_unauthorized();
  }
}

pub struct AccountPool {
  accounts: Vec<Arc<Account>>,
  cursor: AtomicUsize,
  cooldown_base: Duration,
}

impl AccountPool {
  pub fn from_config(cfg: &Config) -> Result<Arc<Self>> {
    if cfg.accounts.is_empty() {
      return NoAccountsSnafu.fail();
    }
    let mut accounts = Vec::with_capacity(cfg.accounts.len());
    for a in &cfg.accounts {
      let p = provider::build_for_account(a, &cfg.copilot)
        .context(BuildAccountSnafu { id: a.id.clone() })?;
      accounts.push(Arc::new(Account {
        id: a.id.clone(),
        provider: p,
        inner: RwLock::new(AccountInner {
          cooldown_until: None,
          consecutive_failures: 0,
        }),
      }));
    }
    Ok(Arc::new(Self {
      accounts,
      cursor: AtomicUsize::new(0),
      cooldown_base: Duration::from_secs(cfg.pool.failure_cooldown_secs),
    }))
  }

  pub fn len(&self) -> usize {
    self.accounts.len()
  }

  pub fn cooldown_base(&self) -> Duration {
    self.cooldown_base
  }

  /// Pick the next account that supports both `model` and `endpoint`
  /// (round-robin).
  ///
  /// Returns `None` only when **no** account in the pool supports the
  /// `(model, endpoint)` tuple — in which case the handler should map to
  /// `501 Not Implemented` rather than retrying. Cooldown is best-effort:
  /// if every supporting account is in cooldown we still hand one back so
  /// the caller can attempt the request.
  pub fn acquire(&self, model: Option<&str>, endpoint: Endpoint) -> Option<Arc<Account>> {
    let n = self.accounts.len();
    if n == 0 {
      return None;
    }
    let start = self.cursor.fetch_add(1, Ordering::Relaxed);

    // Helper: does this account claim support for (model, endpoint)?
    let supports = |a: &Account| -> bool {
      let m = model.unwrap_or("");
      a.provider.supports(m, endpoint)
    };

    // First pass: healthy AND supports endpoint (and model, when given).
    for i in 0..n {
      let idx = (start + i) % n;
      let a = &self.accounts[idx];
      if a.is_healthy() && supports(a) {
        return Some(a.clone());
      }
    }
    // Second pass: any account that supports the endpoint, even if in
    // cooldown — pick the one with the earliest cooldown_until.
    let mut best: Option<&Arc<Account>> = None;
    let mut best_t: Option<Instant> = None;
    for a in &self.accounts {
      if !supports(a) {
        continue;
      }
      let t = a.inner.read().cooldown_until;
      if best.is_none() || t < best_t {
        best = Some(a);
        best_t = t;
      }
    }
    best.cloned()
  }

  pub fn all(&self) -> &[Arc<Account>] {
    &self.accounts
  }
}
