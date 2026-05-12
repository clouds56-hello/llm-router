use crate::cli::import::ImportArgs;
use crate::cli::login::LoginArgs;
use crate::config::{Account, AccountState, AccountTier, Config};
use crate::provider::{ID_GITHUB_COPILOT, ZAI_PROVIDERS};
use crate::util::http::build_client;
use crate::util::secret::Secret;
use crate::util::timefmt::{relative_from_now, relative_from_now_ms};
use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use llm_auth::AuthStore;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
  /// List configured accounts (grouped by provider, sorted by tier)
  List(ListArgs),
  /// Remove an account by id
  Remove { id: String },
  /// Show details for an account
  Show { id: String },
  /// Add an account interactively (provider → credential source → id)
  Add(AddArgs),
  /// Add a Copilot account via GitHub device-flow login
  Login(LoginArgs),
  /// Import an existing GitHub token (from `gh` or the Copilot plugin),
  /// or a static API key (from an env var). Flag-driven; suitable for CI.
  Import(ImportArgs),
  /// Force-refresh an account's short-lived access token (no-op for
  /// providers that use a static API key)
  Refresh { id: String },
  /// Print one-line per-account status (gh-auth-style)
  Status { id: Option<String> },
  /// Change account activation tiers (active / fallback / disabled).
  /// See `--only`, `--all`, and repeatable `--account` flags.
  Switch(SwitchArgs),
}

#[derive(Args, Debug)]
pub struct ListArgs {
  /// Skip live upstream quota lookups (faster, no network).
  #[arg(long)]
  pub no_quota: bool,
  /// Per-upstream timeout in seconds for the live quota probe.
  #[arg(long, default_value_t = 5u64)]
  pub timeout: u64,
}

#[derive(Args, Debug)]
pub struct AddArgs {
  /// Provider id (skip the provider picker).
  #[arg(long)]
  pub provider: Option<String>,
  /// Account id (skip the id prompt).
  #[arg(long)]
  pub id: Option<String>,
}

/// Activation surface. Three mutually-exclusive primary modes:
///
/// 1. `--only <id>` — set `<id>` Active and demote every other enabled
///    account in the same provider to Fallback.
/// 2. `--all --provider <p>` — set every enabled account in provider `<p>`
///    to Active.
/// 3. `--account <id>` (repeatable) — set each listed `<id>` to Active and
///    demote every other enabled account in the affected providers to
///    Fallback.
#[derive(Args, Debug)]
pub struct SwitchArgs {
  /// Mode 1. Single Active account; others (same provider) become Fallback.
  #[arg(long, value_name = "ID")]
  pub only: Option<String>,

  /// Mode 2. Mark every enabled account of `--provider` as Active.
  #[arg(long, requires = "provider", conflicts_with_all = ["only", "account_multi"])]
  pub all: bool,

  /// Provider scope for `--all`.
  #[arg(long, value_name = "ID")]
  pub provider: Option<String>,

  /// Mode 3. Repeatable: each listed account becomes Active; other enabled
  /// accounts in the same provider(s) are demoted to Fallback.
  #[arg(long = "account", value_name = "ID", conflicts_with_all = ["only", "all"])]
  pub account_multi: Vec<String>,

  /// Also operate on currently-disabled accounts (re-enable as needed).
  #[arg(long)]
  pub include_disabled: bool,
}

