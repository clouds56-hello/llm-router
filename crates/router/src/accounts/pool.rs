use super::affinity::{Affinity, Lookup};
use super::handle::AccountHandle;
use crate::api::routing::{RouteResolution, RouteSelector};
use llm_config::Config;
use llm_core::account::{AccountConfig, AccountTier};
use llm_core::provider::{Endpoint, Provider};
use snafu::{ResultExt, Snafu};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info};

/// Errors that can occur while constructing or querying an [`AccountPool`].
///
/// Runtime acquisition (`AccountPool::acquire`) signals "no supporting
/// account" via `Option::None` rather than an error variant - that case is
/// load-bearing for the dispatcher's 501 mapping and not a failure.
#[derive(Debug, Snafu)]
#[snafu(visibility(pub(crate)))]
pub enum Error {
  #[snafu(display("no accounts configured. Run `llm-router account add` first."))]
  NoAccounts,

  #[snafu(display("failed to build provider for account `{id}`"))]
  BuildAccount {
    id: String,
    source: llm_core::provider::Error,
  },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct AccountPool {
  buckets: BTreeMap<String, ProviderBucket>,
  accounts: Vec<Arc<AccountHandle>>,
  cooldown_base: Duration,
  affinity: Affinity,
}

struct ProviderBucket {
  provider: Arc<dyn Provider>,
  /// Accounts whose effective state is `Active`. Tried first, round-robin.
  accounts: Vec<Arc<AccountHandle>>,
  cursor: AtomicUsize,
  /// Accounts whose effective state is `Fallback`. Only consulted when
  /// every `Active` account in this bucket is unhealthy / cooled down.
  fallback_accounts: Vec<Arc<AccountHandle>>,
  fallback_cursor: AtomicUsize,
}

#[allow(dead_code)]
pub enum SessionAcquire {
  Account(Arc<AccountHandle>),
  SessionExpired,
  None,
}

pub enum EndpointAcquire {
  Account {
    acct: Arc<AccountHandle>,
    endpoint: Endpoint,
  },
  SessionExpired,
  None,
}

impl AccountPool {
  pub fn empty(cfg: &Config) -> Arc<Self> {
    Arc::new(Self {
      buckets: BTreeMap::new(),
      accounts: Vec::new(),
      cooldown_base: Duration::from_secs(cfg.pool.failure_cooldown_secs),
      affinity: Affinity::new(
        Duration::from_secs(cfg.pool.session_ttl_secs),
        Duration::from_secs(cfg.pool.session_tombstone_secs),
      ),
    })
  }

