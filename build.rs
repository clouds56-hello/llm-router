//! Acquire the models.dev catalogue snapshot at build time.
//!
//! Strategy (in order):
//!   1. If `MODELS_DEV_SNAPSHOT` is set, copy that file into `OUT_DIR`. This
//!      is the airgapped / vendored path.
//!   2. If `OUT_DIR/models.dev.json` already exists from a previous build,
//!      reuse it. `cargo clean` wipes this; rebuilds in the same target dir
//!      stay offline-friendly.
//!   3. Otherwise, fetch <https://models.dev/api.json> via `curl`. We do
//!      *not* refresh on every build — once the file exists in `OUT_DIR`,
//!      it's frozen until a clean.
//!
//! The chosen file's path is exposed to the crate as
//! `env!("MODELS_DEV_SNAPSHOT_PATH")` so `include_bytes!` can embed it.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const URL: &str = "https://models.dev/api.json";
const MIN_BYTES: u64 = 100 * 1024; // sanity: real api.json is ~1.8 MB

fn main() {
  println!("cargo:rerun-if-env-changed=MODELS_DEV_SNAPSHOT");
  println!("cargo:rerun-if-changed=build.rs");

  let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set by cargo"));
  let dest = out_dir.join("models.dev.json");

  // 1. Explicit override wins.
  if let Some(p) = env::var_os("MODELS_DEV_SNAPSHOT") {
    let src = PathBuf::from(&p);
    println!("cargo:rerun-if-changed={}", src.display());
    fs::copy(&src, &dest).unwrap_or_else(|e| panic!("MODELS_DEV_SNAPSHOT={} could not be read: {e}", src.display()));
    ensure_sane(&dest);
    emit(&dest);
    return;
  }

  // 2. Reuse a previously-fetched snapshot if it's plausible.
  if dest.exists() {
    if let Ok(meta) = fs::metadata(&dest) {
      if meta.len() >= MIN_BYTES {
        emit(&dest);
        return;
      }
    }
  }

  // 3. First-time fetch.
  let status = Command::new("curl")
    .args(["-fsSL", "--max-time", "30", "--retry", "2", "--retry-delay", "2", "-o"])
    .arg(&dest)
    .arg(URL)
    .status();

  match status {
    Ok(s) if s.success() => {
      ensure_sane(&dest);
      emit(&dest);
    }
    _ => {
      // Clean up a partial file so a retry doesn't think it's cached.
      let _ = fs::remove_file(&dest);
      panic!(
        "could not fetch {URL}.\n\
                 \n\
                 Either:\n  \
                   1. ensure `curl` is on PATH and the host has internet, or\n  \
                   2. set MODELS_DEV_SNAPSHOT=/path/to/api.json to point at a\n     \
                      vendored copy of the catalogue."
      );
    }
  }
}

fn ensure_sane(p: &Path) {
  let len = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
  if len < MIN_BYTES {
    let _ = fs::remove_file(p);
    panic!(
      "models.dev snapshot at {} is implausibly small ({} bytes); refusing to embed",
      p.display(),
      len
    );
  }
}

fn emit(p: &Path) {
  println!("cargo:rustc-env=MODELS_DEV_SNAPSHOT_PATH={}", p.display());
}
