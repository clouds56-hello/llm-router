//! Generic account pool. Each account holds an `Arc<dyn Provider>`; the pool
//! manages provider-bucketed round-robin, health/cooldown state, and optional
//! session-id affinity.

pub mod affinity;

use crate::config::Config;
use crate::pool::affinity::{Affinity, Lookup};
use crate::provider::{self, Endpoint, Provider};
use parking_lot::RwLock;
use std::collections::BTreeMap;
use snafu::{ResultExt, Snafu};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

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
    warn!(
      account = %self.id,
      retry_in_secs = cd.as_secs(),
      consecutive_failures = g.consecutive_failures,
      "account in cooldown"
    );
  }

  pub fn mark_success(&self) {
    let mut g = self.inner.write();
    if g.consecutive_failures > 0 {
      debug!(account = %self.id, recovered_after = g.consecutive_failures, "account recovered");
    }
    g.consecutive_failures = 0;
    g.cooldown_until = None;
  }

  /// Notify the underlying provider that an upstream 401 happened so it can
  /// drop any cached short-lived credential.
  pub fn invalidate_credentials(&self) {
    debug!(account = %self.id, "invalidating credentials due to upstream 401");
    self.provider.on_unauthorized();
  }
}

pub struct AccountPool {
  buckets: BTreeMap<String, ProviderBucket>,
  accounts: Vec<Arc<Account>>,
  cooldown_base: Duration,
  affinity: Affinity,
}

struct ProviderBucket {
  provider: Arc<dyn Provider>,
  accounts: Vec<Arc<Account>>,
  cursor: AtomicUsize,
}

pub enum SessionAcquire {
  Account(Arc<Account>),
  SessionExpired,
  None,
}

impl AccountPool {
  pub fn from_config(cfg: &Config) -> Result<Arc<Self>> {
    if cfg.accounts.is_empty() {
      return NoAccountsSnafu.fail();
    }
    let mut accounts = Vec::with_capacity(cfg.accounts.len());
    let mut buckets: BTreeMap<String, ProviderBucket> = BTreeMap::new();
    for a in &cfg.accounts {
      let p = provider::build_for_account(a, &cfg.copilot)
        .context(BuildAccountSnafu { id: a.id.clone() })?;
      debug!(account = %a.id, provider = %p.info().id, "pool: built account");
      let acct = Arc::new(Account {
        id: a.id.clone(),
        provider: p.clone(),
        inner: RwLock::new(AccountInner {
          cooldown_until: None,
          consecutive_failures: 0,
        }),
      });
      let bucket_key = p.info().id.clone();
      buckets
        .entry(bucket_key)
        .or_insert_with(|| ProviderBucket {
          provider: p.clone(),
          accounts: Vec::new(),
          cursor: AtomicUsize::new(0),
        })
        .accounts
        .push(acct.clone());
      accounts.push(acct);
    }
    info!(
      accounts = accounts.len(),
      providers = buckets.len(),
      cooldown_base_secs = cfg.pool.failure_cooldown_secs,
      "account pool initialised"
    );
    Ok(Arc::new(Self {
      buckets,
      accounts,
      cooldown_base: Duration::from_secs(cfg.pool.failure_cooldown_secs),
      affinity: Affinity::new(
        Duration::from_secs(cfg.pool.session_ttl_secs),
        Duration::from_secs(cfg.pool.session_tombstone_secs),
      ),
    }))
  }

  pub fn len(&self) -> usize {
    self.accounts.len()
  }

  pub fn cooldown_base(&self) -> Duration {
    self.cooldown_base
  }

  pub fn acquire_for_session(
    &self,
    session_id: Option<&str>,
    model: Option<&str>,
    endpoint: Endpoint,
  ) -> SessionAcquire {
    if let Some(id) = session_id {
      match self.affinity.lookup(id) {
        Lookup::Hit(account_id) => {
          if let Some(acct) = self.account_by_id(&account_id) {
            if acct.is_healthy() && self.account_matches(&acct, model, endpoint) {
              return SessionAcquire::Account(acct);
            }
          }
        }
        Lookup::Expired => return SessionAcquire::SessionExpired,
        Lookup::Unknown => {}
      }
    }

    match self.acquire_from_buckets(model, endpoint) {
      Some(acct) => {
        if let Some(id) = session_id {
          self.record_session(id, &acct.id);
        }
        SessionAcquire::Account(acct)
      }
      None => SessionAcquire::None,
    }
  }

  pub fn record_session(&self, session_id: &str, account_id: &str) {
    self.affinity.record(session_id, account_id);
  }

  pub fn all(&self) -> &[Arc<Account>] {
    &self.accounts
  }

  fn acquire_from_buckets(&self, model: Option<&str>, endpoint: Endpoint) -> Option<Arc<Account>> {
    let mut candidates = Vec::new();
    for bucket in self.buckets.values() {
      if bucket.matches(model, endpoint) {
        candidates.push(bucket);
      }
    }

    for bucket in &candidates {
      if let Some(acct) = bucket.pick_healthy() {
        return Some(acct);
      }
    }

    let mut best: Option<Arc<Account>> = None;
    let mut best_t: Option<Instant> = None;
    for bucket in candidates {
      if let Some((acct, t)) = bucket.pick_earliest_cooldown() {
        if best.is_none() || t < best_t {
          best = Some(acct);
          best_t = t;
        }
      }
    }
    best
  }

  fn account_by_id(&self, id: &str) -> Option<Arc<Account>> {
    self.accounts.iter().find(|a| a.id == id).cloned()
  }

  fn account_matches(&self, acct: &Account, model: Option<&str>, endpoint: Endpoint) -> bool {
    let model = model.unwrap_or("");
    (model.is_empty() || acct.provider.model_info(model).is_some()) && acct.provider.supports(model, endpoint)
  }
}