  pub fn from_accounts_with<F>(
    accounts_in: &[AccountConfig],
    cfg: &Config,
    build_provider: F,
  ) -> Result<Arc<Self>>
  where
    F: Fn(Arc<AccountConfig>) -> llm_core::provider::Result<Arc<dyn Provider>>,
  {
    if accounts_in.is_empty() {
      return NoAccountsSnafu.fail();
    }
    let mut accounts = Vec::with_capacity(accounts_in.len());
    let mut buckets: BTreeMap<String, ProviderBucket> = BTreeMap::new();
    for a in accounts_in {
      // Disabled accounts are dropped at pool construction time and so
      // never participate in routing. Re-enable via `account switch` or
      // by editing the TOML.
      if !a.enabled {
        debug!(account = %a.id, "pool: skipped disabled account");
        continue;
      }
      let cfg = Arc::new(a.clone());
      let p = build_provider(cfg.clone()).context(BuildAccountSnafu { id: a.id.clone() })?;
      debug!(account = %a.id, provider = %p.info().id, tier = ?a.tier, "pool: built account");
      let acct = Arc::new(AccountHandle::new(cfg, p.clone()));
      let bucket_key = p.info().id.clone();
      let bucket = buckets.entry(bucket_key).or_insert_with(|| ProviderBucket {
        provider: p.clone(),
        accounts: Vec::new(),
        cursor: AtomicUsize::new(0),
        fallback_accounts: Vec::new(),
        fallback_cursor: AtomicUsize::new(0),
      });
      match a.tier {
        AccountTier::Active => bucket.accounts.push(acct.clone()),
        AccountTier::Fallback => bucket.fallback_accounts.push(acct.clone()),
      }
      accounts.push(acct);
    }
    if accounts.is_empty() {
      return NoAccountsSnafu.fail();
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

  #[allow(dead_code)]
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
          self.record_session(id, &acct.id());
        }
        SessionAcquire::Account(acct)
      }
      None => SessionAcquire::None,
    }
  }

  pub fn acquire_for_session_convertible(
    &self,
    session_id: Option<&str>,
    model: Option<&str>,
    requested: Endpoint,
  ) -> EndpointAcquire {
    if let Some(id) = session_id {
      match self.affinity.lookup(id) {
        Lookup::Hit(account_id) => {
          if let Some(acct) = self.account_by_id(&account_id) {
            if acct.is_healthy() {
              if let Some(endpoint) = self.account_matching_endpoint(&acct, model, requested) {
                return EndpointAcquire::Account { acct, endpoint };
              }
            }
          }
        }
        Lookup::Expired => return EndpointAcquire::SessionExpired,
        Lookup::Unknown => {}
      }
    }

    match self.acquire_from_buckets_convertible(model, requested) {
      Some((acct, endpoint)) => {
        if let Some(id) = session_id {
          self.record_session(id, &acct.id());
        }
        EndpointAcquire::Account { acct, endpoint }
      }
      None => EndpointAcquire::None,
    }
  }

  pub fn acquire_for_route(
    &self,
    session_id: Option<&str>,
    route: &RouteResolution,
    requested: Endpoint,
  ) -> EndpointAcquire {
    if let Some(id) = session_id {
      match self.affinity.lookup(id) {
        Lookup::Hit(account_id) => {
          if let Some(acct) = self.account_by_id(&account_id) {
            if acct.is_healthy() {
              if let Some(endpoint) = self.account_matching_route_endpoint(&acct, route, requested) {
                return EndpointAcquire::Account { acct, endpoint };
              }
            }
          }
        }
        Lookup::Expired => return EndpointAcquire::SessionExpired,
        Lookup::Unknown => {}
      }
    }

    match self.acquire_from_route(route, requested) {
      Some((acct, endpoint)) => {
        if let Some(id) = session_id {
          self.record_session(id, &acct.id());
        }
        EndpointAcquire::Account { acct, endpoint }
      }
      None => EndpointAcquire::None,
    }
  }

  pub fn record_session(&self, session_id: &str, account_id: &str) {
    self.affinity.record(session_id, account_id);
  }

  pub fn all(&self) -> &[Arc<AccountHandle>] {
    &self.accounts
  }

  #[allow(dead_code)]
  fn acquire_from_buckets(&self, model: Option<&str>, endpoint: Endpoint) -> Option<Arc<AccountHandle>> {
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

    let mut best: Option<Arc<AccountHandle>> = None;
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

  fn acquire_from_buckets_convertible(
    &self,
    model: Option<&str>,
    requested: Endpoint,
  ) -> Option<(Arc<AccountHandle>, Endpoint)> {
    for endpoint in fallback_order(requested) {
      let mut candidates = Vec::new();
      for bucket in self.buckets.values() {
        if bucket.matches(model, endpoint) {
          candidates.push(bucket);
        }
      }

      for bucket in &candidates {
        if let Some(acct) = bucket.pick_healthy() {
          return Some((acct, endpoint));
        }
      }

      let mut best: Option<Arc<AccountHandle>> = None;
      let mut best_t: Option<Instant> = None;
      for bucket in candidates {
        if let Some((acct, t)) = bucket.pick_earliest_cooldown() {
          if best.is_none() || t < best_t {
            best = Some(acct);
            best_t = t;
          }
        }
      }
      if let Some(acct) = best {
        return Some((acct, endpoint));
      }
    }
    None
  }

  fn account_by_id(&self, id: &str) -> Option<Arc<AccountHandle>> {
    self.accounts.iter().find(|a| a.config.load().id == id).cloned()
  }

  fn account_matches(&self, acct: &AccountHandle, model: Option<&str>, endpoint: Endpoint) -> bool {
    let model = model.unwrap_or("");
    (model.is_empty() || acct.provider.model_info(model).is_some()) && acct.provider.supports(model, endpoint)
  }

  fn account_matching_endpoint(
    &self,
    acct: &AccountHandle,
    model: Option<&str>,
    requested: Endpoint,
  ) -> Option<Endpoint> {
    fallback_order(requested)
      .into_iter()
      .find(|endpoint| self.account_matches(acct, model, *endpoint))
  }

  fn account_matching_route_endpoint(
    &self,
    acct: &AccountHandle,
    route: &RouteResolution,
    requested: Endpoint,
  ) -> Option<Endpoint> {
    fallback_order(requested)
      .into_iter()
      .find(|endpoint| self.account_matches_route(acct, route, *endpoint))
  }

  fn account_matches_route(&self, acct: &AccountHandle, route: &RouteResolution, endpoint: Endpoint) -> bool {
    match &route.selector {
      RouteSelector::Any => acct.provider.supports(&route.upstream_model, endpoint),
      RouteSelector::Provider(provider) => {
        acct.provider.info().id == *provider && self.account_matches(acct, Some(&route.upstream_model), endpoint)
      }
      RouteSelector::Model => self.account_matches(acct, Some(&route.upstream_model), endpoint),
      RouteSelector::Fuzzy { candidates } => candidates
        .iter()
        .any(|candidate| self.account_matches(acct, Some(candidate), endpoint)),
    }
  }

  fn acquire_from_route(&self, route: &RouteResolution, requested: Endpoint) -> Option<(Arc<AccountHandle>, Endpoint)> {
    match &route.selector {
      RouteSelector::Any => self.acquire_any_convertible(requested),
      RouteSelector::Provider(provider) => {
        self.acquire_provider_convertible(provider, &route.upstream_model, requested)
      }
      RouteSelector::Model => self.acquire_from_buckets_convertible(Some(&route.upstream_model), requested),
      RouteSelector::Fuzzy { candidates } => {
        for candidate in candidates {
          if let Some((acct, endpoint)) = self.acquire_from_buckets_convertible(Some(candidate), requested) {
            return Some((acct, endpoint));
          }
        }
        None
      }
    }
  }

  fn acquire_any_convertible(&self, requested: Endpoint) -> Option<(Arc<AccountHandle>, Endpoint)> {
    for endpoint in fallback_order(requested) {
      for bucket in self.buckets.values() {
        if bucket.provider.supports("", endpoint) {
          if let Some(acct) = bucket.pick_healthy() {
            return Some((acct, endpoint));
          }
        }
      }

      let mut best: Option<Arc<AccountHandle>> = None;
      let mut best_t: Option<Instant> = None;
      for bucket in self.buckets.values() {
        if bucket.provider.supports("", endpoint) {
          if let Some((acct, t)) = bucket.pick_earliest_cooldown() {
            if best.is_none() || t < best_t {
              best = Some(acct);
              best_t = t;
            }
          }
        }
      }
      if let Some(acct) = best {
        return Some((acct, endpoint));
      }
    }
    None
  }

  fn acquire_provider_convertible(
    &self,
    provider: &str,
    model: &str,
    requested: Endpoint,
  ) -> Option<(Arc<AccountHandle>, Endpoint)> {
    for endpoint in fallback_order(requested) {
      let Some(bucket) = self.buckets.get(provider) else {
        return None;
      };
      if !bucket.matches(Some(model), endpoint) {
        continue;
      }
      if let Some(acct) = bucket.pick_healthy() {
        return Some((acct, endpoint));
      }
      if let Some((acct, _)) = bucket.pick_earliest_cooldown() {
        return Some((acct, endpoint));
      }
    }
    None
  }
}

