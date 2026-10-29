//! `csub serve`: the single owner of a subscription pool.
//!
//! Holds the pool lock for its whole life, keeps the runtime's usage monitor
//! running, and serves the status API and the subscription gateway on one
//! loopback port.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::GracefulShutdown;
use rayline_subscriptions::{
    PoolLock, PoolLockError, SubscriptionPoolRuntime, SubscriptionRuntimeOptions,
};
use serde_json::json;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::gateway::{Body, Gateway, LAUNCH_PREFIX, json_response};

pub const DEFAULT_PORT: u16 = 20816;

/// Longest wait for in-flight requests on shutdown. The launchd job's
/// ExitTimeOut is longer, so launchd never kills a draining daemon.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(45);

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
    // Installed before any credential work: startup polls usage and may
    // rotate an expired grant, and the default disposition would kill the
    // process mid-refresh. A signal that arrives during startup is held and
    // handled by the orderly shutdown below once startup returns.
    let mut signals = Signals::install().context("install signal handlers")?;
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
    let monitor = runtime.spawn_monitor();
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

    let port = options.port;
    let graceful = GracefulShutdown::new();
    loop {
        let (stream, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    warn!("accept failed: {error}");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            },
            () = signals.recv() => break,
        };
        let runtime = runtime.clone();
        let gateway = gateway.clone();
        let service = service_fn(move |request| {
            let runtime = runtime.clone();
            let gateway = gateway.clone();
            async move {
                Ok::<_, std::convert::Infallible>(route(request, port, &runtime, &gateway).await)
            }
        });
        let connection =
            graceful.watch(http1::Builder::new().serve_connection(TokioIo::new(stream), service));
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::debug!("connection closed: {error}");
            }
        });
    }
    drop(listener);
    // In-flight requests may be mid-refresh; let them finish before the lock
    // is released so a successor never races a rotating grant.
    info!("csub shutting down; draining connections (signal again to stop now)");
    tokio::select! {
        result = tokio::time::timeout(SHUTDOWN_GRACE, graceful.shutdown()) => {
            if result.is_err() {
                warn!("connections still open after {SHUTDOWN_GRACE:?}; stopping anyway");
            }
        }
        () = signals.recv() => warn!("second signal; stopping without draining"),
    }
    // Background usage polls and recoveries run on their own tasks; stop
    // scheduling them and let any refresh in flight persist its rotated grant
    // before the lock is released.
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    info!("waiting for in-flight credential refreshes");
    tokio::select! {
        result = tokio::time::timeout(SHUTDOWN_GRACE, runtime.quiesce_credentials()) => {
            if result.is_err() {
                warn!("credential refresh still running after {SHUTDOWN_GRACE:?}; stopping anyway");
            }
        }
        () = signals.recv() => warn!("second signal; not waiting for credential refreshes"),
    }
    info!("flushing recovery state");
    if let Err(error) = runtime.flush_recovery_state().await {
        warn!("recovery state flush failed: {error}");
    }
    drop(lock);
    Ok(())
}

fn is_browser_request<B>(request: &Request<B>) -> bool {
    request.method() == Method::OPTIONS
        || ["origin", "sec-fetch-site", "sec-fetch-mode"]
            .iter()
            .any(|name| request.headers().contains_key(*name))
}

async fn route(
    request: Request<Incoming>,
    port: u16,
    runtime: &Arc<SubscriptionPoolRuntime>,
    gateway: &Gateway,
) -> Response<Body> {
    // A DNS-rebound web page reaches this port with its own host name; only
    // loopback names are served, so a browser can never spend pool quota.
    if !is_loopback_host(request.headers(), port) {
        return json_response(
            StatusCode::MISDIRECTED_REQUEST,
            json!({"ok": false, "error": "csub only serves 127.0.0.1, localhost or [::1]"}),
        );
    }
    // Claude Code and the csub CLI never send browser headers. Anything with
    // an Origin (or a CORS preflight) is a web page; refusing it outright
    // keeps a site the user visits from spending pool quota through
    // fetch(..., {mode: "no-cors"}).
    if is_browser_request(&request) {
        return json_response(
            StatusCode::FORBIDDEN,
            json!({"ok": false, "error": "csub does not serve browser requests"}),
        );
    }
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

fn is_loopback_host(headers: &hyper::HeaderMap, port: u16) -> bool {
    let Some(host) = headers
        .get(hyper::header::HOST)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let host = host.trim();
    ["127.0.0.1", "localhost", "[::1]"].iter().any(|name| {
        host.eq_ignore_ascii_case(name) || host.eq_ignore_ascii_case(&format!("{name}:{port}"))
    })
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

/// SIGTERM (launchd) and SIGINT (Ctrl-C), registered eagerly so that from
/// `install` on neither terminates the process by default.
struct Signals {
    #[cfg(unix)]
    terminate: tokio::signal::unix::Signal,
    #[cfg(unix)]
    interrupt: tokio::signal::unix::Signal,
}

impl Signals {
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                terminate: signal(SignalKind::terminate())?,
                interrupt: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// Cancel-safe: a signal not yet received stays pending.
    async fn recv(&mut self) {
        #[cfg(unix)]
        tokio::select! {
            _ = self.terminate.recv() => {}
            _ = self.interrupt.recv() => {}
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(value: &str) -> hyper::HeaderMap {
        let mut headers = hyper::HeaderMap::new();
        headers.insert(
            hyper::header::HOST,
            hyper::header::HeaderValue::from_str(value).expect("host"),
        );
        headers
    }

    #[test]
    fn serves_only_loopback_host_names() {
        assert!(is_loopback_host(&host("127.0.0.1:20816"), 20816));
        assert!(is_loopback_host(&host("localhost:20816"), 20816));
        assert!(is_loopback_host(&host("[::1]:20816"), 20816));
        assert!(is_loopback_host(&host("LOCALHOST"), 20816));
        assert!(!is_loopback_host(&host("attacker.example:20816"), 20816));
        assert!(!is_loopback_host(&host("127.0.0.1:80"), 20816));
        assert!(!is_loopback_host(&hyper::HeaderMap::new(), 20816));
    }

    #[test]
    fn refuses_browser_requests() {
        let request = |method: Method, header: Option<(&str, &str)>| {
            let mut builder = Request::builder().method(method).uri("/v1/messages");
            if let Some((name, value)) = header {
                builder = builder.header(name, value);
            }
            builder.body(()).expect("request")
        };
        assert!(!is_browser_request(&request(Method::POST, None)));
        assert!(is_browser_request(&request(Method::OPTIONS, None)));
        assert!(is_browser_request(&request(
            Method::POST,
            Some(("origin", "https://attacker.example"))
        )));
        assert!(is_browser_request(&request(
            Method::POST,
            Some(("origin", "null"))
        )));
        assert!(is_browser_request(&request(
            Method::GET,
            Some(("sec-fetch-mode", "no-cors"))
        )));
    }
}
