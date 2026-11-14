//! Serves the routes router on loopback against a decide service over HTTP.
//!
//! ```text
//! cargo run -p rayline-arc-routes --example serve -- \
//!     --package <package.json> --alias <alias> --decide http://127.0.0.1:<port> \
//!     --upstream https://<hosted router> --port <port>
//! ```

use std::sync::Arc;

use rayline_arc_routes::{Catalog, HttpDecider, Router, RouterConfig};
use tokio::net::TcpListener;

fn argument(name: &str) -> Result<String, String> {
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        if argument == name {
            return arguments.next().ok_or(format!("{name} takes a value"));
        }
    }
    Err(format!("{name} is required"))
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let manifest = std::fs::read(argument("--package")?)?;
    let catalog = Catalog::from_manifest(&manifest, &argument("--alias")?)?;
    let mut config = RouterConfig::new(catalog, &argument("--upstream")?);
    config.salt = format!("{:032x}", rand::random::<u128>());
    let decider = Arc::new(HttpDecider::new(&argument("--decide")?)?);
    let router = Arc::new(Router::new(config, decider)?);
    let port: u16 = argument("--port")?.parse()?;
    let listener = TcpListener::bind(("127.0.0.1", port)).await?;
    tracing::info!(port, "listening");
    router.serve(listener).await?;
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    if let Err(error) = run().await {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
