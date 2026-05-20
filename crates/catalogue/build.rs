//! Acquire the models.dev catalogue snapshot at build time.
//!
//! Strategy (in order):
//!   1. If `MODELS_DEV_SNAPSHOT` is set, copy that file into `OUT_DIR`. This
//!      is the explicit airgapped override.
//!   2. On docs.rs, prefer the vendored snapshot because network access is
//!      unavailable there.
//!   3. If Cargo set `CARGO_NET_OFFLINE`, prefer the vendored snapshot and do
//!      not attempt a network fetch.
//!   4. Otherwise, try to fetch <https://models.dev/api.json> via `curl` so
//!      ordinary builds refresh to the latest catalogue.
//!   5. If the host appears offline or `curl` is unavailable, fall back to a
//!      vendored snapshot in `crates/catalogue/vendor`.
//!   6. If neither download nor vendored snapshot is available, reuse a
//!      previously-fetched `OUT_DIR/models.dev.json` when present.
//!
//! The chosen file's path is exposed to the crate as
//! `env!("MODELS_DEV_SNAPSHOT_PATH")` so `include_bytes!` can embed it.

use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

const URL: &str = "https://models.dev/api.json";

fn main() {
  println!("cargo:rerun-if-env-changed=MODELS_DEV_SNAPSHOT");
  println!("cargo:rerun-if-env-changed=DOCS_RS");
  println!("cargo:rerun-if-env-changed=CARGO_NET_OFFLINE");
  println!("cargo:rerun-if-changed=build.rs");

  let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR must be set by cargo"));
  let dest = out_dir.join("models.dev.json");
  let tmp = out_dir.join("models.dev.json.tmp");
  let vendored = PathBuf::from("vendor/models.dev.json");

  // 1. Explicit override wins.
  if let Some(p) = env::var_os("MODELS_DEV_SNAPSHOT") {
    let src = PathBuf::from(&p);
    println!("cargo:rerun-if-changed={}", src.display());
    fs::copy(&src, &dest).unwrap_or_else(|e| panic!("MODELS_DEV_SNAPSHOT={} could not be read: {e}", src.display()));
    ensure_sane(&dest);
    emit(&dest);
    return;
  }

  let docs_rs = env::var_os("DOCS_RS").is_some();
  let cargo_offline = env::var_os("CARGO_NET_OFFLINE").as_deref().is_some_and(is_truthy);

  // 2-3. docs.rs and Cargo offline mode should not fetch.
  println!("cargo:rerun-if-changed={}", vendored.display());
  if docs_rs || cargo_offline {
    if vendored.exists() {
      ensure_sane(&vendored);
      fs::copy(&vendored, &dest)
        .unwrap_or_else(|e| panic!("vendored snapshot {} could not be copied: {e}", vendored.display()));
      emit(&dest);
      return;
    }
    let mode = if docs_rs { "DOCS_RS" } else { "CARGO_NET_OFFLINE" };
    panic!("{mode} is set, but vendored snapshot {} is missing", vendored.display());
  }

  // 4. Prefer a fresh download on ordinary builds.
  let status = Command::new("curl")
    .args(["-fsSL", "--max-time", "30", "--retry", "2", "--retry-delay", "2", "-o"])
    .arg(&tmp)
    .arg(URL)
    .status();

  match status {
    Ok(s) if s.success() => {
      ensure_sane(&tmp);
      fs::rename(&tmp, &dest).unwrap_or_else(|e| {
        panic!(
          "downloaded snapshot {} could not be moved into place: {e}",
          tmp.display()
        )
      });
      emit(&dest);
    }
    _ => {
      // Clean up a partial file so a retry doesn't think it's cached.
      let _ = fs::remove_file(&tmp);

      // 5. Offline or curl-less environments fall back to the vendored copy.
      if vendored.exists() {
        ensure_sane(&vendored);
        fs::copy(&vendored, &dest)
          .unwrap_or_else(|e| panic!("vendored snapshot {} could not be copied: {e}", vendored.display()));
        emit(&dest);
        return;
      }

      // 6. Last resort: reuse a previously-fetched snapshot if it's plausible.
      if dest.exists() {
        ensure_sane(&dest);
        emit(&dest);
        return;
      }

      panic!(
        "could not fetch {URL}, and no usable fallback snapshot was found.\n\
                 \n\
                 Either:\n  \
                   1. ensure `curl` is on PATH and the host has internet, or\n  \
                   2. commit `crates/catalogue/vendor/models.dev.json`, or\n  \
                   3. set MODELS_DEV_SNAPSHOT=/path/to/api.json to point at a\n     \
                      vendored copy of the catalogue."
      );
    }
  }
}

fn ensure_sane(p: &Path) {
  let mut file =
    fs::File::open(p).unwrap_or_else(|e| panic!("models.dev snapshot at {} could not be opened: {e}", p.display()));
  let mut buf = Vec::new();
  file
    .read_to_end(&mut buf)
    .unwrap_or_else(|e| panic!("models.dev snapshot at {} could not be read: {e}", p.display()));

  if buf.is_empty() {
    panic!("models.dev snapshot at {} is empty", p.display());
  }

  let first_non_ws = buf.iter().copied().find(|b| !b.is_ascii_whitespace());
  if first_non_ws != Some(b'{') {
    panic!(
      "models.dev snapshot at {} does not look like a JSON object",
      p.display()
    );
  }
}

fn emit(p: &Path) {
  println!("cargo:rustc-env=MODELS_DEV_SNAPSHOT_PATH={}", p.display());
}

fn is_truthy(v: &std::ffi::OsStr) -> bool {
  v.to_str()
    .map(|s| {
      matches!(
        s,
        "1" | "true" | "TRUE" | "True" | "yes" | "YES" | "Yes" | "on" | "ON" | "On"
      )
    })
    .unwrap_or(false)
}