pub async fn run(cfg_path: Option<PathBuf>, cmd: AccountCmd) -> Result<()> {
  let (cfg, path) = Config::load(cfg_path.as_deref())?;
  let mut store = AuthStore::load(None, Some(&path))?;
  match cmd {
    AccountCmd::List(args) => list(&cfg, &mut store, args).await?,
    AccountCmd::Remove { id } => {
      let removed = store.remove(&id).ok_or_else(|| anyhow!("no account with id '{id}'"))?;
      store.save()?;
      tracing::info!(account = %removed.id, remaining = store.accounts.len(), "account removed");
      println!("Removed '{id}'");
    }
    AccountCmd::Show { id } => show(&store, &id)?,
    AccountCmd::Add(args) => add(cfg_path, args).await?,
    AccountCmd::Login(args) => crate::cli::login::run(cfg_path, args).await?,
    AccountCmd::Import(args) => crate::cli::import::run(cfg_path, args).await?,
    AccountCmd::Refresh { id } => refresh(&cfg, &mut store, &id).await?,
    AccountCmd::Status { id } => status(&cfg, &mut store, id).await?,
    AccountCmd::Switch(args) => switch(&mut store, args)?,
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(cfg: &Config, store: &mut AuthStore, args: ListArgs) -> Result<()> {
  if store.accounts.is_empty() {
    println!("(no accounts)");
    return Ok(());
  }

  // Fetch quotas concurrently. Each future is wrapped in an outer timeout
  // so a single hung upstream cannot freeze the entire CLI invocation.
  let quotas: Vec<QuotaResult> = if args.no_quota {
    store.accounts.iter().map(|_| QuotaResult::Skipped).collect()
  } else {
    let http = build_client(&cfg.proxy)?;
    let timeout = Duration::from_secs(args.timeout.max(1));
    let futs = store
      .accounts
      .iter()
      .map(|a| fetch_quota(http.clone(), a.clone(), timeout));
    futures::future::join_all(futs).await
  };

  // Persist any refreshed Copilot access tokens. The quota probe already calls
  // `token::exchange`, so we piggy-back on its result instead of issuing a
  // second request. This is a no-op under `--no-quota`.
  let mut dirty = false;
  for (a, q) in store.accounts.iter_mut().zip(quotas.iter()) {
    if let QuotaResult::Copilot(c) = q {
      let same_tok = a
        .access_token
        .as_ref()
        .map(|s| s.expose().as_str() == c.fresh_token.as_str())
        .unwrap_or(false);
      if !same_tok || a.access_token_expires_at != Some(c.fresh_expires_at) {
        a.access_token = Some(crate::util::secret::Secret::new(c.fresh_token.clone()));
        a.access_token_expires_at = Some(c.fresh_expires_at);
        a.last_refresh = Some(time::OffsetDateTime::now_utc().unix_timestamp());
        dirty = true;
      }
    }
  }
  if dirty {
    store.save()?;
  }

  // Render: group by provider (alphabetical), within each group sort by
  // effective state (Active → Fallback → Disabled). Account index in the
  // original Vec is preserved so we can pick the right quota slot.
  let mut by_provider: BTreeMap<String, Vec<usize>> = BTreeMap::new();
  for (i, a) in store.accounts.iter().enumerate() {
    by_provider.entry(a.provider.clone()).or_default().push(i);
  }
  let mut first = true;
  for (provider, mut idxs) in by_provider {
    idxs.sort_by_key(|&i| state_sort_key(store.accounts[i].state()));
    if !first {
      println!();
    }
    first = false;
    println!("# {provider}");
    for i in idxs {
      render_account(&store.accounts[i], &quotas[i]);
    }
  }
  Ok(())
}

#[derive(Debug)]
enum QuotaResult {
  Skipped,
  None, // not applicable to this provider
  Copilot(CopilotMonthly),
  Zai(crate::provider::zai::quota::ZaiQuota),
  Err(String),
}

#[derive(Debug)]
struct CopilotMonthly {
  /// Headline figure to display: `(label, remaining, Some(entitlement))`
  /// for metered features, `(label, 0, None)` rendered as "unlimited".
  headline: Option<(String, u64, Option<u64>)>,
  /// Marketing plan name (e.g. `individual_pro`).
  plan: Option<String>,
  reset_date: Option<String>, // ISO YYYY-MM-DD
  /// Fresh short-lived Copilot access token returned by the same exchange
  /// call that produced the quota. Persisted back to config so that the
  /// daemon (which never writes to disk at runtime) starts up with a
  /// non-expired cache.
  fresh_token: String,
  fresh_expires_at: i64,
}

async fn fetch_quota(http: reqwest::Client, account: Account, timeout: Duration) -> QuotaResult {
  if account.provider == ID_GITHUB_COPILOT {
    let Some(gh) = account.refresh_token.clone() else {
      return QuotaResult::None;
    };
    let header_value = serde_json::to_value(&account.settings).unwrap_or_else(|_| serde_json::json!({}));
    let core_headers = match llm_provider_copilot::config::CopilotHeaders::from_value(&header_value) {
      Ok(h) => h,
      Err(e) => return QuotaResult::Err(short_err(&e)),
    };
    // Two parallel probes:
    //   1. token exchange — refreshes access_token (always needed on `list`)
    //   2. user-info     — quota_snapshots (Plus/Business plans)
    let http2 = http.clone();
    let gh2 = gh.clone();
    let h2 = core_headers.clone();
    let fut = async move {
      let (tok, info) = tokio::join!(
        crate::provider::github_copilot::token::exchange(&http, gh.expose(), &core_headers),
        crate::provider::github_copilot::user::fetch(&http2, gh2.expose(), &h2),
      );
      (tok, info)
    };
    return match tokio::time::timeout(timeout, fut).await {
      Err(_) => QuotaResult::Err("timeout".into()),
      Ok((Err(e), _)) => QuotaResult::Err(short_err(&e)),
      Ok((Ok(tok), info_res)) => {
        // Pick the most informative bucket:
        //   premium_interactions (metered on Plus) > chat > completions.
        // Fall back to the first metered snapshot if the well-known
        // ones are all unlimited.
        let headline = info_res.as_ref().ok().and_then(pick_headline);
        let (plan, reset_date) = info_res
          .as_ref()
          .map(|i| (i.copilot_plan.clone(), i.quota_reset_date.clone()))
          .unwrap_or((None, None));
        QuotaResult::Copilot(CopilotMonthly {
          headline,
          plan,
          reset_date,
          fresh_token: tok.token,
          fresh_expires_at: tok.expires_at,
        })
      }
    };
  }

  if ZAI_PROVIDERS.contains(&account.provider.as_str()) {
    let Some(key) = account.api_key.clone() else {
      return QuotaResult::None;
    };
    let provider = account.provider.clone();
    let fut = async move { crate::provider::zai::quota::fetch(&http, &provider, key.expose()).await };
    return match tokio::time::timeout(timeout, fut).await {
      Err(_) => QuotaResult::Err("timeout".into()),
      Ok(Err(e)) => QuotaResult::Err(short_err(&e)),
      Ok(Ok(q)) => QuotaResult::Zai(q),
    };
  }

  QuotaResult::None
}

fn short_err<E: std::fmt::Display>(e: &E) -> String {
  let s = e.to_string();
  if s.len() > 80 {
    format!("{}…", &s[..80])
  } else {
    s
  }
}

fn render_account(a: &Account, q: &QuotaResult) {
  println!("[{}] {}", state_marker(a.state()), a.id);

  let has = a.access_token.is_some() || a.api_key.is_some() || a.refresh_token.is_some();
  println!("  credentials : {}", if has { "present" } else { "missing" });

  // Expiry: short-lived OAuth (access_token_expires_at) vs static api_key.
  match a.access_token_expires_at {
    Some(ts) => println!("  expires     : {} (access_token)", relative_from_now(ts)),
    None if a.api_key.is_some() => println!("  expires     : never (static api_key)"),
    None => println!("  expires     : -"),
  }

  match q {
    QuotaResult::Skipped => {}
    QuotaResult::None => {}
    QuotaResult::Err(e) => println!("  quota       : unavailable ({e})"),
    QuotaResult::Copilot(c) => {
      if c.headline.is_some() || c.reset_date.is_some() || c.plan.is_some() {
        render_copilot(c);
      }
    }
    QuotaResult::Zai(z) => render_zai(z),
  }
}

fn render_copilot(c: &CopilotMonthly) {
  // Copilot's premium-request budget is a *monthly* counter; we display
  // remaining count + reset date for metered features, "unlimited" for
  // unmetered ones (e.g. chat on Plus).
  if let Some(plan) = &c.plan {
    println!("  copilot plan: {plan}");
  }
  let reset = c
    .reset_date
    .as_deref()
    .map(|d| format!(" — resets {d}"))
    .unwrap_or_default();
  match &c.headline {
    Some((label, remaining, Some(entitlement))) => {
      let pct = if *entitlement > 0 {
        100.0 * (*remaining as f64) / (*entitlement as f64)
      } else {
        0.0
      };
      println!("  copilot     : {remaining} / {entitlement} {label} ({pct:.1}%){reset}");
    }
    Some((label, _, None)) => {
      println!("  copilot     : unlimited {label}{reset}");
    }
    None => {
      // No metered feature reported but we have a reset date or plan.
      if !reset.is_empty() {
        println!("  copilot     : monthly quota{reset}");
      }
    }
  }
}

/// Pick the most informative quota snapshot for one-line display.
///
/// Preference order:
///   1. `premium_interactions` (the visible Plus quota)
///   2. `chat`
///   3. `completions`
///   4. first remaining metered snapshot
///
/// For unmetered features we still surface them (as `unlimited <label>`),
/// but only if no metered candidate is available.
fn pick_headline(info: &crate::provider::github_copilot::user::CopilotUserInfo) -> Option<(String, u64, Option<u64>)> {
  let snaps = &info.quota_snapshots;
  let preferred = ["premium_interactions", "chat", "completions"];

  // First pass: preferred metered.
  for k in preferred {
    if let Some(s) = snaps.get(k) {
      if !s.unlimited {
        return Some((k.to_string(), s.remaining.unwrap_or(0), s.entitlement));
      }
    }
  }
  // Second pass: any metered.
  for (k, s) in snaps {
    if !s.unlimited && s.entitlement.is_some() {
      return Some((k.clone(), s.remaining.unwrap_or(0), s.entitlement));
    }
  }
  // Third pass: preferred unmetered.
  for k in preferred {
    if snaps.get(k).map(|s| s.unlimited).unwrap_or(false) {
      return Some((k.to_string(), 0, None));
    }
  }
  None
}

fn render_zai(z: &crate::provider::zai::quota::ZaiQuota) {
  if let Some(level) = &z.level {
    println!("  zai plan    : {level}");
  }
  if let Some(b) = &z.five_hour {
    println!("  5h tokens   : {}", fmt_token_bucket(b));
  }
  if let Some(b) = &z.weekly {
    println!("  weekly tok  : {}", fmt_token_bucket(b));
  }
  if let Some(m) = &z.mcp_monthly {
    let reset = m
      .next_reset_ms
      .map(|t| format!(" — resets {}", relative_from_now_ms(t)))
      .unwrap_or_default();
    println!(
      "  mcp monthly : {} / {} ({:.1}%){reset}",
      m.used, m.total, m.percent_used
    );
  }
}

fn fmt_token_bucket(b: &crate::provider::zai::quota::TokenBucket) -> String {
  let total = b.total.map(|t| format!(" of {}", fmt_int(t))).unwrap_or_default();
  let reset = b
    .next_reset_ms
    .map(|t| format!(" — resets {}", relative_from_now_ms(t)))
    .unwrap_or_default();
  format!("{:.1}%{total}{reset}", b.percent_used)
}

fn fmt_int(mut n: u64) -> String {
  // Thousands separator without pulling in num-format.
  if n == 0 {
    return "0".into();
  }
  let mut parts = Vec::new();
  while n > 0 {
    parts.push(format!("{:03}", n % 1000));
    n /= 1000;
  }
  let mut out = parts.pop().unwrap().trim_start_matches('0').to_string();
  if out.is_empty() {
    out.push('0');
  }
  while let Some(p) = parts.pop() {
    out.push(',');
    out.push_str(&p);
  }
  out
}

// ---------------------------------------------------------------------------
// show (unchanged behaviour, lifted into a helper)
// ---------------------------------------------------------------------------

fn show(store: &AuthStore, id: &str) -> Result<()> {
  let a = store
    .get(id)
    .ok_or_else(|| anyhow!("no account with id '{id}'"))?;
  println!("id: {}", a.id);
  println!("provider: {}", a.provider);
  println!("enabled: {}", a.enabled);
  println!("state: {}", state_label(a.state()));
  if !a.tags.is_empty() {
    println!("tags: {}", a.tags.join(", "));
  }
  if let Some(label) = &a.label {
    println!("label: {label}");
  }
  if let Some(refresh) = a.refresh_token.as_ref().map(|s| s.expose()) {
    println!("refresh_token: {}", mask(refresh));
  }
  if let Some(k) = a.api_key.as_ref().map(|s| s.expose()) {
    println!("api_key: {}", mask(k));
  }
  if a.access_token.is_some() || a.access_token_expires_at.is_some() {
    println!(
      "access_token: {}",
      a.access_token
        .as_ref()
        .map(|s| mask(s.expose()))
        .unwrap_or_else(|| "-".into())
    );
    match a.access_token_expires_at {
      Some(ts) => println!("access_token_expires_at: {ts} ({})", relative_from_now(ts)),
      None => println!("access_token_expires_at: -"),
    }
  }
  if let Some(b) = &a.base_url {
    println!("base_url: {b}");
  }
  if let Some(ts) = a.last_refresh {
    println!("last_refresh: {ts} ({})", relative_from_now(ts));
  }
  if !a.settings.is_empty() {
    println!("settings: {} keys", a.settings.len());
  }
  Ok(())
}

fn mask(s: &str) -> String {
  let n = s.len();
  if n <= 8 {
    return "***".into();
  }
  format!("{}…{}", &s[..4], &s[n - 4..])
}

// ---------------------------------------------------------------------------
// state / tier helpers
// ---------------------------------------------------------------------------

fn state_marker(s: AccountState) -> char {
  match s {
    AccountState::Active => 'A',
    AccountState::Fallback => 'F',
    AccountState::Disabled => 'D',
  }
}

fn state_sort_key(s: AccountState) -> u8 {
  match s {
    AccountState::Active => 0,
    AccountState::Fallback => 1,
    AccountState::Disabled => 2,
  }
}

fn state_label(s: AccountState) -> &'static str {
  match s {
    AccountState::Active => "active",
    AccountState::Fallback => "fallback",
    AccountState::Disabled => "disabled",
  }
}

// ---------------------------------------------------------------------------
// add (interactive wizard)
// ---------------------------------------------------------------------------

async fn add(cfg_path: Option<PathBuf>, args: AddArgs) -> Result<()> {
  let (cfg, path) = Config::load(cfg_path.as_deref())?;
  let mut store = AuthStore::load(None, Some(&path))?;
  let client = build_client(&cfg.proxy)?;
  let account = crate::cli::onboarding::interactive_add_account(&client, args.provider, args.id).await?;
  let id = account.id.clone();
  let provider = account.provider.clone();
  store.upsert(account);
  store.save()?;
  tracing::info!(account = %id, %provider, path = %store.path().display(), "account added");
  println!("Saved account '{id}' ({provider}) to {}", store.path().display());
  Ok(())
}

// ---------------------------------------------------------------------------
// refresh (force token re-exchange for github-copilot)
// ---------------------------------------------------------------------------

async fn refresh(cfg: &Config, store: &mut AuthStore, id: &str) -> Result<()> {
  let account = store
    .get(id)
    .ok_or_else(|| anyhow!("no account with id '{id}'"))?
    .clone();

  if account.provider != ID_GITHUB_COPILOT {
    if account.api_key.is_some() {
      println!("nothing to refresh: provider '{}' uses a static api_key", account.provider);
      return Ok(());
    }
    bail!("refresh is only supported for github-copilot accounts");
  }
  let Some(gh) = account.refresh_token.clone() else {
    bail!("account '{id}' has no refresh_token; re-add it via `account login` or `account import`");
  };
  let header_value = serde_json::to_value(toml::Value::Table(account.settings.clone()))
    .unwrap_or(serde_json::Value::Null);
  let core_headers = llm_provider_copilot::config::CopilotHeaders::from_value(&header_value)
    .map_err(|e| anyhow!("invalid copilot headers: {e}"))?;
  let http = build_client(&cfg.proxy)?;
  let resp = crate::provider::github_copilot::token::exchange(&http, gh.expose(), &core_headers)
    .await
    .map_err(|e| anyhow!("token exchange failed: {e}"))?;
  let acct = store.get_mut(id).expect("checked above");
  acct.access_token = Some(Secret::new(resp.token));
  acct.access_token_expires_at = Some(resp.expires_at);
  acct.last_refresh = Some(time::OffsetDateTime::now_utc().unix_timestamp());
  store.save()?;
  tracing::info!(account = %id, "access token refreshed");
  println!(
    "Refreshed '{id}': access_token expires {}",
    relative_from_now(resp.expires_at)
  );
  Ok(())
}

// ---------------------------------------------------------------------------
// status (gh-auth-style one-line per account)
// ---------------------------------------------------------------------------

async fn status(cfg: &Config, store: &mut AuthStore, id: Option<String>) -> Result<()> {
  if store.accounts.is_empty() {
    println!("(no accounts) — run `llm-router account add` to add one");
    return Ok(());
  }
  let timeout = Duration::from_secs(5);
  let http = build_client(&cfg.proxy)?;
  let futs = store
    .accounts
    .iter()
    .map(|a| fetch_quota(http.clone(), a.clone(), timeout));
  let quotas: Vec<QuotaResult> = futures::future::join_all(futs).await;

  // Persist any token side-effects, same as `list`.
  let mut dirty = false;
  for (a, q) in store.accounts.iter_mut().zip(quotas.iter()) {
    if let QuotaResult::Copilot(c) = q {
      let same_tok = a
        .access_token
        .as_ref()
        .map(|s| s.expose().as_str() == c.fresh_token.as_str())
        .unwrap_or(false);
      if !same_tok || a.access_token_expires_at != Some(c.fresh_expires_at) {
        a.access_token = Some(Secret::new(c.fresh_token.clone()));
        a.access_token_expires_at = Some(c.fresh_expires_at);
        a.last_refresh = Some(time::OffsetDateTime::now_utc().unix_timestamp());
        dirty = true;
      }
    }
  }
  if dirty {
    store.save()?;
  }

  let mut shown = 0usize;
  for (a, q) in store.accounts.iter().zip(quotas.iter()) {
    if let Some(filter) = &id {
      if a.id != *filter {
        continue;
      }
    }
    print_status_line(a, q);
    shown += 1;
  }
  if shown == 0 {
    bail!("no account with id '{}'", id.unwrap_or_default());
  }
  Ok(())
}

fn print_status_line(a: &Account, q: &QuotaResult) {
  let state = state_label(a.state());
  let expiry = match a.access_token_expires_at {
    Some(ts) => relative_from_now(ts),
    None if a.api_key.is_some() => "static".into(),
    None => "-".into(),
  };
  let extra = match q {
    QuotaResult::Copilot(c) => c.plan.clone().unwrap_or_default(),
    QuotaResult::Zai(z) => z.level.clone().unwrap_or_default(),
    QuotaResult::Err(e) => format!("quota: {e}"),
    _ => String::new(),
  };
  let extra = if extra.is_empty() {
    String::new()
  } else {
    format!(" · {extra}")
  };
  println!("{} ({}) [{state}] · expires {expiry}{extra}", a.id, a.provider);
}

// ---------------------------------------------------------------------------
// switch (tri-state activation)
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
struct SwitchChange {
  id: String,
  provider: String,
  old: AccountState,
  new: AccountState,
}

fn switch(store: &mut AuthStore, args: SwitchArgs) -> Result<()> {
  let changes = apply_switch(&mut store.accounts, &args)?;
  if changes.is_empty() {
    println!("no changes");
    return Ok(());
  }
  store.save()?;
  for c in &changes {
    println!(
      "{}  ({})  {} → {}",
      c.id,
      c.provider,
      state_label(c.old),
      state_label(c.new)
    );
  }
  tracing::info!(changes = changes.len(), "account switch applied");
  Ok(())
}

/// Pure mutation kernel for `switch`. Extracted for unit-testing.
fn apply_switch(accounts: &mut [Account], args: &SwitchArgs) -> Result<Vec<SwitchChange>> {
  // Validate exactly one mode is set.
  let modes_set = (args.only.is_some() as u8) + (args.all as u8) + (!args.account_multi.is_empty() as u8);
  if modes_set == 0 {
    bail!("specify exactly one of `--only <id>`, `--all --provider <p>`, or `--account <id>` (repeatable)");
  }
  if modes_set > 1 {
    bail!("`--only`, `--all`, and `--account` are mutually exclusive");
  }

  // Resolve the set of target ids and the affected provider scope.
  let (active_ids, providers): (Vec<String>, Vec<String>) = if let Some(id) = &args.only {
    let provider = lookup_provider(accounts, id)?;
    (vec![id.clone()], vec![provider])
  } else if args.all {
    let p = args.provider.clone().expect("clap: --all requires --provider");
    if !accounts.iter().any(|a| a.provider == p) {
      bail!("no accounts for provider '{p}'");
    }
    let ids: Vec<String> = accounts
      .iter()
      .filter(|a| a.provider == p && (args.include_disabled || a.enabled))
      .map(|a| a.id.clone())
      .collect();
    (ids, vec![p])
  } else {
    let ids = args.account_multi.clone();
    let mut providers = Vec::new();
    for id in &ids {
      let p = lookup_provider(accounts, id)?;
      if !providers.contains(&p) {
        providers.push(p);
      }
    }
    (ids, providers)
  };

  let active_set: std::collections::HashSet<&str> = active_ids.iter().map(String::as_str).collect();

  let mut changes = Vec::new();
  for a in accounts.iter_mut() {
    if !providers.contains(&a.provider) {
      continue;
    }
    let want_active = active_set.contains(a.id.as_str());
    // Disabled accounts only flip if --include-disabled was passed (or
    // they're explicitly named via --account / --only).
    let touches_disabled = !a.enabled && (args.include_disabled || want_active);
    if !a.enabled && !touches_disabled {
      continue;
    }
    let old = a.state();
    let (new_enabled, new_tier) = if want_active {
      (true, AccountTier::Active)
    } else {
      // Demote to Fallback if we're modifying actives in this provider; but
      // for `--all` the expected behaviour is "everyone in provider becomes
      // Active" — so non-named accounts are simply unchanged.
      if args.all {
        continue;
      }
      (true, AccountTier::Fallback)
    };
    let new = if !new_enabled {
      AccountState::Disabled
    } else {
      match new_tier {
        AccountTier::Active => AccountState::Active,
        AccountTier::Fallback => AccountState::Fallback,
      }
    };
    if old == new && a.enabled == new_enabled {
      continue;
    }
    a.enabled = new_enabled;
    a.tier = new_tier;
    changes.push(SwitchChange {
      id: a.id.clone(),
      provider: a.provider.clone(),
      old,
      new,
    });
  }
  Ok(changes)
}

fn lookup_provider(accounts: &[Account], id: &str) -> Result<String> {
  accounts
    .iter()
    .find(|a| a.id == id)
    .map(|a| a.provider.clone())
    .ok_or_else(|| anyhow!("no account with id '{id}'"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use llm_core::account::AccountTier;

  #[test]
  fn fmt_int_groups_thousands() {
    assert_eq!(fmt_int(0), "0");
    assert_eq!(fmt_int(7), "7");
    assert_eq!(fmt_int(999), "999");
    assert_eq!(fmt_int(1_000), "1,000");
    assert_eq!(fmt_int(80_000_000), "80,000,000");
    assert_eq!(fmt_int(6_000_000), "6,000,000");
  }

  fn acct(id: &str, provider: &str, enabled: bool, tier: AccountTier) -> Account {
    Account {
      id: id.into(),
      provider: provider.into(),
      enabled,
      tier,
      label: None,
      tags: vec![],
      base_url: None,
      headers: std::collections::BTreeMap::new(),
      auth_type: None,
      username: None,
      api_key: None,
      api_key_expires_at: None,
      access_token: None,
      access_token_expires_at: None,
      id_token: None,
      refresh_token: None,
      extra: std::collections::BTreeMap::new(),
      refresh_url: None,
      last_refresh: None,
      settings: toml::Table::new(),
    }
  }

  fn switch_args(only: Option<&str>, all: bool, provider: Option<&str>, accts: &[&str], include_disabled: bool) -> SwitchArgs {
    SwitchArgs {
      only: only.map(String::from),
      all,
      provider: provider.map(String::from),
      account_multi: accts.iter().map(|s| s.to_string()).collect(),
      include_disabled,
    }
  }

  #[test]
  fn switch_only_promotes_named_demotes_others_in_same_provider() {
    let mut accts = vec![
      acct("a1", "p1", true, AccountTier::Active),
      acct("a2", "p1", true, AccountTier::Active),
      acct("b1", "p2", true, AccountTier::Active), // untouched (different provider)
    ];
    let changes = apply_switch(&mut accts, &switch_args(Some("a2"), false, None, &[], false)).unwrap();
    // a1: Active→Fallback; a2: already Active→no change; b1: untouched.
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].id, "a1");
    assert_eq!(changes[0].new, AccountState::Fallback);
    assert_eq!(accts[0].tier, AccountTier::Fallback);
    assert_eq!(accts[1].tier, AccountTier::Active);
    assert_eq!(accts[2].tier, AccountTier::Active);
  }

  #[test]
  fn switch_all_marks_every_enabled_account_in_provider_active() {
    let mut accts = vec![
      acct("a1", "p1", true, AccountTier::Fallback),
      acct("a2", "p1", true, AccountTier::Fallback),
      acct("a3", "p1", false, AccountTier::Fallback), // disabled, skipped
    ];
    let changes = apply_switch(&mut accts, &switch_args(None, true, Some("p1"), &[], false)).unwrap();
    assert_eq!(changes.len(), 2);
    assert!(accts[0].tier == AccountTier::Active);
    assert!(accts[1].tier == AccountTier::Active);
    assert!(!accts[2].enabled); // unchanged
  }

  #[test]
  fn switch_account_repeatable_promotes_listed_demotes_rest() {
    let mut accts = vec![
      acct("a1", "p1", true, AccountTier::Active),
      acct("a2", "p1", true, AccountTier::Active),
      acct("a3", "p1", true, AccountTier::Fallback),
    ];
    apply_switch(&mut accts, &switch_args(None, false, None, &["a1", "a3"], false)).unwrap();
    assert_eq!(accts[0].tier, AccountTier::Active);
    assert_eq!(accts[1].tier, AccountTier::Fallback);
    assert_eq!(accts[2].tier, AccountTier::Active);
  }

  #[test]
  fn switch_rejects_zero_or_multiple_modes() {
    let mut accts = vec![acct("a1", "p1", true, AccountTier::Active)];
    assert!(apply_switch(&mut accts, &switch_args(None, false, None, &[], false)).is_err());
    assert!(apply_switch(&mut accts, &switch_args(Some("a1"), true, Some("p1"), &[], false)).is_err());
  }

  #[test]
  fn switch_unknown_id_errors() {
    let mut accts = vec![acct("a1", "p1", true, AccountTier::Active)];
    assert!(apply_switch(&mut accts, &switch_args(Some("ghost"), false, None, &[], false)).is_err());
  }
}
