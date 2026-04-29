//! `llm-router config` subcommand — git-style key/value access plus profile
//! helpers. Comment-preserving edits via `toml_edit`.

use crate::config::{paths, Config};
use crate::provider::profiles::{self, Profiles};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Subcommand};
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
  Profiles::parse(&raw).context("validation failed: edited profiles.toml is invalid")?;
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
}
