use clap::Parser;
use std::error::Error as StdError;
use std::process::ExitCode;

use llm_config as config;
mod cli;
mod db;
mod error;
mod logging;
mod provider;
mod server_runtime;
mod util;

#[tokio::main]
async fn main() -> ExitCode {
  // The CLI installs its own subscriber once it has loaded config + decided
  // on a [`logging::RunMode`]. We do NOT call `logging::init_basic()` here
  // anymore: that races against the real subscriber.
  let parsed = cli::Cli::parse();
  match parsed.run().await {
    Ok(()) => ExitCode::SUCCESS,
    Err(e) => {
      report(&e);
      ExitCode::FAILURE
    }
  }
}

/// Print an error and its full source chain to stderr.
fn report(e: &dyn StdError) {
  eprintln!("error: {e}");
  let mut src = e.source();
  while let Some(s) = src {
    eprintln!("  caused by: {s}");
    src = s.source();
  }
}
