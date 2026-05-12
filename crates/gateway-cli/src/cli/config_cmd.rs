//! `llm-router config` subcommand — git-style key/value access plus profile
//! helpers. Comment-preserving edits via `toml_edit`.

use crate::config::{paths, Config};
use crate::provider::profiles::{self, Profiles};
use crate::util::http::build_client;
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
use inquire::{Confirm, Select, Text};
use llm_auth::AuthStore;
use llm_config::RouteMode;
use std::path::PathBuf;
use toml_edit::{value, Array, DocumentMut, Item, Table, Value as EditValue};

#[derive(Args, Debug)]
pub struct ConfigArgs {
  #[command(subcommand)]
  pub cmd: ConfigCmd,
}

#[derive(Subcommand, Debug)]
pub enum ConfigCmd {
  /// Print the value of a key (e.g. `copilot.user_agent`)
  Get(GetArgs),
  /// Set a key (e.g. `copilot.user_agent "vscode/1.95.0"`)
  Set(SetArgs),
  /// Remove a key
  Unset(UnsetArgs),
  /// Print effective config as TOML
  List,
  /// Open the config file in $EDITOR; validates after save
  Edit,
  /// Open the user profiles file in $EDITOR; validates after save
  EditProfiles,
  /// Print the path to the config file (or `--profiles` for profiles.toml)
  Path {
    #[arg(long)]
    profiles: bool,
  },
  /// List known persona profiles and verified status
  ListProfiles,
  /// Initialize config with onboarding wizard
  Init(InitArgs),
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
pub enum RouteModeArg {
  Passthrough,
  Exact,
  Route,
  Fuzzy,
}

impl From<RouteModeArg> for RouteMode {
  fn from(value: RouteModeArg) -> Self {
    match value {
      RouteModeArg::Passthrough => RouteMode::Passthrough,
      RouteModeArg::Exact => RouteMode::Exact,
      RouteModeArg::Route => RouteMode::Route,
      RouteModeArg::Fuzzy => RouteMode::Fuzzy,
    }
  }
}

#[derive(Args, Debug)]
pub struct InitArgs {
  /// Non-interactive mode.
  #[arg(long)]
  pub yes: bool,
  /// Runtime route mode override.
  #[arg(long, value_enum)]
  pub route_mode: Option<RouteModeArg>,
  /// Runtime serve host override.
  #[arg(long)]
  pub host: Option<String>,
  /// Runtime serve port override.
  #[arg(long)]
  pub port: Option<u16>,
  /// Runtime proxy host override.
  #[arg(long)]
  pub proxy_host: Option<String>,
  /// Runtime proxy port override.
  #[arg(long)]
  pub proxy_port: Option<u16>,
  /// Runtime proxy default route mode override.
  #[arg(long, value_enum)]
  pub proxy_route_mode: Option<RouteModeArg>,
  /// Non-interactive repeatable account specs:
  /// id=...,provider=...,from=...[,env_var=...]
  #[arg(long = "account")]
  pub accounts: Vec<String>,
}

#[derive(Args, Debug)]
pub struct GetArgs {
  pub key: String,
  /// Operate inside the [accounts.<id>] subtree
  #[arg(long)]
  pub account: Option<String>,
}

#[derive(Args, Debug)]
pub struct SetArgs {
  pub key: String,
  pub value: String,
  /// Append to an array instead of replacing
  #[arg(long)]
  pub add: bool,
  /// Operate inside the [accounts.<id>] subtree
  #[arg(long)]
  pub account: Option<String>,
}

#[derive(Args, Debug)]
pub struct UnsetArgs {
  pub key: String,
  /// Operate inside the [accounts.<id>] subtree
  #[arg(long)]
  pub account: Option<String>,
}

pub async fn run(cfg_path: Option<PathBuf>, args: ConfigArgs) -> Result<()> {
  let path = match cfg_path {
    Some(p) => p,
    None => paths::config_path()?,
  };

  match args.cmd {
    ConfigCmd::Get(a) => cmd_get(&path, a),
    ConfigCmd::Set(a) => cmd_set(&path, a),
    ConfigCmd::Unset(a) => cmd_unset(&path, a),
    ConfigCmd::List => cmd_list(&path),
    ConfigCmd::Edit => cmd_edit(&path),
    ConfigCmd::EditProfiles => cmd_edit_profiles(),
    ConfigCmd::Path { profiles } => cmd_path(&path, profiles),
    ConfigCmd::ListProfiles => cmd_list_profiles(),
    ConfigCmd::Init(a) => cmd_init(&path, a).await,
  }
}

// --- get ---------------------------------------------------------------

fn cmd_get(path: &std::path::Path, args: GetArgs) -> Result<()> {
  let doc = load_doc(path)?;
  let segments = key_segments(args.account.as_deref(), &args.key);
  match lookup(&doc, &segments) {
    Some(item) => {
      print!("{}", render_item(item));
      if !render_item(item).ends_with('\n') {
        println!();
      }
      Ok(())
    }
    None => Err(anyhow!("key not found: {}", args.key)),
  }
}

fn render_item(item: &Item) -> String {
  match item {
    Item::Value(v) => match v {
      EditValue::String(s) => s.value().to_string(),
      EditValue::Integer(i) => i.value().to_string(),
      EditValue::Float(f) => f.value().to_string(),
      EditValue::Boolean(b) => b.value().to_string(),
      EditValue::Datetime(d) => d.value().to_string(),
      EditValue::Array(a) => a.to_string(),
      EditValue::InlineTable(t) => t.to_string(),
    },
    Item::Table(t) => t.to_string(),
    Item::ArrayOfTables(a) => format!("{} table(s)", a.len()),
    Item::None => String::new(),
  }
}

// --- set ---------------------------------------------------------------

fn cmd_set(path: &std::path::Path, args: SetArgs) -> Result<()> {
  Config::edit_in_place(path, |doc| {
    let segments = key_segments(args.account.as_deref(), &args.key);
    if args.add {
      append_array(doc, &segments, &args.value)?;
    } else {
      let existing = lookup(doc, &segments).cloned();
      let new = coerce(&args.value, existing.as_ref());
      insert(doc, &segments, new)?;
    }
    Ok(())
  })?;
  tracing::info!(key = %args.key, account = ?args.account, add = args.add, "config set");
  println!("set {}", args.key);
  Ok(())
}

fn coerce(raw: &str, prior: Option<&Item>) -> Item {
  // Honour the existing type if present.
  if let Some(Item::Value(v)) = prior {
    match v {
      EditValue::Boolean(_) => {
        if let Ok(b) = raw.parse::<bool>() {
          return value(b);
        }
      }
      EditValue::Integer(_) => {
        if let Ok(n) = raw.parse::<i64>() {
          return value(n);
        }
      }
      EditValue::Float(_) => {
        if let Ok(n) = raw.parse::<f64>() {
          return value(n);
        }
      }
      EditValue::Array(_) => {
        let arr: Array = raw.split(',').map(|s| s.trim().to_string()).collect();
        return value(arr);
      }
      _ => {}
    }
  }
  // Heuristic fallback
  if let Ok(b) = raw.parse::<bool>() {
    return value(b);
  }
  if let Ok(n) = raw.parse::<i64>() {
    return value(n);
  }
  value(raw)
}

fn append_array(doc: &mut DocumentMut, segments: &[String], raw: &str) -> Result<()> {
  let existing = lookup(doc, segments).cloned();
  let mut arr = match existing {
    Some(Item::Value(EditValue::Array(a))) => a,
    Some(_) => bail!("--add: existing value is not an array"),
    None => Array::new(),
  };
  arr.push(raw);
  insert(doc, segments, value(arr))
}

// --- unset -------------------------------------------------------------

fn cmd_unset(path: &std::path::Path, args: UnsetArgs) -> Result<()> {
  Config::edit_in_place(path, |doc| {
    let segments = key_segments(args.account.as_deref(), &args.key);
    if !remove(doc, &segments) {
      return Err(anyhow::anyhow!("key not found: {}", args.key).into());
    }
    Ok(())
  })?;
  tracing::info!(key = %args.key, account = ?args.account, "config unset");
  println!("unset {}", args.key);
  Ok(())
}

// --- list / edit / path ------------------------------------------------

fn cmd_list(path: &std::path::Path) -> Result<()> {
  let (cfg, _) = Config::load(Some(path))?;
  let s = toml::to_string_pretty(&cfg)?;
  print!("{s}");
  Ok(())
}

fn cmd_edit(path: &std::path::Path) -> Result<()> {
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).ok();
  }
  open_in_editor(path)?;
  // Validate
  let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
  let _: DocumentMut = raw.parse().context("edited file is not valid TOML")?;
  let (_cfg, _) = Config::load(Some(path)).context("validation failed after edit")?;
  println!("ok");
  Ok(())
}