fn fallback_order(requested: Endpoint) -> Vec<Endpoint> {
  match requested {
    Endpoint::ChatCompletions => vec![Endpoint::ChatCompletions, Endpoint::Responses, Endpoint::Messages],
    Endpoint::Responses => vec![Endpoint::Responses, Endpoint::ChatCompletions, Endpoint::Messages],
    Endpoint::Messages => vec![Endpoint::Messages, Endpoint::ChatCompletions, Endpoint::Responses],
  }
}

impl ProviderBucket {
  fn matches(&self, model: Option<&str>, endpoint: Endpoint) -> bool {
    let model = model.unwrap_or("");
    (model.is_empty() || self.provider.model_info(model).is_some()) && self.provider.supports(model, endpoint)
  }

  fn pick_healthy(&self) -> Option<Arc<AccountHandle>> {
    if let Some(a) = pick_healthy_rr(&self.accounts, &self.cursor) {
      return Some(a);
    }
    pick_healthy_rr(&self.fallback_accounts, &self.fallback_cursor)
  }

  fn pick_earliest_cooldown(&self) -> Option<(Arc<AccountHandle>, Option<Instant>)> {
    if let Some(out) = earliest_cooldown(&self.accounts) {
      return Some(out);
    }
    earliest_cooldown(&self.fallback_accounts)
  }
}

