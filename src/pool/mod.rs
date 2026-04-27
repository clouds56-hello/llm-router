use crate::config::{Account as CfgAccount, Config, CopilotHeaders};
use crate::copilot;
use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex as AsyncMutex;

pub struct Account {
    pub id: String,
    pub github_token: String,
    pub headers: CopilotHeaders,
    /// Single-flight lock for token refresh.
    refresh_lock: AsyncMutex<()>,
    inner: RwLock<AccountInner>,
}

struct AccountInner {
    api_token: Option<String>,
    expires_at: Option<i64>,
    /// Cooldown until this instant (None = healthy).
    cooldown_until: Option<Instant>,
    consecutive_failures: u32,
}

impl Account {
    pub fn from_cfg(a: &CfgAccount, global: &CopilotHeaders) -> Self {
        Self {
            id: a.id.clone(),
            github_token: a.github_token.clone(),
            headers: global.merged(a.copilot.as_ref()),
            refresh_lock: AsyncMutex::new(()),
            inner: RwLock::new(AccountInner {
                api_token: a.api_token.clone(),
                expires_at: a.api_token_expires_at,
                cooldown_until: None,
                consecutive_failures: 0,
            }),
        }
    }

    pub fn snapshot(&self) -> (Option<String>, Option<i64>) {
        let g = self.inner.read();
        (g.api_token.clone(), g.expires_at)
    }

    fn is_healthy(&self) -> bool {
        match self.inner.read().cooldown_until {
            None => true,
            Some(t) => Instant::now() >= t,
        }
    }

    pub fn mark_failure(&self, cooldown_base: Duration) {
        let mut g = self.inner.write();
        g.consecutive_failures = g.consecutive_failures.saturating_add(1);
        let mult = 1u32 << (g.consecutive_failures.min(5) - 1); // up to 16x
        let cd = cooldown_base.saturating_mul(mult);
        g.cooldown_until = Some(Instant::now() + cd);
        tracing::warn!(account = %self.id, retry_in_secs = cd.as_secs(), "account in cooldown");
    }

    pub fn mark_success(&self) {
        let mut g = self.inner.write();
        g.consecutive_failures = 0;
        g.cooldown_until = None;
    }

    /// Ensure we have a non-expired Copilot API token; refresh if needed.
    pub async fn ensure_api_token(&self, client: &reqwest::Client) -> Result<String> {
        const SKEW_SECS: i64 = 300;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if let (Some(tok), Some(exp)) = self.snapshot() {
            if exp - SKEW_SECS > now {
                return Ok(tok);
            }
        }
        let _g = self.refresh_lock.lock().await;
        // re-check after acquiring lock
        if let (Some(tok), Some(exp)) = self.snapshot() {
            if exp - SKEW_SECS > now {
                return Ok(tok);
            }
        }
        let resp = copilot::token::exchange(client, &self.github_token, &self.headers).await?;
        {
            let mut g = self.inner.write();
            g.api_token = Some(resp.token.clone());
            g.expires_at = Some(resp.expires_at);
        }
        Ok(resp.token)
    }

    /// Force the next request to refresh (e.g. after a 401).
    pub fn invalidate_api_token(&self) {
        let mut g = self.inner.write();
        g.api_token = None;
        g.expires_at = None;
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
            return Err(anyhow!(
                "no accounts configured. Run `llm-router login` or `llm-router import` first."
            ));
        }
        let accounts = cfg
            .accounts
            .iter()
            .map(|a| Arc::new(Account::from_cfg(a, &cfg.copilot)))
            .collect();
        Ok(Arc::new(Self {
            accounts,
            cursor: AtomicUsize::new(0),
            cooldown_base: Duration::from_secs(cfg.pool.failure_cooldown_secs),
        }))
    }

    pub fn len(&self) -> usize { self.accounts.len() }

    pub fn cooldown_base(&self) -> Duration { self.cooldown_base }

    /// Pick the next healthy account (round-robin). Falls back to the
    /// least-cooled account if all are in cooldown.
    pub fn acquire(&self) -> Arc<Account> {
        let n = self.accounts.len();
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
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

    #[allow(dead_code)]
    pub fn all(&self) -> &[Arc<Account>] { &self.accounts }
}