fn cmd_edit_profiles() -> Result<()> {
  let path = profiles::user_profiles_path().ok_or_else(|| anyhow!("could not resolve user profiles path"))?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).ok();
  }
  if !path.exists() {
    std::fs::write(
      &path,
      b"# User-defined personas. See built-in profiles.toml for schema.\n",
    )?;
  }
  open_in_editor(&path)?;
  let raw = std::fs::read_to_string(&path)?;
  Profiles::parse(&raw).map_err(|e| anyhow!("validation failed: edited profiles.toml is invalid: {e}"))?;
  println!("ok");
  Ok(())
}

fn cmd_path(path: &std::path::Path, want_profiles: bool) -> Result<()> {
  if want_profiles {
    let p = profiles::user_profiles_path().ok_or_else(|| anyhow!("could not resolve user profiles path"))?;
    println!("{}", p.display());
  } else {
    println!("{}", path.display());
  }
  Ok(())
}

fn cmd_list_profiles() -> Result<()> {
  let p = Profiles::global();
  for (name, verified) in p.personas() {
    let tag = if verified { "verified" } else { "UNVERIFIED" };
    println!("{name:<16}  {tag}");
  }
  Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct AccountSpec {
  id: String,
  provider: String,
  from: String,
  env_var: Option<String>,
  refresh_token: Option<String>,
  refresh_token_env_var: Option<String>,
}

async fn cmd_init(path: &std::path::Path, args: InitArgs) -> Result<()> {
  let (mut cfg, _) = Config::load(Some(path))?;
  println!("Config path: {}", path.display());

  apply_runtime_overrides(&mut cfg, &args);

  if args.yes {
    if args.accounts.is_empty() {
      bail!("--yes requires at least one --account spec");
    }
    let mut store = AuthStore::load(None, Some(path))?;
    let client = build_client(&cfg.proxy)?;
    for raw in &args.accounts {
      let spec = parse_account_spec(raw)?;
      let source = account_source_from_spec(&spec, false)?;
      let account =
        crate::cli::onboarding::resolve_account(&client, &spec.provider, Some(spec.id.clone()), source).await?;
      store.upsert(account);
    }
    cfg.save(path)?;
    store.save()?;
    println!("Initialized config and upserted {} account(s).", args.accounts.len());
    return Ok(());
  }

  interactive_runtime_prompts(&mut cfg)?;
  let mut store = AuthStore::load(None, Some(path))?;
  let client = build_client(&cfg.proxy)?;
  let mut upserted = 0usize;
  loop {
    let account = crate::cli::onboarding::interactive_add_account(&client, None, None).await?;
    store.upsert(account);
    upserted += 1;
    let more = Confirm::new("Add another account?")
      .with_default(false)
      .prompt()
      .context("account loop cancelled")?;
    if !more {
      break;
    }
  }

  cfg.save(path)?;
  store.save()?;
  println!("Initialized config and upserted {upserted} account(s).");
  println!("Next: llm-router serve  # or llm-router proxy start");
  Ok(())
}

fn apply_runtime_overrides(cfg: &mut Config, args: &InitArgs) {
  if let Some(mode) = args.route_mode {
    cfg.server.route_mode = mode.into();
  }
  if let Some(host) = &args.host {
    cfg.server.host = host.clone();
  }
  if let Some(port) = args.port {
    cfg.server.port = port;
  }
  if let Some(host) = &args.proxy_host {
    cfg.proxy_mode.host = host.clone();
  }
  if let Some(port) = args.proxy_port {
    cfg.proxy_mode.port = port;
  }
  if let Some(mode) = args.proxy_route_mode {
    cfg.proxy_mode.route_mode = mode.into();
  }
}

fn interactive_runtime_prompts(cfg: &mut Config) -> Result<()> {
  let route_options = vec!["route", "passthrough", "exact", "fuzzy"];
  let default_idx = match cfg.server.route_mode {
    RouteMode::Route => 0,
    RouteMode::Passthrough => 1,
    RouteMode::Exact => 2,
    RouteMode::Fuzzy => 3,
  };
  let selected = Select::new("Route mode:", route_options.clone())
    .with_starting_cursor(default_idx)
    .prompt()
    .context("route mode selection cancelled")?;
  cfg.server.route_mode = match selected {
    "route" => RouteMode::Route,
    "passthrough" => RouteMode::Passthrough,
    "exact" => RouteMode::Exact,
    "fuzzy" => RouteMode::Fuzzy,
    _ => RouteMode::Route,
  };

  let proxy_default_idx = match cfg.proxy_mode.route_mode {
    RouteMode::Route => 0,
    RouteMode::Passthrough => 1,
    RouteMode::Exact => 2,
    RouteMode::Fuzzy => 3,
  };
  let proxy_selected = Select::new("Proxy route mode:", route_options)
    .with_starting_cursor(proxy_default_idx)
    .prompt()
    .context("proxy route mode selection cancelled")?;
  cfg.proxy_mode.route_mode = match proxy_selected {
    "route" => RouteMode::Route,
    "passthrough" => RouteMode::Passthrough,
    "exact" => RouteMode::Exact,
    "fuzzy" => RouteMode::Fuzzy,
    _ => RouteMode::Route,
  };

  if Confirm::new("Set serve host/port?")
    .with_default(false)
    .prompt()
    .context("serve host/port prompt cancelled")?
  {
    let host = Text::new("Serve host:")
      .with_initial_value(&cfg.server.host)
      .prompt()
      .context("serve host prompt cancelled")?;
    let port = Text::new("Serve port:")
      .with_initial_value(&cfg.server.port.to_string())
      .prompt()
      .context("serve port prompt cancelled")?;
    cfg.server.host = host;
    cfg.server.port = port.parse().context("serve port must be a valid u16")?;
  }

  if Confirm::new("Set proxy host/port?")
    .with_default(false)
    .prompt()
    .context("proxy host/port prompt cancelled")?
  {
    let host = Text::new("Proxy host:")
      .with_initial_value(&cfg.proxy_mode.host)
      .prompt()
      .context("proxy host prompt cancelled")?;
    let port = Text::new("Proxy port:")
      .with_initial_value(&cfg.proxy_mode.port.to_string())
      .prompt()
      .context("proxy port prompt cancelled")?;
    cfg.proxy_mode.host = host;
    cfg.proxy_mode.port = port.parse().context("proxy port must be a valid u16")?;
  }
  Ok(())
}

fn parse_account_spec(raw: &str) -> Result<AccountSpec> {
  let mut id: Option<String> = None;
  let mut provider: Option<String> = None;
  let mut from: Option<String> = None;
  let mut env_var: Option<String> = None;
  let mut refresh_token: Option<String> = None;
  let mut refresh_token_env_var: Option<String> = None;

  for part in raw.split(',') {
    let (k, v) = part
      .split_once('=')
      .ok_or_else(|| anyhow!("invalid account spec segment '{part}', expected key=value"))?;
    let key = k.trim();
    let val = v.trim();
    if val.is_empty() {
      bail!("account spec key '{key}' cannot be empty");
    }
    match key {
      "id" => id = Some(val.to_string()),
      "provider" => provider = Some(val.to_string()),
      "from" => from = Some(val.to_string()),
      "env_var" => env_var = Some(val.to_string()),
      "refresh_token" => refresh_token = Some(val.to_string()),
      "refresh_token_env_var" => refresh_token_env_var = Some(val.to_string()),
      _ => bail!("unknown account spec key '{key}'"),
    }
  }

  let spec = AccountSpec {
    id: id.ok_or_else(|| anyhow!("account spec missing required key 'id'"))?,
    provider: provider.ok_or_else(|| anyhow!("account spec missing required key 'provider'"))?,
    from: from.ok_or_else(|| anyhow!("account spec missing required key 'from'"))?,
    env_var,
    refresh_token,
    refresh_token_env_var,
  };
  crate::cli::onboarding::validate_provider(&spec.provider)?;
  Ok(spec)
}

fn account_source_from_spec(spec: &AccountSpec, allow_login: bool) -> Result<crate::cli::onboarding::CredentialSource> {
  let source = match spec.from.as_str() {
    "login" => {
      if !allow_login {
        bail!("from=login is interactive-only; use env/gh/copilot-plugin/refresh-token in --yes mode");
      }
      crate::cli::onboarding::CredentialSource::Login
    }
    "gh" => crate::cli::onboarding::CredentialSource::Gh,
    "copilot-plugin" => crate::cli::onboarding::CredentialSource::CopilotPlugin,
    "refresh-token" => {
      let token = if let Some(t) = spec.refresh_token.clone() {
        let trimmed = t.trim().to_string();
        if trimmed.is_empty() {
          bail!("refresh_token cannot be empty");
        }
        trimmed
      } else {
        let env_name = spec
          .refresh_token_env_var
          .clone()
          .unwrap_or_else(|| "GITHUB_COPILOT_REFRESH_TOKEN".to_string());
        let value = std::env::var(&env_name)
          .map_err(|_| anyhow!("environment variable `{env_name}` is not set; set it or pass refresh_token=..."))?;
        let trimmed = value.trim().to_string();
        if trimmed.is_empty() {
          bail!("environment variable `{env_name}` is empty");
        }
        trimmed
      };
      crate::cli::onboarding::CredentialSource::RefreshToken { token }
    }
    "env" => crate::cli::onboarding::CredentialSource::Env {
      env_var: spec.env_var.clone().unwrap_or_else(|| "ZAI_API_KEY".to_string()),
    },
    other => bail!("unsupported from='{other}'; expected login|gh|copilot-plugin|refresh-token|env"),
  };
  crate::cli::onboarding::validate_provider_source(&spec.provider, &source)?;
  Ok(source)
}

fn open_in_editor(path: &std::path::Path) -> Result<()> {
  let editor = std::env::var("VISUAL")
    .or_else(|_| std::env::var("EDITOR"))
    .unwrap_or_else(|_| "vi".into());
  let status = std::process::Command::new(&editor)
    .arg(path)
    .status()
    .with_context(|| format!("spawn editor `{editor}`"))?;
  if !status.success() {
    bail!("editor exited with status {status}");
  }
  Ok(())
}

// --- key plumbing ------------------------------------------------------

fn key_segments(account: Option<&str>, key: &str) -> Vec<String> {
  let mut out = Vec::new();
  if let Some(id) = account {
    out.push("accounts".into());
    out.push(id.into());
  }
  for s in key.split('.') {
    out.push(s.to_string());
  }
  out
}

fn lookup<'a>(doc: &'a DocumentMut, segments: &[String]) -> Option<&'a Item> {
  if segments.is_empty() {
    return None;
  }
  // Special-case [[accounts]] array-of-tables when first two segments are
  // "accounts" "<id>".
  if segments.len() >= 2 && segments[0] == "accounts" {
    let arr = doc.get("accounts")?.as_array_of_tables()?;
    let entry = arr
      .iter()
      .find(|t| t.get("id").and_then(|i| i.as_str()) == Some(segments[1].as_str()))?;
    return descend_table(entry, &segments[2..]);
  }
  let first = doc.get(&segments[0])?;
  descend_item(first, &segments[1..])
}

