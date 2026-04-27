//! Generic account pool. Each account holds an `Arc<dyn Provider>`; the pool
//! itself only manages health/cooldown state and round-robin selection.

use crate::config::Config;
use crate::provider::{self, Provider};
use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    pub fn invalidate_credentials(&self) { self.provider.on_unauthorized(); }
}

pub struct AccountPool {
    accounts: Vec<Arc<Account>>,
    cursor: AtomicUsize,
    cooldown_base: Duration,
}

impl AccountPool {
    pub fn from_config(cfg: &Config) -> Result<Arc<Self>> {
        if cfg.accounts.is_empty() {
            return Err(anyhow!(
                "no accounts configured. Run `llm-router login` or `llm-router import` first."
            ));
        }
        let mut accounts = Vec::with_capacity(cfg.accounts.len());
        for a in &cfg.accounts {
            let p = provider::build_for_account(a, &cfg.copilot)?;
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

    pub fn len(&self) -> usize { self.accounts.len() }

    pub fn cooldown_base(&self) -> Duration { self.cooldown_base }

    /// Pick the next healthy account that supports the given model
    /// (round-robin). If `model` is None or no account claims support, fall
    /// back to plain round-robin over healthy accounts.
    pub fn acquire(&self, model: Option<&str>) -> Arc<Account> {
        let n = self.accounts.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);

        // First pass: healthy + supports_model
        if let Some(m) = model {
            for i in 0..n {
                let idx = (start + i) % n;
                let a = &self.accounts[idx];
                if a.is_healthy() && a.provider.supports_model(m) {
                    return a.clone();
                }
            }
        }
        // Second pass: any healthy
        for i in 0..n {
            let idx = (start + i) % n;
            let a = &self.accounts[idx];
            if a.is_healthy() {
                return a.clone();
            }
        }
        // All in cooldown: pick the one with the earliest cooldown_until.
        let mut best = &self.accounts[0];
        let mut best_t = best.inner.read().cooldown_until;
        for a in &self.accounts[1..] {
            let t = a.inner.read().cooldown_until;
            if t < best_t {
                best = a;
                best_t = t;
            }
        }
        best.clone()
    }

    pub fn all(&self) -> &[Arc<Account>] { &self.accounts }
}
