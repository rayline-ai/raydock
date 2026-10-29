//! `csub serve`: the single owner of a subscription pool.
//!
//! Holds the pool lock for its whole life, keeps the runtime's usage monitor
//! running, and serves the status API and the subscription gateway on one
//! loopback port.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, anyhow};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rayline_subscriptions::{
    PoolLock, PoolLockError, SubscriptionPoolRuntime, SubscriptionRuntimeOptions,
};
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::gateway::{Body, Gateway, LAUNCH_PREFIX, json_response};

pub const DEFAULT_PORT: u16 = 20816;

/// Exit status when another process already owns the pool. launchd retries
/// after its throttle interval, so a supervisor never starts a second owner.
pub const EXIT_POOL_OWNED: i32 = 75;

pub struct ServeOptions {
    pub pool_id: String,
    pub config_path: Option<PathBuf>,
    pub port: u16,
    pub state_dir: Option<PathBuf>,
    pub anthropic_base_url: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error("{0}")]
    PoolOwned(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub async fn serve(options: ServeOptions) -> Result<(), ServeError> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory not found"))?;
    let state_dir = options
        .state_dir
        .clone()
        .unwrap_or_else(|| PoolLock::default_state_dir(&home));
    let lock = match PoolLock::try_acquire(&state_dir, &options.pool_id) {
        Ok(lock) => lock,
        Err(error @ PoolLockError::Held { .. }) => {
            return Err(ServeError::PoolOwned(error.to_string()));
        }
        Err(error) => return Err(anyhow::Error::new(error).into()),
    };
    if let Some(pid) = legacy_rld_pool_daemon(&home) {
        return Err(ServeError::PoolOwned(format!(
            "a legacy rld subscription-pool daemon (pid {pid}) owns the pool; stop it before starting csub"
        )));
    }
    let address = SocketAddr::from(([127, 0, 0, 1], options.port));
    let listener = TcpListener::bind(address).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AddrInUse {
            ServeError::PoolOwned(format!(
                "127.0.0.1:{} is already in use; another pool daemon may be running",
                options.port
            ))
        } else {
            anyhow::Error::new(error)
                .context(format!("bind 127.0.0.1:{}", options.port))
                .into()
        }
    })?;

    let (config_path, config) =
        crate::cli::load_config(options.config_path.as_deref()).map_err(|error| anyhow!(error))?;
    let pool = config.pools.get(&options.pool_id).cloned().ok_or_else(|| {
        anyhow!(
            "subscription pool {:?} is not registered in {}",
            options.pool_id,
            config_path.display()
        )
    })?;
    let runtime = SubscriptionPoolRuntime::start(
        options.pool_id.clone(),
        pool,
        SubscriptionRuntimeOptions {
            anthropic_base_url: options.anthropic_base_url.clone(),
            home_dir: Some(home.clone()),
            state_dir: Some(state_dir.join(format!("pool-{}", options.pool_id))),
            ..Default::default()
        },
    )
    .await
    .context("start subscription pool")?;
    let _monitor = runtime.spawn_monitor();
    let gateway = Arc::new(
        Gateway::new(runtime.clone(), options.anthropic_base_url.clone())
            .context("build gateway HTTP client")?,
    );
    info!(
        "csub pool={} accounts={} listening on http://{address} (lock {})",
        runtime.pool_id(),
        runtime.account_count(),
        lock.path().display()
    );

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    warn!("accept failed: {error}");
                    continue;
                }
            },
            () = &mut shutdown => break,
        };
        let runtime = runtime.clone();
        let gateway = gateway.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let runtime = runtime.clone();
                let gateway = gateway.clone();
                async move {
                    Ok::<_, std::convert::Infallible>(route(request, &runtime, &gateway).await)
                }
            });
            if let Err(error) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                tracing::debug!("connection closed: {error}");
            }
        });
    }
    info!("csub shutting down; flushing recovery state");
    if let Err(error) = runtime.flush_recovery_state().await {
        warn!("recovery state flush failed: {error}");
    }
    drop(lock);
    Ok(())
}

async fn route(
    request: Request<Incoming>,
    runtime: &Arc<SubscriptionPoolRuntime>,
    gateway: &Gateway,
) -> Response<Body> {
    let path = request.uri().path();
    if path.starts_with(LAUNCH_PREFIX) || path.starts_with("/v1/messages") {
        return gateway.handle(request).await;
    }
    match (request.method(), path) {
        (&Method::GET, "/healthz") => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "service": "csub",
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
                "pool": runtime.pool_id(),
            }),
        ),
        (&Method::GET, "/v1/subscriptions/status") => {
            json_response(StatusCode::OK, json!(runtime.status()))
        }
        // Guarded before the pool is touched: a reload reopens every
        // credential source, which on macOS is a Keychain access per account.
        // Requiring JSON keeps a cross-site form post from triggering it.
        (&Method::POST, "/v1/subscriptions/reload") if !sends_json(&request) => json_response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            json!({"ok": false, "error": "reload requires content-type: application/json"}),
        ),
        (&Method::POST, "/v1/subscriptions/reload") => {
            json_response(StatusCode::OK, json!(runtime.reload_credentials().await))
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            json!({"ok": false, "error": "not found"}),
        ),
    }
}

fn sends_json(request: &Request<Incoming>) -> bool {
    request
        .headers()
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

/// The pre-csub daemon (`rld proxy --subscription-pool`, started by
/// `rayline claude --subscription-pool` or the launchd shim) does not take the
/// pool lock. Its pidfile is the only evidence it owns the pool.
fn legacy_rld_pool_daemon(home: &Path) -> Option<u32> {
    let pidfile = home.join(".rayline/rld/subscriptions/rl-rld-proxy.pid");
    let pid: u32 = std::fs::read_to_string(pidfile).ok()?.trim().parse().ok()?;
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&output.stdout);
    (command.contains("rld") && command.contains(" proxy")).then_some(pid)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = terminate.recv() => {}
                    _ = tokio::signal::ctrl_c() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