fn descend_item<'a>(item: &'a Item, rest: &[String]) -> Option<&'a Item> {
  if rest.is_empty() {
    return Some(item);
  }
  match item {
    Item::Table(t) => descend_table(t, rest),
    Item::Value(EditValue::InlineTable(t)) => {
      // Convert path manually
      let next = t.get(&rest[0])?;
      // Wrap as Item temporarily — but lifetime hard; just stop traversal here.
      if rest.len() == 1 {
        // We can't return &Item from &Value cheaply; build a borrowed-shaped thing.
        // Safe shortcut: only support flat lookup inside inline tables via map_value path.
        return inline_value_as_item(next);
      }
      None
    }
    _ => None,
  }
}

fn inline_value_as_item(_v: &EditValue) -> Option<&Item> {
  // toml_edit doesn't let us cheaply borrow Item from inside a Value; for
  // CLI purposes we don't expect deep inline-table reads, so we return None
  // and let the caller report "key not found".
  None
}

fn descend_table<'a>(t: &'a Table, rest: &[String]) -> Option<&'a Item> {
  if rest.is_empty() {
    // Need an Item; manufacture by going through the underlying entry. We
    // can't, so just return None; callers handle "table" path via table
    // reference upstream. For our needs, only leaf reads matter.
    return None;
  }
  let item = t.get(&rest[0])?;
  descend_item(item, &rest[1..])
}

