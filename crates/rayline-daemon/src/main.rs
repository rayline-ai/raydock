// Rayline daemon: supervises the ARC execution core for `raydock hybrid`.

use anyhow::Result;
use clap::{Parser, Subcommand};

mod hybrid;
mod hybrid_classifier;
mod hybrid_counter;
mod hybrid_metrics;

const DAEMON_VERSION: &str = env!("RAYLINE_DAEMON_VERSION");

#[derive(Parser)]
#[command(
    name = env!("CARGO_BIN_NAME"),
    bin_name = env!("CARGO_BIN_NAME"),
    version = DAEMON_VERSION
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Supervise the ARC execution core; inference runtimes remain externally owned.
    Hybrid(hybrid::Args),
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Hybrid(args) => hybrid::run(args).await,
    }
}
