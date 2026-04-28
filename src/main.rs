use anyhow::Result;
use clap::Parser;

mod catalogue;
mod cli;
mod config;
mod pool;
mod provider;
mod server;
mod usage;
mod util;

#[tokio::main]
async fn main() -> Result<()> {
  tracing_subscriber::fmt()
    .with_env_filter(
      tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,llm_router=info")),
    )
    .init();

  let cli = cli::Cli::parse();
  cli.run().await
}