fn insert(doc: &mut DocumentMut, segments: &[String], new: Item) -> Result<()> {
  if segments.is_empty() {
    bail!("empty key");
  }
  if segments.len() >= 2 && segments[0] == "accounts" {
    let entry = ensure_account(doc, &segments[1])?;
    return insert_into_table(entry, &segments[2..], new);
  }
  if segments.len() == 1 {
    doc.insert(&segments[0], new);
    return Ok(());
  }
  let head = &segments[0];
  if doc.get(head).is_none() {
    doc.insert(head, Item::Table(Table::new()));
  }
  let item = doc.get_mut(head).unwrap();
  let table = item.as_table_mut().ok_or_else(|| anyhow!("`{head}` is not a table"))?;
  insert_into_table(table, &segments[1..], new)
}

fn insert_into_table(t: &mut Table, segments: &[String], new: Item) -> Result<()> {
  if segments.is_empty() {
    bail!("empty key");
  }
  if segments.len() == 1 {
    t.insert(&segments[0], new);
    return Ok(());
  }
  let head = &segments[0];
  if t.get(head).is_none() {
    t.insert(head, Item::Table(Table::new()));
  }
  let next = t
    .get_mut(head)
    .and_then(|i| i.as_table_mut())
    .ok_or_else(|| anyhow!("`{head}` is not a table"))?;
  insert_into_table(next, &segments[1..], new)
}

