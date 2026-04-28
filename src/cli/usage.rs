use crate::config::{paths, Config};
use crate::db::UsageDb;
use anyhow::Result;
use clap::Args;
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Args, Debug)]
pub struct UsageArgs {
  /// Time window, e.g. "24h", "7d", "30m". Default 24h.
  #[arg(long, default_value = "24h")]
  pub since: String,

  /// Filter by account id.
  #[arg(long)]
  pub account: Option<String>,

  /// Filter by provider id.
  #[arg(long)]
  pub provider: Option<String>,
}

pub async fn run(cfg_path: Option<PathBuf>, args: UsageArgs) -> Result<()> {
  let (cfg, _) = Config::load(cfg_path.as_deref())?;
  let path = cfg
    .db
    .usage_db_path
    .clone()
    .map(Ok)
    .unwrap_or_else(paths::default_usage_db)?;
  let conn = Connection::open(&path)?;
  let db = UsageDb::open(conn)?;

  let since: Duration = humantime::parse_duration(&args.since)?;
  let since_ts = time::OffsetDateTime::now_utc().unix_timestamp() - since.as_secs() as i64;

  let rows = db.summary(since_ts, args.account.as_deref(), args.provider.as_deref())?;
  if rows.is_empty() {
    println!("(no requests in window)");
    return Ok(());
  }
  println!(
    "{:<16}  {:<18}  {:<24}  {:<7}  {:>6}  {:>10}  {:>12}  {:>10}",
    "account", "provider", "model", "init", "calls", "prompt_tok", "completion_tok", "avg_ms"
  );
  for r in rows {
    println!(
      "{:<16}  {:<18}  {:<24}  {:<7}  {:>6}  {:>10}  {:>12}  {:>10.0}",
      r.account, r.provider, r.model, r.initiator, r.count, r.prompt_tokens, r.completion_tokens, r.avg_latency_ms
    );
  }
  Ok(())
}
