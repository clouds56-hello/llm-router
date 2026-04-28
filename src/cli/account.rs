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
        AccountCmd::List(args) => list(&cfg, args).await?,
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

async fn list(cfg: &Config, args: ListArgs) -> Result<()> {
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
    /// Premium-interactions budget remaining for the month.
    /// Upstream sends per-feature buckets; we surface the most informative
    /// non-null one in `headline`, plus the reset date when available.
    headline: Option<(String, u64)>, // (feature label, remaining)
    reset_date: Option<String>,      // ISO YYYY-MM-DD
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
        let fut = async move {
            crate::provider::github_copilot::token::exchange(&http, &gh, &headers).await
        };
        return match tokio::time::timeout(timeout, fut).await {
            Err(_) => QuotaResult::Err("timeout".into()),
            Ok(Err(e)) => QuotaResult::Err(short_err(&e)),
            Ok(Ok(resp)) => {
                let q = resp.limited_user_quotas.as_ref();
                let headline = q.and_then(|q| {
                    q.premium_interactions
                        .map(|n| ("premium_interactions".to_string(), n))
                        .or_else(|| q.chat.map(|n| ("chat".to_string(), n)))
                        .or_else(|| q.completions.map(|n| ("completions".to_string(), n)))
                });
                if headline.is_none() && resp.limited_user_reset_date.is_none() {
                    QuotaResult::None
                } else {
                    QuotaResult::Copilot(CopilotMonthly {
                        headline,
                        reset_date: resp.limited_user_reset_date,
                    })
                }
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
        QuotaResult::Copilot(c) => render_copilot(c),
        QuotaResult::Zai(z) => render_zai(z),
    }
}

fn render_copilot(c: &CopilotMonthly) {
    // Copilot's premium-request budget is a *monthly* counter; we display
    // remaining count + reset date. Per-feature buckets sometimes diverge,
    // so we surface the most relevant headline.
    let reset = c
        .reset_date
        .as_deref()
        .map(|d| format!(" — resets {d}"))
        .unwrap_or_default();
    match &c.headline {
        Some((label, n)) => {
            println!("  copilot     : {n} {label} remaining (monthly){reset}");
        }
        None => {
            // Unlimited / org plan: we got a reset date but no remaining.
            println!("  copilot     : monthly quota{reset}");
        }
    }
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