fn remove(doc: &mut DocumentMut, segments: &[String]) -> bool {
  if segments.is_empty() {
    return false;
  }
  if segments.len() >= 2 && segments[0] == "accounts" {
    let Some(arr) = doc.get_mut("accounts").and_then(|i| i.as_array_of_tables_mut()) else {
      return false;
    };
    let Some(entry) = arr
      .iter_mut()
      .find(|t| t.get("id").and_then(|i| i.as_str()) == Some(segments[1].as_str()))
    else {
      return false;
    };
    return remove_from_table(entry, &segments[2..]);
  }
  if segments.len() == 1 {
    return doc.remove(&segments[0]).is_some();
  }
  let Some(item) = doc.get_mut(&segments[0]) else {
    return false;
  };
  let Some(t) = item.as_table_mut() else {
    return false;
  };
  remove_from_table(t, &segments[1..])
}

fn remove_from_table(t: &mut Table, segments: &[String]) -> bool {
  if segments.is_empty() {
    return false;
  }
  if segments.len() == 1 {
    return t.remove(&segments[0]).is_some();
  }
  let Some(item) = t.get_mut(&segments[0]) else {
    return false;
  };
  let Some(inner) = item.as_table_mut() else {
    return false;
  };
  remove_from_table(inner, &segments[1..])
}