impl ProviderBucket {
  fn matches(&self, model: Option<&str>, endpoint: Endpoint) -> bool {
    let model = model.unwrap_or("");
    (model.is_empty() || self.provider.model_info(model).is_some()) && self.provider.supports(model, endpoint)
  }

  fn pick_healthy(&self) -> Option<Arc<Account>> {
    let n = self.accounts.len();
    if n == 0 {
      return None;
    }
    let start = self.cursor.fetch_add(1, Ordering::Relaxed);
    for i in 0..n {
      let idx = (start + i) % n;
      let a = &self.accounts[idx];
      if a.is_healthy() {
        return Some(a.clone());
      }
    }
    None
  }

  fn pick_earliest_cooldown(&self) -> Option<(Arc<Account>, Option<Instant>)> {
    let mut best: Option<Arc<Account>> = None;
    let mut best_t: Option<Instant> = None;
    for a in &self.accounts {
      let t = a.inner.read().cooldown_until;
      if best.is_none() || t < best_t {
        best = Some(a.clone());
        best_t = t;
      }
    }
    best.map(|acct| (acct, best_t))
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::provider::{AuthKind, Capabilities, Interleaved, Limits, Modalities, ModelInfo, ProviderInfo, RequestCtx};
  use async_trait::async_trait;
  use serde_json::Value;

  struct MockProvider {
    info: ProviderInfo,
  }

  impl MockProvider {
    fn new(id: &str, aliases: &'static [&'static str], models: &[&str]) -> Arc<Self> {
      Arc::new(Self {
        info: ProviderInfo {
          id: id.into(),
          aliases,
          display_name: "mock",
          upstream_url: "https://mock.invalid".into(),
          auth_kind: AuthKind::StaticApiKey,
          default_models: models.iter().map(|m| model(m)).collect(),
        },
      })
    }
  }

  #[async_trait]
  impl Provider for MockProvider {
    fn id(&self) -> &str {
      &self.info.id
    }

    fn info(&self) -> &ProviderInfo {
      &self.info
    }

    async fn list_models(&self, _http: &reqwest::Client) -> crate::provider::Result<Value> {
      Ok(serde_json::json!({ "object": "list", "data": [] }))
    }

    async fn chat(&self, _ctx: RequestCtx<'_>) -> crate::provider::Result<reqwest::Response> {
      unreachable!()
    }
  }

  fn model(id: &str) -> ModelInfo {
    ModelInfo {
      id: id.into(),
      name: id.into(),
      capabilities: Capabilities {
        temperature: true,
        reasoning: false,
        attachment: false,
        toolcall: true,
        input: Modalities::TEXT_ONLY,
        output: Modalities::TEXT_ONLY,
        interleaved: Interleaved::Disabled(false),
      },
      cost: None,
      limit: Limits {
        context: 1,
        output: 1,
      },
      release_date: None,
    }
  }

  fn acct(id: &str, provider: Arc<dyn Provider>) -> Arc<Account> {
    Arc::new(Account {
      id: id.into(),
      provider,
      inner: RwLock::new(AccountInner {
        cooldown_until: None,
        consecutive_failures: 0,
      }),
    })
  }

  fn pool() -> AccountPool {
    static A: &[&str] = &["provider-a"];
    static B: &[&str] = &["provider-b"];
    let pa = MockProvider::new("provider-a", A, &["model-a"]);
    let pb = MockProvider::new("provider-b", B, &["model-b"]);
    let a1 = acct("a1", pa.clone());
    let a2 = acct("a2", pa.clone());
    let b1 = acct("b1", pb.clone());
    let mut buckets = BTreeMap::new();
    buckets.insert(
      "provider-a".into(),
      ProviderBucket {
        provider: pa,
        accounts: vec![a1.clone(), a2.clone()],
        cursor: AtomicUsize::new(0),
      },
    );
    buckets.insert(
      "provider-b".into(),
      ProviderBucket {
        provider: pb,
        accounts: vec![b1.clone()],
        cursor: AtomicUsize::new(0),
      },
    );
    AccountPool {
      buckets,
      accounts: vec![a1, a2, b1],
      cooldown_base: Duration::from_secs(1),
      affinity: Affinity::new(Duration::from_secs(60), Duration::from_secs(120)),
    }
  }

  #[test]
  fn routes_by_provider_model_catalogue() {
    let p = pool();
    for _ in 0..8 {
      let SessionAcquire::Account(a) = p.acquire_for_session(None, Some("model-a"), Endpoint::ChatCompletions) else {
        panic!("expected provider-a account");
      };
      assert!(a.id.starts_with('a'), "wrong account: {}", a.id);
    }
    for _ in 0..8 {
      let SessionAcquire::Account(a) = p.acquire_for_session(None, Some("model-b"), Endpoint::ChatCompletions) else {
        panic!("expected provider-b account");
      };
      assert_eq!(a.id, "b1");
    }
    assert!(matches!(
      p.acquire_for_session(None, Some("unknown"), Endpoint::ChatCompletions),
      SessionAcquire::None
    ));
  }

  #[test]
  fn session_affinity_reuses_recorded_account() {
    let p = pool();
    let SessionAcquire::Account(first) = p.acquire_for_session(Some("s1"), Some("model-a"), Endpoint::ChatCompletions) else {
      panic!("expected account");
    };
    for _ in 0..4 {
      let SessionAcquire::Account(next) = p.acquire_for_session(Some("s1"), Some("model-a"), Endpoint::ChatCompletions) else {
        panic!("expected account");
      };
      assert_eq!(next.id, first.id);
    }
  }
}
