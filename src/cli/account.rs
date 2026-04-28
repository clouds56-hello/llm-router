use crate::config::{Account, Config};
use crate::provider::{ID_GITHUB_COPILOT, ZAI_ALIASES};
use crate::util::http::build_client;
use crate::util::timefmt::{relative_from_now, relative_from_now_ms};
use anyhow::{anyhow, Result};
use clap::Subcommand;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Subcommand, Debug)]
pub enum AccountCmd {
    /// List configured accounts
    List(ListArgs),
    /// Remove an account by id
    Remove { id: String },
    /// Show details for an account
    Show { id: String },
}

#[derive(clap::Args, Debug)]
pub struct ListArgs {
    /// Skip live upstream quota lookups (faster, no network).
    #[arg(long)]
    pub no_quota: bool,
    /// Per-upstream timeout in seconds for the live quota probe.
    #[arg(long, default_value_t = 5u64)]
    pub timeout: u64,
}

pub async fn run(cfg_path: Option<PathBuf>, cmd: AccountCmd) -> Result<()> {
    let (mut cfg, path) = Config::load(cfg_path.as_deref())?;
    match cmd {
        AccountCmd::List(args) => list(&mut cfg, &path, args).await?,
        AccountCmd::Remove { id } => {
            let before = cfg.accounts.len();
            cfg.accounts.retain(|a| a.id != id);
            if cfg.accounts.len() == before {
                return Err(anyhow!("no account with id '{id}'"));
            }
            cfg.save(&path)?;
            println!("Removed '{id}'");
        }
        AccountCmd::Show { id } => show(&cfg, &id)?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

async fn list(cfg: &mut Config, cfg_path: &std::path::Path, args: ListArgs) -> Result<()> {
    if cfg.accounts.is_empty() {
        println!("(no accounts)");
        return Ok(());
    }

    // Fetch quotas concurrently. Each future is wrapped in an outer timeout
    // so a single hung upstream cannot freeze the entire CLI invocation.
    let quotas: Vec<QuotaResult> = if args.no_quota {
        cfg.accounts.iter().map(|_| QuotaResult::Skipped).collect()
    } else {
        let http = build_client(&cfg.proxy)?;
        let timeout = Duration::from_secs(args.timeout.max(1));
        let futs = cfg
            .accounts
            .iter()
            .map(|a| fetch_quota(http.clone(), a.clone(), timeout));
        futures::future::join_all(futs).await
    };

    // Persist any refreshed Copilot api_tokens. The quota probe already calls
    // `token::exchange`, so we piggy-back on its result instead of issuing a
    // second request. This is a no-op under `--no-quota`.
    let mut dirty = false;
    for (a, q) in cfg.accounts.iter_mut().zip(quotas.iter()) {
        if let QuotaResult::Copilot(c) = q {
            if a.api_token.as_deref() != Some(c.fresh_token.as_str())
                || a.api_token_expires_at != Some(c.fresh_expires_at)
            {
                a.api_token = Some(c.fresh_token.clone());
                a.api_token_expires_at = Some(c.fresh_expires_at);
                dirty = true;
            }
        }
    }
    if dirty {
        cfg.save(cfg_path)?;
    }

    let mut first = true;
    for (a, q) in cfg.accounts.iter().zip(quotas.iter()) {
        if !first {
            println!();
        }
        first = false;
        render_account(a, q);
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
    /// Fresh short-lived Copilot api_token returned by the same exchange
    /// call that produced the quota. Persisted back to config so that the
    /// daemon (which never writes to disk at runtime) starts up with a
    /// non-expired cache.
    fresh_token: String,
    fresh_expires_at: i64,
}

async fn fetch_quota(
    http: reqwest::Client,
    account: Account,
    timeout: Duration,
) -> QuotaResult {
    if account.provider == ID_GITHUB_COPILOT {
        let Some(gh) = account.github_token.clone() else {
            return QuotaResult::None;
        };
        let headers = account.copilot.clone().unwrap_or_default();
        // Two parallel probes:
        //   1. token exchange — refreshes api_token (always needed on `list`)
        //   2. user-info     — quota_snapshots (Plus/Business plans)
        let http2 = http.clone();
        let gh2 = gh.clone();
        let h2 = headers.clone();
        let fut = async move {
            let (tok, info) = tokio::join!(
                crate::provider::github_copilot::token::exchange(&http, &gh, &headers),
                crate::provider::github_copilot::user::fetch(&http2, &gh2, &h2),
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
                let headline = info_res.as_ref().ok().and_then(|info| {
                    pick_headline(info)
                });
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

    if ZAI_ALIASES.contains(&account.provider.as_str()) {
        let Some(key) = account.api_key.clone() else {
            return QuotaResult::None;
        };
        let provider = account.provider.clone();
        let fut = async move {
            crate::provider::zai::quota::fetch(&http, &provider, &key).await
        };
        return match tokio::time::timeout(timeout, fut).await {
            Err(_) => QuotaResult::Err("timeout".into()),
            Ok(Err(e)) => QuotaResult::Err(short_err(&e)),
            Ok(Ok(q)) => QuotaResult::Zai(q),
        };
    }

    QuotaResult::None
}

fn short_err(e: &anyhow::Error) -> String {
    let s = e.to_string();
    if s.len() > 80 { format!("{}…", &s[..80]) } else { s }
}

fn render_account(a: &Account, q: &QuotaResult) {
    println!("{}  ({})", a.id, a.provider);

    let has = a.api_token.is_some() || a.api_key.is_some();
    println!("  credentials : {}", if has { "present" } else { "missing" });

    // Expiry: short-lived OAuth (api_token_expires_at) vs static api_key.
    match a.api_token_expires_at {
        Some(ts) => println!("  expires     : {} (api_token)", relative_from_now(ts)),
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
            println!(
                "  copilot     : {remaining} / {entitlement} {label} ({pct:.1}%){reset}"
            );
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
fn pick_headline(
    info: &crate::provider::github_copilot::user::CopilotUserInfo,
) -> Option<(String, u64, Option<u64>)> {
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
    let total = b
        .total
        .map(|t| format!(" of {}", fmt_int(t)))
        .unwrap_or_default();
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

fn show(cfg: &Config, id: &str) -> Result<()> {
    let a = cfg
        .accounts
        .iter()
        .find(|a| a.id == id)
        .ok_or_else(|| anyhow!("no account with id '{id}'"))?;
    println!("id: {}", a.id);
    println!("provider: {}", a.provider);
    if let Some(gh) = a.github_token.as_deref() {
        println!("github_token: {}…", &gh[..gh.len().min(7)]);
    }
    if let Some(k) = a.api_key.as_deref() {
        println!("api_key: {}", mask(k));
    }
    if a.api_token.is_some() || a.api_token_expires_at.is_some() {
        println!(
            "api_token: {}",
            a.api_token.as_deref().map(mask).unwrap_or("-".into())
        );
        match a.api_token_expires_at {
            Some(ts) => println!("api_token_expires_at: {ts} ({})", relative_from_now(ts)),
            None => println!("api_token_expires_at: -"),
        }
    }
    println!("override_headers: {}", a.copilot.is_some());
    if let Some(z) = &a.zai {
        if let Some(b) = &z.base_url {
            println!("zai.base_url: {b}");
        }
    }
    Ok(())
}

fn mask(s: &str) -> String {
    let n = s.len();
    if n <= 8 { return "***".into(); }
    format!("{}…{}", &s[..4], &s[n - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_int_groups_thousands() {
        assert_eq!(fmt_int(0), "0");
        assert_eq!(fmt_int(7), "7");
        assert_eq!(fmt_int(999), "999");
        assert_eq!(fmt_int(1_000), "1,000");
        assert_eq!(fmt_int(80_000_000), "80,000,000");
        assert_eq!(fmt_int(6_000_000), "6,000,000");
    }
}