fn ensure_account<'a>(doc: &'a mut DocumentMut, id: &str) -> Result<&'a mut Table> {
  if doc.get("accounts").is_none() {
    doc.insert("accounts", Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
  }
  let arr = doc
    .get_mut("accounts")
    .and_then(|i| i.as_array_of_tables_mut())
    .ok_or_else(|| anyhow!("`accounts` is not an array of tables"))?;
  let pos = arr
    .iter()
    .position(|t| t.get("id").and_then(|i| i.as_str()) == Some(id));
  if pos.is_none() {
    let mut t = Table::new();
    t.insert("id", value(id));
    arr.push(t);
  }
  let idx = arr
    .iter()
    .position(|t| t.get("id").and_then(|i| i.as_str()) == Some(id))
    .unwrap();
  Ok(arr.get_mut(idx).unwrap())
}

fn load_doc(path: &std::path::Path) -> Result<DocumentMut> {
  if !path.exists() {
    return Ok(DocumentMut::new());
  }
  let raw = std::fs::read_to_string(path)?;
  raw.parse().context("invalid TOML")
}

#[cfg(test)]
mod tests {
  use super::*;

  fn doc(s: &str) -> DocumentMut {
    s.parse().unwrap()
  }

  #[test]
  fn insert_top_level() {
    let mut d = doc("");
    insert(&mut d, &["copilot".into(), "user_agent".into()], value("x")).unwrap();
    assert!(d.to_string().contains("user_agent = \"x\""));
  }

