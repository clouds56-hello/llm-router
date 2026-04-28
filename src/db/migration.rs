use rusqlite::Connection;
use std::path::{Path, PathBuf};

use super::Result;

pub fn migrate_legacy_usage(data_dir: &Path, usage_db: &Path) -> Result<()> {
  if usage_db.exists() {
    return Ok(());
  }
  let legacy = data_dir.join("usage.sqlite");
  if !legacy.exists() {
    return Ok(());
  }
  if let Some(parent) = usage_db.parent() {
    std::fs::create_dir_all(parent)?;
  }
  std::fs::copy(&legacy, usage_db)?;
  let conn = Connection::open(usage_db)?;
  crate::db::usage::add_column_if_missing(&conn, "provider_id", "TEXT NOT NULL DEFAULT ''")?;
  crate::db::usage::add_column_if_missing(&conn, "initiator", "TEXT NOT NULL DEFAULT 'user'")?;
  conn.pragma_update(None, "user_version", 2)?;
  let backup = backup_path(&legacy);
  std::fs::rename(&legacy, backup)?;
  Ok(())
}

fn backup_path(path: &Path) -> PathBuf {
  let mut i = 0;
  loop {
    let candidate = if i == 0 {
      path.with_extension("sqlite.bak")
    } else {
      path.with_extension(format!("sqlite.bak.{i}"))
    };
    if !candidate.exists() {
      return candidate;
    }
    i += 1;
  }
}
