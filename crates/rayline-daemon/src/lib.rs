//! The Rayline local process: supervises the ARC execution core and serves
//! its authenticated loopback gateway. `rayline` runs it through its internal
//! entry point; there is no separate binary.

use std::ffi::OsString;

use anyhow::Result;
use clap::Parser;

mod hybrid;
mod hybrid_arc;
mod hybrid_classifier;
mod hybrid_counter;
mod hybrid_metrics;

/// Run the local process with `args`, the options `rayline` passes after its
/// internal entry point, until it is told to stop.
pub async fn run(args: Vec<OsString>) -> Result<()> {
    let args = hybrid::Args::try_parse_from(args)?;
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
    hybrid::run(args).await
}
