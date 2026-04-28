//! Catalogue source resolution.
//!
//! At process start we pick exactly one source and freeze it: either the
//! disk cache (if a previous `llm-router update` left one) or the snapshot
//! embedded at build time. There is no auto-refresh — the explicit `update`
//! subcommand is the only way to grow the cached copy.

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::Instant;

use super::schema::Catalogue;

/// Where the loaded catalogue came from. Surfaced by `update --status`.
#[derive(Debug, Clone)]
pub enum Source {
  /// Compile-time `include_bytes!` of `models.dev/api.json`.
  Embedded,
  /// On-disk JSON, written by `llm-router update`.
  DiskCache(PathBuf),
}

/// The embedded snapshot, baked in by `build.rs`.
const EMBEDDED: &[u8] = include_bytes!(env!("MODELS_DEV_SNAPSHOT_PATH"));

static GLOBAL: OnceLock<(Catalogue, Source)> = OnceLock::new();

/// Path of the on-disk catalogue cache, if we can determine an XDG cache dir.
pub fn cache_path() -> Option<PathBuf> {
  directories::ProjectDirs::from("", "", "llm-router").map(|d| d.cache_dir().join("catalogue.json"))
}

/// Borrow the global catalogue, loading it on first call.
///
/// Lookup order:
///   1. On-disk cache at [`cache_path`] — the result of a successful
///      `llm-router update`.
///   2. The embedded snapshot — always present, always parses.
///
/// If the disk cache exists but fails to parse we log a warning and fall
/// back to the embedded copy.
pub fn global() -> &'static Catalogue {
  &load_global().0
}

/// Same as [`global`] but also returns where the data came from.
pub fn global_with_source() -> &'static (Catalogue, Source) {
  load_global()
}

fn load_global() -> &'static (Catalogue, Source) {
  GLOBAL.get_or_init(|| match try_disk_cache() {
    Some((c, p)) => (c, Source::DiskCache(p)),
    None => (parse_embedded(), Source::Embedded),
  })
}

fn try_disk_cache() -> Option<(Catalogue, PathBuf)> {
  let path = cache_path()?;
  let bytes = std::fs::read(&path).ok()?;
  match serde_json::from_slice::<Catalogue>(&bytes) {
    Ok(c) => Some((c, path)),
    Err(e) => {
      tracing::warn!(
          cache = %path.display(),
          error = %e,
          "models.dev cache failed to parse; falling back to embedded snapshot"
      );
      None
    }
  }
}

fn parse_embedded() -> Catalogue {
  serde_json::from_slice(EMBEDDED).expect("embedded models.dev snapshot must parse — fix build.rs")
}

/// Outcome of a successful `llm-router update` run.
#[derive(Debug)]
pub struct UpdateReport {
  pub providers: usize,
  pub models: usize,
  pub bytes: u64,
  pub path: PathBuf,
  pub elapsed: std::time::Duration,
}

/// Fetch `url`, validate, atomically replace the on-disk cache.
///
/// The fetched bytes must:
///   * parse as our [`Catalogue`] schema, and
///   * contain at least one provider (defends against silent empty payloads).
pub async fn fetch_and_persist(http: &reqwest::Client, url: &str) -> Result<UpdateReport> {
  let started = Instant::now();
  let resp = http.get(url).send().await.context("HTTP GET failed")?;
  let status = resp.status();
  let body = resp.bytes().await.context("read response body")?;
  if !status.is_success() {
    anyhow::bail!("{url} returned HTTP {status}");
  }
  let parsed: Catalogue =
    serde_json::from_slice(&body).with_context(|| format!("parse {url} as models.dev catalogue"))?;
  if parsed.is_empty() {
    anyhow::bail!("{url} returned an empty catalogue");
  }
  let models: usize = parsed.values().map(|p| p.models.len()).sum();

  let path = cache_path().context("could not resolve a cache directory")?;
  if let Some(parent) = path.parent() {
    std::fs::create_dir_all(parent).with_context(|| format!("create cache dir {}", parent.display()))?;
  }

  // Atomic rename: write a sibling `.tmp` then rename onto the final path.
  let tmp = path.with_extension("json.tmp");
  std::fs::write(&tmp, &body).with_context(|| format!("write {}", tmp.display()))?;
  std::fs::rename(&tmp, &path).with_context(|| format!("atomic rename to {}", path.display()))?;

  Ok(UpdateReport {
    providers: parsed.len(),
    models,
    bytes: body.len() as u64,
    path,
    elapsed: started.elapsed(),
  })
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn embedded_snapshot_parses() {
    let c = parse_embedded();
    assert!(!c.is_empty(), "embedded snapshot must contain providers");
    assert!(c.contains_key("github-copilot"), "missing github-copilot");
    for id in ["zai", "zai-coding-plan", "zhipuai", "zhipuai-coding-plan"] {
      assert!(c.contains_key(id), "missing provider {id}");
    }
  }

  #[test]
  fn copilot_has_models() {
    let c = parse_embedded();
    let p = c.get("github-copilot").unwrap();
    assert!(!p.models.is_empty(), "github-copilot has no models");
  }
}