  #[test]
  fn insert_account_field() {
    let mut d = doc("[[accounts]]\nid = \"work\"\n");
    insert(
      &mut d,
      &["accounts".into(), "work".into(), "behave_as".into()],
      value("opencode"),
    )
    .unwrap();
    let s = d.to_string();
    assert!(s.contains("behave_as = \"opencode\""));
  }

  #[test]
  fn remove_top_level() {
    let mut d = doc("[copilot]\nuser_agent = \"x\"\n");
    assert!(remove(&mut d, &["copilot".into(), "user_agent".into()]));
    assert!(!d.to_string().contains("user_agent"));
  }

  #[test]
  fn coerce_keeps_existing_type() {
    let prior = value(true);
    let new = coerce("false", Some(&prior));
    assert!(matches!(new, Item::Value(EditValue::Boolean(_))));
  }

  #[test]
  fn parse_account_spec_happy_path() {
    let spec = parse_account_spec("id=work,provider=github-copilot,from=gh").unwrap();
    assert_eq!(spec.id, "work");
    assert_eq!(spec.provider, "github-copilot");
    assert_eq!(spec.from, "gh");
    assert_eq!(spec.env_var, None);
    assert_eq!(spec.refresh_token, None);
    assert_eq!(spec.refresh_token_env_var, None);
  }

  #[test]
  fn parse_account_spec_requires_id_provider_from() {
    let err = parse_account_spec("provider=github-copilot,from=gh")
      .unwrap_err()
      .to_string();
    assert!(err.contains("missing required key 'id'"));

    let err = parse_account_spec("id=work,from=gh").unwrap_err().to_string();
    assert!(err.contains("missing required key 'provider'"));

    let err = parse_account_spec("id=work,provider=github-copilot")
      .unwrap_err()
      .to_string();
    assert!(err.contains("missing required key 'from'"));
  }

  #[test]
  fn account_source_rejects_incompatible_provider_source() {
    let spec = AccountSpec {
      id: "cn".into(),
      provider: "zai".into(),
      from: "gh".into(),
      env_var: None,
      refresh_token: None,
      refresh_token_env_var: None,
    };
    let err = account_source_from_spec(&spec, false).unwrap_err().to_string();
    assert!(err.contains("static-API-key provider"));
  }

  #[test]
  fn account_source_rejects_login_in_non_interactive() {
    let spec = AccountSpec {
      id: "work".into(),
      provider: "github-copilot".into(),
      from: "login".into(),
      env_var: None,
      refresh_token: None,
      refresh_token_env_var: None,
    };
    let err = account_source_from_spec(&spec, false).unwrap_err().to_string();
    assert!(err.contains("interactive-only"));
  }

  #[test]
  fn account_source_accepts_refresh_token_literal() {
    let spec = AccountSpec {
      id: "work".into(),
      provider: "github-copilot".into(),
      from: "refresh-token".into(),
      env_var: None,
      refresh_token: Some("rtok".into()),
      refresh_token_env_var: None,
    };
    let source = account_source_from_spec(&spec, false).unwrap();
    assert!(matches!(
      source,
      crate::cli::onboarding::CredentialSource::RefreshToken { .. }
    ));
  }
}