fn pick_healthy_rr(accounts: &[Arc<AccountHandle>], cursor: &AtomicUsize) -> Option<Arc<AccountHandle>> {
  let n = accounts.len();
  if n == 0 {
    return None;
  }
  let start = cursor.fetch_add(1, Ordering::Relaxed);
  for i in 0..n {
    let idx = (start + i) % n;
    let a = &accounts[idx];
    if a.is_healthy() {
      return Some(a.clone());
    }
  }
  None
}

fn earliest_cooldown(accounts: &[Arc<AccountHandle>]) -> Option<(Arc<AccountHandle>, Option<Instant>)> {
  let mut best: Option<Arc<AccountHandle>> = None;
  let mut best_t: Option<Instant> = None;
  for a in accounts {
    let t = a.cooldown_until();
    if best.is_none() || t < best_t {
      best = Some(a.clone());
      best_t = t;
    }
  }
  best.map(|acct| (acct, best_t))
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
      limit: Limits { context: 1, output: 1 },
      release_date: None,
    }
  }

  fn acct(id: &str, provider: Arc<dyn Provider>) -> Arc<AccountHandle> {
    acct_tier(id, provider, AccountTier::Active)
  }

  fn acct_tier(id: &str, provider: Arc<dyn Provider>, tier: AccountTier) -> Arc<AccountHandle> {
    Arc::new(AccountHandle::new(
      Arc::new(AccountConfig {
        id: id.into(),
        provider: provider.info().id.clone(),
        enabled: true,
        tier,
        tags: Vec::new(),
        label: None,
        base_url: None,
        headers: BTreeMap::new(),
        auth_type: None,
        username: None,
        api_key: None,
        api_key_expires_at: None,
        access_token: None,
        access_token_expires_at: None,
        id_token: None,
        refresh_token: None,
        extra: BTreeMap::new(),
        refresh_url: None,
        last_refresh: None,
        settings: toml::Table::new(),
      }),
      provider,
    ))
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
        fallback_accounts: Vec::new(),
        fallback_cursor: AtomicUsize::new(0),
      },
    );
    buckets.insert(
      "provider-b".into(),
      ProviderBucket {
        provider: pb,
        accounts: vec![b1.clone()],
        cursor: AtomicUsize::new(0),
        fallback_accounts: Vec::new(),
        fallback_cursor: AtomicUsize::new(0),
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
      assert!(a.id().starts_with('a'), "wrong account: {}", a.id());
    }
    for _ in 0..8 {
      let SessionAcquire::Account(a) = p.acquire_for_session(None, Some("model-b"), Endpoint::ChatCompletions) else {
        panic!("expected provider-b account");
      };
      assert_eq!(a.id(), "b1");
    }
    assert!(matches!(
      p.acquire_for_session(None, Some("unknown"), Endpoint::ChatCompletions),
      SessionAcquire::None
    ));
  }

  #[test]
  fn session_affinity_reuses_recorded_account() {
    let p = pool();
    let SessionAcquire::Account(first) = p.acquire_for_session(Some("s1"), Some("model-a"), Endpoint::ChatCompletions)
    else {
      panic!("expected account");
    };
    for _ in 0..4 {
      let SessionAcquire::Account(next) = p.acquire_for_session(Some("s1"), Some("model-a"), Endpoint::ChatCompletions)
      else {
        panic!("expected account");
      };
      assert_eq!(next.id(), first.id());
    }
  }
}
