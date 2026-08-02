//! Transparent HTTP CONNECT proxy for Claude Code Rayline routing.
//!
//! The proxy only intercepts TLS for `api.anthropic.com`. Rayline-routed
//! inference/model requests have claude.ai auth stripped and Rayline router
//! auth injected. Anthropic account, OAuth, session, MCP, and unknown paths
//! pass through to Anthropic with the original claude.ai auth intact.
//!
//! CONNECT-tunnelled traffic to any other host is blind-tunnelled (raw TCP
//! passthrough). Clients that forward-proxy HTTPS instead of CONNECT-tunnelling
//! it — notably axios's built-in env-proxy support, used by stdio MCP servers
//! such as firecrawl — send an absolute-form request target; those are
//! re-originated to the upstream via the proxy's reqwest client (native OS
//! trust store), so corporate TLS-MITM roots validate without the MCP child
//! needing to trust them.

use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;

pub use rayline_authcache::{
    AuthCache, MAX_AUTH_CACHE_ENTRIES, evict_auth_cache_overflow, new_auth_cache,
    stash_auth_headers,
};

use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{HeaderName, HeaderValue};
use hyper::service::service_fn;
use hyper::{HeaderMap, Method, Request, Response, StatusCode, Uri};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_metrics::{MetricsUpdate, REQUEST_ID_HEADER, SharedMetricsSink, new_request_id};
use rayline_subscriptions::{
    HeaderSnapshot, ResponseClassification, SessionAssignmentReason, SessionRouteStatus,
    SessionStatusSnapshot, SubscriptionPoolRuntime, classify_response, derive_status_id,
};
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair, KeyUsagePurpose,
    PublicKeyData,
};
use rustls::ServerConfig;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_util::io::StreamReader;
use tracing::{debug, info, warn};

pub const DEFAULT_PORT: u16 = 20810;
pub const ANTHROPIC_HOST: &str = "api.anthropic.com";
pub const DEFAULT_ANTHROPIC_URL: &str = "https://api.anthropic.com";
pub const DEFAULT_ROUTER_URL: &str = "https://api.rayline.ai";
const CLAUDE_CODE_AGENT_ID_HEADER: &str = "x-claude-code-agent-id";
const RAYLINE_AGENT_TYPE_HEADER: &str = "x-rayline-claude-code-agent-type";
/// Claude Code writes `agent-<id>.meta.json` (which carries `agentType`)
/// concurrently with — sometimes a few ms after — it fires the subagent's
/// first request. Resolving the type just once races that write and can fall
/// back to passthrough, sending a routable subagent to the cloud. Poll briefly
/// so the meta file has time to land. Only paid when an `agent_id` is present
/// and unresolved, so main-thread traffic is never delayed.
const AGENT_TYPE_RESOLVE_MAX_ATTEMPTS: usize = 6;
const AGENT_TYPE_RESOLVE_RETRY_DELAY: Duration = Duration::from_millis(40);
const MAX_SUBSCRIPTION_ERROR_BYTES: usize = 2 * 1024 * 1024;
const MAX_SESSION_STATUS_BYTES: usize = 64 * 1024;
const SESSION_STATUS_RETENTION: Duration = Duration::from_secs(24 * 60 * 60);
const SESSION_STATUS_ASSIGNMENT_MAX_ATTEMPTS: usize = 6;
const SESSION_STATUS_ASSIGNMENT_RETRY_DELAY: Duration = Duration::from_millis(40);

#[derive(Clone)]
pub struct ProxyOptions {
    pub port: u16,
    pub router_url: String,
    pub router_api_key: String,
    pub local_available: bool,
    /// Shared health flag for the local model, flipped by `rld serve`'s
    /// watchdog. Gates `local_available` dynamically: when present and `false`
    /// the proxy stops advertising local so the cloud router serves the turn.
    /// `None` = honour `local_available` statically.
    pub local_healthy: Option<Arc<AtomicBool>>,
    pub local_model_id: Option<String>,
    pub local_adapter_port: Option<u16>,
    /// Custom user endpoint: advertise `x-rayline-local-custom` and suppress the
    /// forced hint so the router only delegates exploration subagents (mirrors
    /// the injector's custom-mode behavior in override routing).
    pub custom_mode: bool,
    pub auth_cache: Option<AuthCache>,
    pub ca_cert_path: PathBuf,
    pub ca_key_path: PathBuf,
    pub anthropic_url: String,
    pub connect_overrides: HashMap<String, String>,
    /// Additional PEM file of root certificates the proxy should trust when
    /// connecting to upstream servers (real `api.anthropic.com`, the Rayline
    /// router, etc.). Additive: native OS roots are always loaded too.
    /// Set to allow operation behind a corporate MITM gateway (Netskope,
    /// Zscaler, Palo Alto) whose CA is not in the OS trust store.
    pub upstream_ca_path: Option<PathBuf>,
    /// When set, the proxy writes the router's per-turn decision (selected
    /// model, policy, etc.) to this file as JSON after each Rayline-routed
    /// response. The Rayline Claude Code status line reads it
    /// to surface the concrete picked model. Best-effort: IO errors never fail
    /// the proxied request. `None` disables the sidecar.
    pub route_status_path: Option<PathBuf>,
    pub routing_mode: ProxyRoutingMode,
    /// In selective-subagents mode, route only these exact Claude Code agent ids
    /// to the router. Empty preserves the legacy behavior: route every subagent.
    pub selective_subagent_ids: Vec<String>,
    /// When the router URL points at the in-process local router, that router
    /// records router request lifecycle metrics using the same request
    /// id. The proxy still records Anthropic passthrough traffic.
    pub local_router_owns_metrics: bool,
    pub metrics: Option<SharedMetricsSink>,
    /// Local-only Claude subscription pool used for Anthropic `/v1/messages`
    /// passthrough legs. Credentials are read and refreshed by the daemon and
    /// never sent to Rayline's hosted router.
    pub subscription_pool: Option<Arc<SubscriptionPoolRuntime>>,
    /// Directory containing one credential-free subscription assignment
    /// snapshot per Claude Code launch. The launch id is hashed before it is
    /// used as a filename. Best-effort: IO errors never fail proxy traffic.
    pub session_status_dir: Option<PathBuf>,
    /// Shared Claude Code config directory used only to resolve local session
    /// metadata (for selective subagent routing). Credential source profiles
    /// remain separate and are never used as Claude's runtime config.
    pub claude_config_dir: Option<PathBuf>,
}

impl ProxyOptions {
    pub fn with_ca_paths(
        router_api_key: impl Into<String>,
        ca_cert_path: impl Into<PathBuf>,
        ca_key_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            port: DEFAULT_PORT,
            router_url: DEFAULT_ROUTER_URL.to_string(),
            router_api_key: router_api_key.into(),
            local_available: false,
            local_healthy: None,
            local_model_id: None,
            local_adapter_port: None,
            custom_mode: false,
            auth_cache: None,
            ca_cert_path: ca_cert_path.into(),
            ca_key_path: ca_key_path.into(),
            anthropic_url: DEFAULT_ANTHROPIC_URL.to_string(),
            connect_overrides: HashMap::new(),
            upstream_ca_path: None,
            route_status_path: None,
            routing_mode: ProxyRoutingMode::All,
            selective_subagent_ids: Vec::new(),
            local_router_owns_metrics: false,
            metrics: None,
            subscription_pool: None,
            session_status_dir: None,
            claude_config_dir: None,
        }
    }

    /// Effective local availability: the static capability AND, when a health
    /// flag is wired, the live health of the local model. Read per-request so a
    /// crashed/unhealthy local model stops being advertised mid-session.
    fn local_available_now(&self) -> bool {
        self.local_available
            && self
                .local_healthy
                .as_ref()
                .is_none_or(|h| h.load(Ordering::Relaxed))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProxyRoutingMode {
    All,
    SelectiveSubagents,
}

impl ProxyRoutingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::SelectiveSubagents => "selective-subagents",
        }
    }
}

#[derive(Clone)]
struct AppState {
    opts: Arc<ProxyOptions>,
    http: reqwest::Client,
    local_http: reqwest::Client,
    ca: Arc<LocalCa>,
    route_status_generation: Arc<AtomicU64>,
    route_status_io: Arc<AsyncMutex<()>>,
    session_status_generation: Arc<AtomicU64>,
    session_status_io: Arc<AsyncMutex<()>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteTarget {
    Router,
    Anthropic,
    BlindTunnel,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RouteDecision {
    pub target: RouteTarget,
    pub reason: &'static str,
}

/// Parse a PEM file containing one or more `BEGIN CERTIFICATE` blocks into
/// `reqwest::Certificate`s suitable for `ClientBuilder::add_root_certificate`.
///
/// Returns an error that names the file path and the parse failure reason, so
/// users misconfiguring `--upstream-ca-path` see exactly what's wrong (not a
/// later TLS handshake failure that looks like a network problem).
pub fn load_upstream_ca_bundle(path: &Path) -> Result<Vec<reqwest::Certificate>> {
    let pem = std::fs::read(path)
        .with_context(|| format!("read upstream CA bundle from {}", path.display()))?;
    let certs = reqwest::Certificate::from_pem_bundle(&pem)
        .with_context(|| format!("parse PEM certificates from {}", path.display()))?;
    if certs.is_empty() {
        return Err(anyhow!(
            "no certificates found in upstream CA bundle {}",
            path.display()
        ));
    }
    Ok(certs)
}

pub async fn serve(opts: ProxyOptions) -> Result<()> {
    let ca = LocalCa::load_or_generate(&opts.ca_cert_path, &opts.ca_key_path)?;
    let mut http_builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    if let Some(ca_path) = opts.upstream_ca_path.as_deref() {
        let certs = load_upstream_ca_bundle(ca_path)?;
        info!(
            "loaded {} extra root cert(s) from {}",
            certs.len(),
            ca_path.display()
        );
        for cert in certs {
            http_builder = http_builder.add_root_certificate(cert);
        }
    }
    let http = http_builder.build()?;
    let local_http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()?;
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), opts.port);
    let listener = TcpListener::bind(addr).await?;
    info!(
        "proxy listening on 127.0.0.1:{} (router={}, local_available={})",
        opts.port, opts.router_url, opts.local_available
    );

    let state = AppState {
        opts: Arc::new(opts),
        http,
        local_http,
        ca: Arc::new(ca),
        route_status_generation: Arc::new(AtomicU64::new(0)),
        route_status_io: Arc::new(AsyncMutex::new(())),
        session_status_generation: Arc::new(AtomicU64::new(0)),
        session_status_io: Arc::new(AsyncMutex::new(())),
    };

    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle_proxy_request(state, req).await) }
            });
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection_with_upgrades(io, svc)
                .await
            {
                warn!("proxy connection error: {e}");
            }
        });
    }
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn json_response(status: StatusCode, value: Value) -> Response<BoxBody> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(body))
        .unwrap()
}

/// What `handle_proxy_request` should do with an incoming request. Pure
/// function of the method + request-target so it can be unit-tested without a
/// live socket.
#[derive(Debug, PartialEq, Eq)]
enum ProxyAction {
    /// `GET /healthz` liveness probe.
    Healthz,
    /// `CONNECT host:port` — tunnel (intercept Anthropic, blind-tunnel the rest).
    Connect,
    /// Absolute-form request target whose host is `api.anthropic.com`; route it
    /// through the Rayline router path like an intercepted request.
    ForwardAnthropic,
    /// Absolute-form request target for any other host; re-originate verbatim.
    ForwardAbsolute,
    /// Origin-form non-CONNECT request we can't serve.
    Reject,
}

fn classify_proxy_request(method: &Method, uri: &Uri) -> ProxyAction {
    // Only the origin-form `GET /healthz` (no authority) is our local liveness
    // probe. A forward-proxied `GET https://host/healthz` shares the same path
    // but carries an authority, so it must be re-originated upstream rather than
    // shadowed by the proxy's own health response.
    if method == Method::GET && uri.host().is_none() && uri.path() == "/healthz" {
        return ProxyAction::Healthz;
    }
    if method == Method::CONNECT {
        return ProxyAction::Connect;
    }
    // A non-CONNECT request with an authority in the request-target is a
    // forward-proxy request (`GET https://host/path`). Well-behaved clients
    // CONNECT-tunnel HTTPS, but axios's built-in env-proxy support forward-
    // proxies it instead, so stdio MCP servers that use axios (e.g. firecrawl)
    // land here. Re-originate rather than rejecting with 405.
    match uri.host() {
        Some(host) if host.eq_ignore_ascii_case(ANTHROPIC_HOST) => ProxyAction::ForwardAnthropic,
        Some(_) => ProxyAction::ForwardAbsolute,
        None => ProxyAction::Reject,
    }
}

async fn handle_proxy_request(state: AppState, req: Request<Incoming>) -> Response<BoxBody> {
    match classify_proxy_request(req.method(), req.uri()) {
        ProxyAction::Healthz => healthz_response(&state),
        ProxyAction::Connect => {
            let Some(authority) = req.uri().authority().map(|a| a.as_str().to_string()) else {
                return Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(full_body("CONNECT requires authority"))
                    .unwrap();
            };
            let state_for_task = state.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_connect(state_for_task, req, authority.clone()).await {
                    warn!("CONNECT {authority} failed: {e}");
                }
            });
            Response::builder()
                .status(StatusCode::OK)
                .body(full_body(Bytes::new()))
                .unwrap()
        }
        ProxyAction::ForwardAnthropic => handle_forward_proxy(state, req, true).await,
        ProxyAction::ForwardAbsolute => handle_forward_proxy(state, req, false).await,
        ProxyAction::Reject => Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(full_body("proxy expects CONNECT"))
            .unwrap(),
    }
}

/// Serve a forward-proxy request by re-originating it to the upstream named in
/// the absolute request-target. The proxy (not the MCP child) terminates the
/// upstream TLS, so its reqwest client's native OS trust store validates
/// corporate TLS-MITM roots transparently — the child never has to trust them.
/// Anthropic traffic still flows through the Rayline router path.
async fn handle_forward_proxy(
    state: AppState,
    req: Request<Incoming>,
    is_anthropic: bool,
) -> Response<BoxBody> {
    let result = if is_anthropic {
        let launch_id = proxy_launch_id(req.headers());
        forward_anthropic_request(state, req, launch_id.as_deref()).await
    } else {
        forward_absolute_request(state, req).await
    };
    match result {
        Ok(resp) => resp,
        Err(e) => {
            warn!("forward-proxy error: {e}");
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!("proxy forward error: {e}")
                    }
                }),
            )
        }
    }
}

/// Re-originate an absolute-form request to its upstream host verbatim. Used for
/// forward-proxied non-Anthropic traffic (e.g. axios-based MCP servers).
async fn forward_absolute_request(
    state: AppState,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>> {
    let (parts, body) = req.into_parts();
    let url = parts.uri.to_string();
    let bytes = body.collect().await?.to_bytes();
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;
    let mut outbound = state.http.request(method, &url).body(bytes.to_vec());
    for (name, value) in parts.headers.iter() {
        // reqwest derives Host from the URL; passing the client's Host through
        // would double it.
        if is_hop_by_hop(name) || name == hyper::header::HOST {
            continue;
        }
        outbound = outbound.header(name.as_str(), value.as_bytes());
    }
    let resp = outbound.send().await?;
    let status = resp.status();
    debug!(
        "forward-proxy {} {} -> status={}",
        parts.method.as_str(),
        parts.uri,
        status.as_u16()
    );
    response_from_reqwest(resp, status, None, None, None, None, false).await
}

fn healthz_response(state: &AppState) -> Response<BoxBody> {
    json_response(
        StatusCode::OK,
        json!({
            "ok": true,
            "proxy_port": state.opts.port,
            "router_url": state.opts.router_url,
            "local_available": state.opts.local_available_now(),
            "local_model_id": state.opts.local_model_id,
            "local_adapter_port": state.opts.local_adapter_port,
            "has_router_key": !state.opts.router_api_key.is_empty(),
            "ca_cert_path": state.opts.ca_cert_path.display().to_string(),
            "routing_mode": state.opts.routing_mode.as_str(),
        }),
    )
}

fn proxy_launch_id(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(hyper::header::PROXY_AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = std::str::from_utf8(&decoded).ok()?;
    let (username, launch_id) = decoded.split_once(':')?;
    if username != "rayline"
        || launch_id.is_empty()
        || launch_id.len() > 128
        || !launch_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return None;
    }
    Some(launch_id.to_owned())
}

async fn handle_connect(state: AppState, req: Request<Incoming>, authority: String) -> Result<()> {
    let launch_id = proxy_launch_id(req.headers());
    let upgraded = hyper::upgrade::on(req)
        .await
        .context("upgrade CONNECT stream")?;
    if is_anthropic_authority(&authority) {
        intercept_anthropic_tls(state, upgraded, launch_id).await
    } else {
        blind_tunnel(state, upgraded, &authority).await
    }
}

async fn intercept_anthropic_tls(
    state: AppState,
    upgraded: hyper::upgrade::Upgraded,
    launch_id: Option<String>,
) -> Result<()> {
    let config = state.ca.server_config_for_host(ANTHROPIC_HOST)?;
    let acceptor = TlsAcceptor::from(config);
    let tls = acceptor
        .accept(TokioIo::new(upgraded))
        .await
        .context("accept api.anthropic.com TLS")?;
    let io = TokioIo::new(tls);
    let svc = service_fn(move |req| {
        let state = state.clone();
        let launch_id = launch_id.clone();
        async move {
            Ok::<_, Infallible>(handle_anthropic_request(state, req, launch_id.as_deref()).await)
        }
    });
    auto::Builder::new(TokioExecutor::new())
        .serve_connection(io, svc)
        .await
        .map_err(|e| anyhow!("serve intercepted api.anthropic.com request: {e}"))?;
    Ok(())
}

async fn blind_tunnel(
    state: AppState,
    upgraded: hyper::upgrade::Upgraded,
    authority: &str,
) -> Result<()> {
    let mut client = TokioIo::new(upgraded);
    let connect_target = state
        .opts
        .connect_overrides
        .get(authority)
        .map(String::as_str)
        .unwrap_or(authority);
    let mut upstream = TcpStream::connect(connect_target)
        .await
        .with_context(|| format!("connect blind tunnel target {authority}"))?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    Ok(())
}

async fn handle_anthropic_request(
    state: AppState,
    req: Request<Incoming>,
    launch_id: Option<&str>,
) -> Response<BoxBody> {
    match forward_anthropic_request(state, req, launch_id).await {
        Ok(resp) => resp,
        Err(e) => {
            warn!("proxy forward error: {e}");
            json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!("proxy forward error: {e}")
                    }
                }),
            )
        }
    }
}

/// Detect the "agent-id present but type unresolved" state, emit a `warn!`,
/// and increment the `routing_uncertain` counter.  Extracted from
/// `forward_anthropic_request` so the branch can be unit-tested without
/// standing up the full async HTTP machinery.
///
/// Returns `true` when the uncertain branch fired (i.e. the counter was
/// incremented), `false` otherwise.
fn emit_if_agent_type_unresolved(
    agent_id_present: bool,
    agent_type: Option<&str>,
    agent_id: String,
    metrics: Option<&SharedMetricsSink>,
) -> bool {
    let unresolved = agent_id_present && agent_type.is_none();
    if unresolved {
        warn!(
            "proxy agent-type resolution exhausted retries: agent_id={} — \
             Claude Code meta file not found; routing uncertain",
            agent_id
        );
        if let Some(metrics) = metrics {
            metrics.record(MetricsUpdate::RoutingUncertain { agent_id });
        }
    }
    unresolved
}

async fn forward_anthropic_request(
    state: AppState,
    req: Request<Incoming>,
    launch_id: Option<&str>,
) -> Result<Response<BoxBody>> {
    let (parts, body) = req.into_parts();
    let path_and_query = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let bytes = body.collect().await?.to_bytes();
    let agent_id = claude_code_agent_id(&parts.headers)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "<none>".to_owned());
    let agent_id_present = agent_id != "<none>";
    let agent_type = if agent_id_present {
        resolve_claude_code_agent_type_with_retry(
            &agent_id,
            state.opts.claude_config_dir.as_deref(),
        )
        .await
    } else {
        None
    };
    // When agent-id was present but resolution exhausted all retries the
    // meta file was never found. This most likely means Claude Code changed
    // its schema or path layout. Emit a warning and bump the counter so
    // operators notice rather than silently degrading.
    let agent_type_unresolved = emit_if_agent_type_unresolved(
        agent_id_present,
        agent_type.as_deref(),
        agent_id.clone(),
        state.opts.metrics.as_ref(),
    );
    let routed = prepare_anthropic_route_with_subagent_filter(
        &parts.method,
        parts.uri.path(),
        &parts.headers,
        bytes,
        state.opts.routing_mode,
        &state.opts.selective_subagent_ids,
        agent_type.as_deref(),
    );
    let decision = routed.decision;
    let body_model = request_body_model(&routed.body);
    let request_id = rayline_request_id(&parts.headers)
        .map(ToOwned::to_owned)
        .unwrap_or_else(new_request_id);
    let agent_id = routed
        .agent_id
        .clone()
        .unwrap_or_else(|| "<none>".to_owned());
    let agent_type = routed.agent_type.clone();
    let proxy_owns_metrics = proxy_owns_metrics_for_route(&state.opts, decision.target);
    // Distinguish "<unresolved>" (agent-id present but meta not found) from
    // "<none>" (no agent-id header at all) in the decision log.
    let agent_type_log = if agent_type_unresolved {
        "<unresolved>".to_owned()
    } else {
        agent_type.as_deref().unwrap_or("<none>").to_owned()
    };
    info!(
        "proxy route decision method={} path={} model={} agent_id={} agent_type={} target={:?} reason={} selective_subagents=[{}] local_available={} local_custom={}",
        parts.method.as_str(),
        path_and_query,
        body_model.as_deref().unwrap_or("<none>"),
        agent_id,
        agent_type_log,
        decision.target,
        decision.reason,
        state.opts.selective_subagent_ids.join(","),
        state.opts.local_available_now(),
        state.opts.custom_mode
    );
    if let Some(metrics) = state.opts.metrics.as_ref().filter(|_| proxy_owns_metrics) {
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: request_id.clone(),
            source: "proxy".to_owned(),
            requested_model: body_model.clone(),
            agent_id: none_if_marker(&agent_id),
            agent_type: agent_type.clone(),
        });
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: request_id.clone(),
            route_id: None,
            target: match decision.target {
                RouteTarget::Router => "router",
                RouteTarget::Anthropic => "anthropic",
                RouteTarget::BlindTunnel => "blind_tunnel",
            }
            .to_owned(),
            endpoint_id: proxied_sideband_endpoint_id(&decision, path_and_query),
            selected_model: (decision.target == RouteTarget::Anthropic)
                .then(|| body_model.clone())
                .flatten(),
            requested_model: body_model.clone(),
            policy: Some(decision.reason.to_owned()),
            task_class: None,
            agent_id: none_if_marker(&agent_id),
            agent_type: agent_type.clone(),
        });
    }
    let route_status_generation = state.route_status_generation.load(Ordering::SeqCst);
    let upstream_base = match decision.target {
        RouteTarget::Router => state.opts.router_url.trim_end_matches('/'),
        RouteTarget::Anthropic => state.opts.anthropic_url.trim_end_matches('/'),
        RouteTarget::BlindTunnel => unreachable!("intercepted TLS is only for Anthropic"),
    };
    let upstream_url = format!("{upstream_base}{path_and_query}");
    let body_was_rewritten = routed.body_was_rewritten;
    let bytes = routed.body;
    let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;

    if decision.target == RouteTarget::Anthropic
        && parts.method == Method::POST
        && parts.uri.path() == "/v1/messages"
        && let (Some(pool), Some(requested_model)) =
            (state.opts.subscription_pool.as_ref(), body_model.as_deref())
    {
        return forward_subscription_request(SubscriptionForward {
            state: &state,
            pool,
            launch_id: launch_id.unwrap_or_default(),
            requested_model,
            original_headers: &parts.headers,
            method,
            upstream_url: &upstream_url,
            body: &bytes,
            body_was_rewritten,
            request_id: &request_id,
            metrics: proxy_owns_metrics
                .then(|| state.opts.metrics.clone())
                .flatten(),
        })
        .await;
    }

    let mut outbound = state
        .http
        .request(method.clone(), &upstream_url)
        .body(bytes.to_vec());

    for (name, value) in parts.headers.iter() {
        if body_was_rewritten && *name == hyper::header::CONTENT_LENGTH {
            continue;
        }
        if name.as_str().eq_ignore_ascii_case(REQUEST_ID_HEADER) {
            continue;
        }
        if should_drop_header_for_route(name, &decision.target) {
            continue;
        }
        outbound = outbound.header(name.as_str(), value.as_bytes());
    }
    outbound = outbound.header(REQUEST_ID_HEADER, &request_id);

    if decision.target == RouteTarget::Router {
        if !state.opts.router_api_key.is_empty() {
            outbound = outbound.header("x-api-key", &state.opts.router_api_key);
        }
        if let Some(agent_type) = agent_type.as_deref() {
            outbound = outbound.header(RAYLINE_AGENT_TYPE_HEADER, agent_type);
        }
        if state.opts.local_available_now() {
            outbound = outbound.header("x-rayline-local-available", "true");
            if let Some(local_model_id) = state.opts.local_model_id.as_deref() {
                outbound = outbound.header("x-rayline-local-model-id", local_model_id);
            }
            if state.opts.custom_mode {
                // Custom endpoint: trust the user's opt-in (bypasses the model-id
                // allowlist) but stay exploration-subagent-only — no forced hint.
                outbound = outbound.header("x-rayline-local-custom", "true");
            } else {
                outbound = outbound.header("x-rayline-local-hint", "1");
            }
        } else {
            // Explicitly advertise unavailable so the router never routes local
            // to a down model. Client-supplied local-routing headers were
            // already dropped above, so this is the only such header on the
            // request.
            outbound = outbound.header("x-rayline-local-available", "false");
        }
    } else if decision.reason == "selective_main_passthrough"
        && parts.method == Method::POST
        && parts.uri.path() == "/v1/messages"
    {
        if let Some(path) = state.opts.route_status_path.clone() {
            state.route_status_generation.fetch_add(1, Ordering::SeqCst);
            let _guard = state.route_status_io.lock().await;
            RouteStatus::clear(&path).await;
        }
    }

    let resp = match outbound.send().await {
        Ok(resp) => resp,
        Err(error) => {
            if let Some(metrics) = state.opts.metrics.as_ref().filter(|_| proxy_owns_metrics) {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: None,
                    error: format!("upstream request failed: {error}"),
                });
            }
            return Err(error.into());
        }
    };
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Capture the router's per-turn decision before the 307 redirect is
    // followed: the router response (`resp`) carries the `X-Rayline-*` headers
    // on *both* the normal path and the local-redirect path
    // (`X-Rayline-Selected-Model: "local"`), but the local adapter's response
    // does not. Write the sidecar here so both paths are covered.
    if decision.target == RouteTarget::Router
        && let Some(route_status) =
            RouteStatus::from_headers(resp.headers(), state.opts.local_model_id.as_deref())
    {
        if let Some(metrics) = state.opts.metrics.as_ref().filter(|_| proxy_owns_metrics) {
            metrics.record(MetricsUpdate::RouteDecided {
                request_id: request_id.clone(),
                route_id: route_status.route_id.clone(),
                target: route_status_target(&route_status),
                endpoint_id: None,
                selected_model: Some(route_status.selected_model.clone()),
                requested_model: route_status.virtual_model.clone(),
                policy: route_status.policy.clone(),
                task_class: route_status.task_class.clone(),
                agent_id: none_if_marker(&agent_id),
                agent_type: agent_type.clone(),
            });
        }
        let pooled_launch_status = state.opts.subscription_pool.is_some()
            && state.opts.session_status_dir.is_some()
            && launch_id.is_some();
        if !pooled_launch_status && let Some(path) = state.opts.route_status_path.clone() {
            let global_route_status = route_status.clone();
            let generation = state.route_status_generation.clone();
            let io = state.route_status_io.clone();
            tokio::spawn(async move {
                global_route_status
                    .write_to_if_current(&path, generation, io, route_status_generation)
                    .await;
            });
        }
        if let (Some(directory), Some(launch_id)) = (
            state.opts.session_status_dir.clone(),
            launch_id.map(str::to_owned),
        ) {
            let session_route_status = route_status.to_session_status();
            let generation = Arc::clone(&state.session_status_generation);
            let io = Arc::clone(&state.session_status_io);
            tokio::spawn(async move {
                write_session_route_status(
                    &directory,
                    &launch_id,
                    &session_route_status,
                    generation,
                    io,
                )
                .await;
            });
        }
    }

    let local_redirect_location = if decision.target == RouteTarget::Router
        && status == reqwest::StatusCode::TEMPORARY_REDIRECT
    {
        stash_router_auth_for_local_redirect(&state, location.as_deref());
        location
            .as_deref()
            .and_then(|loc| rewrite_local_redirect_port(loc, state.opts.local_adapter_port))
    } else {
        None
    };

    if let Some(local_location) = local_redirect_location.as_deref() {
        return forward_local_redirect(
            state,
            &parts.headers,
            method,
            bytes,
            local_location,
            &request_id,
        )
        .await;
    }

    debug!(
        "{} {} -> {} status={} route={:?}",
        parts.method.as_str(),
        path_and_query,
        upstream_base,
        status.as_u16(),
        decision.target
    );

    response_from_reqwest(
        resp,
        status,
        local_redirect_location.as_deref(),
        proxy_owns_metrics
            .then(|| state.opts.metrics.clone())
            .flatten(),
        Some(request_id),
        Some(approximate_input_tokens(&bytes)),
        false,
    )
    .await
}

struct SubscriptionForward<'a> {
    state: &'a AppState,
    pool: &'a SubscriptionPoolRuntime,
    launch_id: &'a str,
    requested_model: &'a str,
    original_headers: &'a HeaderMap,
    method: reqwest::Method,
    upstream_url: &'a str,
    body: &'a Bytes,
    body_was_rewritten: bool,
    request_id: &'a str,
    metrics: Option<SharedMetricsSink>,
}

struct BufferedUpstream {
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    body: Bytes,
}

async fn forward_subscription_request(
    request: SubscriptionForward<'_>,
) -> Result<Response<BoxBody>> {
    let mut excluded = HashSet::new();
    let mut final_failover_response = None;
    let mut pending_assignment_reason = None;

    while excluded.len() < request.pool.account_count() {
        let selected = match request
            .pool
            .select(request.launch_id, request.requested_model, &excluded)
            .await
        {
            Ok(selected) => selected,
            Err(error) => {
                return match final_failover_response {
                    Some(response) => {
                        buffered_upstream_response(response, request.metrics, request.request_id)
                    }
                    None => {
                        warn!(
                            "subscription pool={} has no eligible account for model={}: {}",
                            request.pool.pool_id(),
                            request.requested_model,
                            error
                        );
                        Ok(subscription_exhausted_response(
                            request.pool.pool_id(),
                            request.requested_model,
                        ))
                    }
                };
            }
        };
        let account_id = selected.account_id.clone();
        let assignment_reason = pending_assignment_reason
            .take()
            .unwrap_or(selected.assignment_reason);
        debug!(
            "subscription placement pool={} account={} model={} reason={:?} score={} active_global_leases={} active_model_leases={}",
            request.pool.pool_id(),
            account_id,
            selected.model,
            assignment_reason,
            selected
                .placement_score
                .map(|score| format!("{score:.3}"))
                .unwrap_or_else(|| "unknown".to_owned()),
            selected.active_global_leases,
            selected.active_model_leases
        );
        write_subscription_status(&request, &account_id, assignment_reason).await;
        let mut access_token = selected.access_token;
        let mut refreshed_after_unauthorized = false;

        loop {
            let mut outbound = request
                .state
                .http
                .request(request.method.clone(), request.upstream_url)
                .body(request.body.to_vec());
            for (name, value) in request.original_headers.iter() {
                if request.body_was_rewritten && *name == hyper::header::CONTENT_LENGTH {
                    continue;
                }
                if name.as_str().eq_ignore_ascii_case(REQUEST_ID_HEADER)
                    || is_hop_by_hop(name)
                    || is_subscription_auth_header(name)
                {
                    continue;
                }
                outbound = outbound.header(name.as_str(), value.as_bytes());
            }
            outbound = outbound
                .header(REQUEST_ID_HEADER, request.request_id)
                .bearer_auth(access_token.expose());

            let response = match outbound.send().await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(metrics) = request.metrics.as_ref() {
                        metrics.record(MetricsUpdate::RequestErrored {
                            request_id: request.request_id.to_owned(),
                            status_code: None,
                            error: format!("Anthropic subscription request failed: {error}"),
                        });
                    }
                    // A network failure may have happened after Anthropic accepted
                    // the request. Never replay it against another subscription.
                    return Err(error.into());
                }
            };
            let status = response.status();
            let headers = subscription_header_snapshot(response.headers());
            request.pool.observe_response_headers(
                &account_id,
                request.requested_model,
                &headers,
            )?;
            write_subscription_status(&request, &account_id, assignment_reason).await;

            if status.is_success() {
                debug!(
                    "subscription pool={} account={} model={} status={}",
                    request.pool.pool_id(),
                    account_id,
                    request.requested_model,
                    status.as_u16()
                );
                return response_from_reqwest(
                    response,
                    status,
                    None,
                    request.metrics,
                    Some(request.request_id.to_owned()),
                    Some(approximate_input_tokens(request.body)),
                    true,
                )
                .await;
            }

            let buffered = buffer_subscription_response(response).await?;
            match classify_response(status.as_u16(), &headers, Some(&buffered.body)) {
                ResponseClassification::RefreshCredential if !refreshed_after_unauthorized => {
                    refreshed_after_unauthorized = true;
                    match request
                        .pool
                        .refresh_after_unauthorized(&account_id, &access_token)
                        .await
                    {
                        Ok(refreshed) => {
                            access_token = refreshed;
                            continue;
                        }
                        Err(error) => {
                            warn!(
                                "subscription pool={} account={} refresh failed: {}",
                                request.pool.pool_id(),
                                account_id,
                                error
                            );
                            final_failover_response = Some(buffered);
                            pending_assignment_reason =
                                Some(SessionAssignmentReason::CredentialFailover);
                        }
                    }
                }
                ResponseClassification::RefreshCredential => {
                    let _ = request.pool.mark_credential_unavailable(
                        &account_id,
                        "Anthropic rejected the refreshed OAuth credential",
                    );
                    final_failover_response = Some(buffered);
                    pending_assignment_reason = Some(SessionAssignmentReason::CredentialFailover);
                }
                ResponseClassification::FailoverQuota { .. } => {
                    final_failover_response = Some(buffered);
                    pending_assignment_reason = Some(SessionAssignmentReason::QuotaFailover);
                }
                ResponseClassification::EntitlementRejected { .. } => {
                    request
                        .pool
                        .mark_entitlement_unavailable(&account_id, request.requested_model)?;
                    final_failover_response = Some(buffered);
                    pending_assignment_reason = Some(SessionAssignmentReason::EntitlementFailover);
                }
                ResponseClassification::TransientRateLimit
                | ResponseClassification::ProviderOverloaded
                | ResponseClassification::PassThrough
                | ResponseClassification::Success => {
                    return buffered_upstream_response(
                        buffered,
                        request.metrics,
                        request.request_id,
                    );
                }
            }

            warn!(
                "subscription pool={} account={} unavailable for model={}; trying another account",
                request.pool.pool_id(),
                account_id,
                request.requested_model
            );
            request
                .pool
                .clear_affinity(request.launch_id, request.requested_model, &account_id);
            excluded.insert(account_id.clone());
            break;
        }
    }

    match final_failover_response {
        Some(response) => buffered_upstream_response(response, request.metrics, request.request_id),
        None => Ok(subscription_exhausted_response(
            request.pool.pool_id(),
            request.requested_model,
        )),
    }
}

async fn write_subscription_status(
    request: &SubscriptionForward<'_>,
    account_id: &str,
    reason: SessionAssignmentReason,
) {
    let Some(directory) = request.state.opts.session_status_dir.as_deref() else {
        return;
    };
    let snapshot = match request.pool.session_status(
        request.launch_id,
        request.requested_model,
        account_id,
        reason,
    ) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            warn!(
                "could not build subscription status for pool={} account={}: {}",
                request.pool.pool_id(),
                account_id,
                error
            );
            return;
        }
    };
    write_session_status_snapshot(
        directory,
        request.launch_id,
        &snapshot,
        Arc::clone(&request.state.session_status_generation),
        Arc::clone(&request.state.session_status_io),
    )
    .await;
}

fn subscription_exhausted_response(pool_id: &str, requested_model: &str) -> Response<BoxBody> {
    json_response(
        StatusCode::TOO_MANY_REQUESTS,
        json!({
            "type": "error",
            "error": {
                "type": "rate_limit_error",
                "message": format!(
                    "Claude subscription pool {pool_id:?} has no remaining included allowance for model {requested_model:?}"
                )
            }
        }),
    )
}

async fn write_session_status_snapshot(
    directory: &Path,
    launch_id: &str,
    snapshot: &SessionStatusSnapshot,
    generation: Arc<AtomicU64>,
    io: Arc<AsyncMutex<()>>,
) {
    update_session_status_snapshot(
        directory,
        launch_id,
        SessionStatusUpdate::Assignment(snapshot),
        generation,
        io,
    )
    .await;
}

async fn write_session_route_status(
    directory: &Path,
    launch_id: &str,
    route: &SessionRouteStatus,
    generation: Arc<AtomicU64>,
    io: Arc<AsyncMutex<()>>,
) {
    let destination = directory.join(format!("{}.json", derive_status_id(launch_id)));
    for attempt in 0..SESSION_STATUS_ASSIGNMENT_MAX_ATTEMPTS {
        let assignment_exists = tokio::fs::symlink_metadata(&destination)
            .await
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink());
        if assignment_exists {
            break;
        }
        if attempt + 1 == SESSION_STATUS_ASSIGNMENT_MAX_ATTEMPTS {
            return;
        }
        tokio::time::sleep(SESSION_STATUS_ASSIGNMENT_RETRY_DELAY).await;
    }
    update_session_status_snapshot(
        directory,
        launch_id,
        SessionStatusUpdate::Route(route),
        generation,
        io,
    )
    .await;
}

enum SessionStatusUpdate<'a> {
    Assignment(&'a SessionStatusSnapshot),
    Route(&'a SessionRouteStatus),
}

async fn update_session_status_snapshot(
    directory: &Path,
    launch_id: &str,
    update: SessionStatusUpdate<'_>,
    generation: Arc<AtomicU64>,
    io: Arc<AsyncMutex<()>>,
) {
    let _guard = io.lock().await;
    if !prepare_private_status_directory(directory).await {
        return;
    }
    let sequence = generation.fetch_add(1, Ordering::Relaxed);
    if sequence.is_multiple_of(256) {
        cleanup_stale_session_status(directory).await;
    }

    let status_id = derive_status_id(launch_id);
    let destination = directory.join(format!("{status_id}.json"));
    if tokio::fs::symlink_metadata(&destination)
        .await
        .is_ok_and(|metadata| metadata.file_type().is_symlink() || !metadata.is_file())
    {
        warn!(
            "refusing to replace non-regular subscription status file {}",
            destination.display()
        );
        return;
    }

    let existing = read_session_status_snapshot(&destination).await;
    let snapshot = match update {
        SessionStatusUpdate::Assignment(snapshot) => {
            let mut snapshot = snapshot.clone();
            if let Some(route) = existing.and_then(|existing| existing.route) {
                snapshot.route = Some(route);
            }
            snapshot
        }
        SessionStatusUpdate::Route(route) => {
            let Some(mut snapshot) = existing else {
                return;
            };
            snapshot.route = Some(route.clone());
            snapshot.updated_at_unix = route.updated_at_unix;
            snapshot
        }
    };
    let Ok(serialized) = serde_json::to_vec_pretty(&snapshot) else {
        return;
    };
    if serialized.len() > MAX_SESSION_STATUS_BYTES {
        warn!(
            "subscription status snapshot for pool={} exceeded {} bytes",
            snapshot.pool_id, MAX_SESSION_STATUS_BYTES
        );
        return;
    }

    let temporary = directory.join(format!(
        ".{status_id}.tmp-{}-{sequence}",
        std::process::id()
    ));
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let Ok(mut file) = options.open(&temporary).await else {
        return;
    };
    if file.write_all(&serialized).await.is_err() || file.flush().await.is_err() {
        drop(file);
        let _ = tokio::fs::remove_file(&temporary).await;
        return;
    }
    drop(file);

    #[cfg(unix)]
    if tokio::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
        .await
        .is_err()
    {
        let _ = tokio::fs::remove_file(&temporary).await;
        return;
    }

    if tokio::fs::rename(&temporary, &destination).await.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
}

async fn read_session_status_snapshot(path: &Path) -> Option<SessionStatusSnapshot> {
    let metadata = tokio::fs::symlink_metadata(path).await.ok()?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_SESSION_STATUS_BYTES as u64
    {
        return None;
    }
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut bytes = Vec::new();
    file.take(MAX_SESSION_STATUS_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() > MAX_SESSION_STATUS_BYTES {
        return None;
    }
    serde_json::from_slice(&bytes).ok()
}

async fn prepare_private_status_directory(directory: &Path) -> bool {
    match tokio::fs::symlink_metadata(directory).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            warn!(
                "subscription status path is not a regular directory: {}",
                directory.display()
            );
            return false;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if tokio::fs::create_dir_all(directory).await.is_err() {
                return false;
            }
            let Ok(metadata) = tokio::fs::symlink_metadata(directory).await else {
                return false;
            };
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return false;
            }
        }
        Err(_) => return false,
    }

    #[cfg(unix)]
    if tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
        .await
        .is_err()
    {
        return false;
    }
    true
}

async fn cleanup_stale_session_status(directory: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(directory).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let valid_status_name = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(".json"))
            .is_some_and(rayline_subscriptions::is_valid_status_id);
        if !valid_status_name {
            continue;
        }
        let Ok(metadata) = tokio::fs::symlink_metadata(&path).await else {
            continue;
        };
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age > SESSION_STATUS_RETENTION);
        if stale {
            let _ = tokio::fs::remove_file(path).await;
        }
    }
}

fn is_subscription_auth_header(name: &HeaderName) -> bool {
    name == hyper::header::AUTHORIZATION || name.as_str().eq_ignore_ascii_case("x-api-key")
}

fn subscription_header_snapshot(headers: &reqwest::header::HeaderMap) -> HeaderSnapshot {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str(), value.to_owned()))
        })
        .collect()
}

async fn buffer_subscription_response(response: reqwest::Response) -> Result<BufferedUpstream> {
    use futures::StreamExt as _;

    let status = response.status();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        if body.len().saturating_add(chunk.len()) > MAX_SUBSCRIPTION_ERROR_BYTES {
            return Err(anyhow!(
                "Anthropic subscription error response exceeded {MAX_SUBSCRIPTION_ERROR_BYTES} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(BufferedUpstream {
        status,
        headers,
        body: Bytes::from(body),
    })
}

fn buffered_upstream_response(
    response: BufferedUpstream,
    metrics: Option<SharedMetricsSink>,
    request_id: &str,
) -> Result<Response<BoxBody>> {
    let mut builder = Response::builder().status(response.status);
    for (name, value) in &response.headers {
        if is_hop_by_hop_str(name.as_str()) || name == reqwest::header::CONTENT_LENGTH {
            continue;
        }
        builder = builder.header(name.as_str(), value.as_bytes());
    }
    if let Some(metrics) = metrics {
        metrics.record(MetricsUpdate::RequestErrored {
            request_id: request_id.to_owned(),
            status_code: Some(response.status.as_u16()),
            error: format!(
                "Anthropic subscription request returned HTTP {}",
                response.status.as_u16()
            ),
        });
    }
    builder
        .header(hyper::header::CONTENT_LENGTH, response.body.len())
        .body(full_body(response.body))
        .map_err(Into::into)
}

fn is_unified_limit_header(name: &str) -> bool {
    name.to_ascii_lowercase()
        .starts_with("anthropic-ratelimit-unified-")
}

fn proxy_owns_metrics_for_route(opts: &ProxyOptions, target: RouteTarget) -> bool {
    !opts.local_router_owns_metrics || target != RouteTarget::Router
}

async fn forward_local_redirect(
    state: AppState,
    original_headers: &HeaderMap,
    method: reqwest::Method,
    body: Bytes,
    location: &str,
    request_id: &str,
) -> Result<Response<BoxBody>> {
    let mut outbound = state
        .local_http
        .request(method, location)
        .body(body.to_vec());
    for (name, value) in original_headers.iter() {
        if should_drop_header_for_local_adapter(name) {
            continue;
        }
        outbound = outbound.header(name.as_str(), value.as_bytes());
    }
    outbound = outbound.header(REQUEST_ID_HEADER, request_id);
    let resp = outbound.send().await?;
    let status = resp.status();
    debug!("local redirect {location} -> status={}", status.as_u16());
    response_from_reqwest(resp, status, None, None, None, None, false).await
}

fn stash_router_auth_for_local_redirect(state: &AppState, location: Option<&str>) {
    let Some(cache) = state.opts.auth_cache.as_ref() else {
        return;
    };
    let Some(doc_id) = location.and_then(extract_usage_doc_id) else {
        return;
    };
    let mut auth_headers = HashMap::new();
    auth_headers.insert("x-api-key".to_string(), state.opts.router_api_key.clone());
    stash_auth_headers(cache, doc_id, auth_headers);
}

async fn response_from_reqwest(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    location_override: Option<&str>,
    metrics: Option<SharedMetricsSink>,
    request_id: Option<String>,
    estimated_input_tokens: Option<u64>,
    strip_unified_limits: bool,
) -> Result<Response<BoxBody>> {
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("<none>")
        .to_owned();
    let content_encoding = resp
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let mut headers_out = HeaderMap::new();
    for (k, v) in resp.headers().iter() {
        if is_hop_by_hop_str(k.as_str())
            || (strip_unified_limits && is_unified_limit_header(k.as_str()))
        {
            continue;
        }
        if k == reqwest::header::LOCATION {
            if let Some(location) = location_override {
                let value = HeaderValue::from_str(location)?;
                headers_out.insert(hyper::header::LOCATION, value);
                continue;
            }
        }
        let name = match HeaderName::from_bytes(k.as_str().as_bytes()) {
            Ok(n) => n,
            Err(_) => continue,
        };
        let value = match HeaderValue::from_bytes(v.as_bytes()) {
            Ok(v) => v,
            Err(_) => continue,
        };
        headers_out.append(name, value);
    }
    let sse_capture = sse_capture_dir().and_then(|dir| {
        request_id
            .as_deref()
            .map(|request_id| SseCapture::new(dir, request_id))
    });
    let has_metrics = metrics.is_some() && request_id.is_some();
    let observe_response = has_metrics || sse_capture.is_some();

    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();

    tokio::spawn(async move {
        use futures::StreamExt;
        let mut s = resp.bytes_stream();
        let mut body = Vec::new();
        let mut sse_buffer = String::new();
        let mut sse_capture_buffer = String::new();
        let mut input_tokens = estimated_input_tokens;
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        let mut prompt_processed_tokens = None;
        let parse_chunks_live = content_encoding_is_identity(content_encoding.as_deref());
        let mut saw_first = false;
        let mut stream_error: Option<String> = None;
        if has_metrics {
            record_remote_usage(
                metrics.as_ref(),
                request_id.as_deref(),
                input_tokens,
                output_tokens,
                prompt_cache_tokens,
                prompt_processed_tokens,
            );
        }
        let decoded_metrics_tx = (has_metrics && !parse_chunks_live)
            .then(|| {
                spawn_decoded_metrics_observer(
                    content_encoding.clone(),
                    metrics.clone(),
                    request_id.clone(),
                    estimated_input_tokens,
                )
            })
            .flatten();
        while let Some(chunk) = s.next().await {
            match chunk {
                Ok(b) => {
                    if observe_response {
                        if !saw_first {
                            saw_first = true;
                            if let (Some(metrics), Some(request_id)) =
                                (metrics.as_ref(), request_id.as_ref())
                            {
                                metrics.record(MetricsUpdate::FirstToken {
                                    request_id: request_id.clone(),
                                });
                            }
                        }
                        let previous_input_tokens = input_tokens;
                        let previous_output_tokens = output_tokens;
                        let previous_prompt_cache_tokens = prompt_cache_tokens;
                        let previous_prompt_processed_tokens = prompt_processed_tokens;
                        if parse_chunks_live {
                            observe_anthropic_sse_chunk(
                                &b,
                                &mut sse_buffer,
                                &mut input_tokens,
                                &mut output_tokens,
                                &mut prompt_cache_tokens,
                                &mut prompt_processed_tokens,
                            );
                            if let Some(capture) = sse_capture.as_ref() {
                                capture_anthropic_sse_chunk(&b, &mut sse_capture_buffer, capture);
                            }
                        }
                        if input_tokens != previous_input_tokens
                            || output_tokens != previous_output_tokens
                            || prompt_cache_tokens != previous_prompt_cache_tokens
                            || prompt_processed_tokens != previous_prompt_processed_tokens
                        {
                            record_remote_usage(
                                metrics.as_ref(),
                                request_id.as_deref(),
                                input_tokens,
                                output_tokens,
                                prompt_cache_tokens,
                                prompt_processed_tokens,
                            );
                        }
                        body.extend_from_slice(&b);
                        if let Some(tx) = decoded_metrics_tx.as_ref() {
                            let _ = tx.try_send(b.clone());
                        }
                    }
                    if tx.send(Ok(Frame::data(b))).await.is_err() {
                        stream_error.get_or_insert_with(|| "downstream disconnected".to_owned());
                        break;
                    }
                }
                Err(e) => {
                    let error = e.to_string();
                    stream_error = Some(error.clone());
                    let _ = tx.send(Err(std::io::Error::other(error))).await;
                    break;
                }
            }
        }
        if observe_response && parse_chunks_live {
            let previous_input_tokens = input_tokens;
            let previous_output_tokens = output_tokens;
            let previous_prompt_cache_tokens = prompt_cache_tokens;
            let previous_prompt_processed_tokens = prompt_processed_tokens;
            observe_anthropic_sse_chunk(
                b"\n\n",
                &mut sse_buffer,
                &mut input_tokens,
                &mut output_tokens,
                &mut prompt_cache_tokens,
                &mut prompt_processed_tokens,
            );
            if let Some(capture) = sse_capture.as_ref() {
                capture_anthropic_sse_chunk(b"\n\n", &mut sse_capture_buffer, capture);
            }
            if input_tokens != previous_input_tokens
                || output_tokens != previous_output_tokens
                || prompt_cache_tokens != previous_prompt_cache_tokens
                || prompt_processed_tokens != previous_prompt_processed_tokens
            {
                record_remote_usage(
                    metrics.as_ref(),
                    request_id.as_deref(),
                    input_tokens,
                    output_tokens,
                    prompt_cache_tokens,
                    prompt_processed_tokens,
                );
            }
        }
        if observe_response {
            let decoded_body = decode_body_for_metrics(&body, content_encoding.as_deref());
            if !parse_chunks_live {
                if let (Some(capture), Some(decoded)) =
                    (sse_capture.as_ref(), decoded_body.as_deref())
                {
                    let mut decoded_capture_buffer = String::new();
                    capture_anthropic_sse_chunk(decoded, &mut decoded_capture_buffer, capture);
                    capture_anthropic_sse_chunk(b"\n\n", &mut decoded_capture_buffer, capture);
                }
            }
            let previous_input_tokens = input_tokens;
            let previous_output_tokens = output_tokens;
            let previous_prompt_cache_tokens = prompt_cache_tokens;
            let previous_prompt_processed_tokens = prompt_processed_tokens;
            if let Some(decoded) = decoded_body.as_deref() {
                usage_from_anthropic_body(decoded).merge_into(
                    &mut input_tokens,
                    &mut output_tokens,
                    &mut prompt_cache_tokens,
                    &mut prompt_processed_tokens,
                );
            }
            if let Some(capture) = sse_capture.as_ref() {
                let raw_path = capture.raw_path();
                if let Err(error) = write_sse_capture_raw(&raw_path, &body) {
                    warn!(
                        "failed to write raw SSE capture {}: {error}",
                        raw_path.display()
                    );
                }
            }
            if input_tokens != previous_input_tokens
                || output_tokens != previous_output_tokens
                || prompt_cache_tokens != previous_prompt_cache_tokens
                || prompt_processed_tokens != previous_prompt_processed_tokens
            {
                record_remote_usage(
                    metrics.as_ref(),
                    request_id.as_deref(),
                    input_tokens,
                    output_tokens,
                    prompt_cache_tokens,
                    prompt_processed_tokens,
                );
            }
            if let (Some(metrics), Some(request_id)) = (metrics.as_ref(), request_id.as_ref()) {
                if let Some(error) = stream_error {
                    metrics.record(MetricsUpdate::RequestErrored {
                        request_id: request_id.clone(),
                        status_code: Some(status.as_u16()),
                        error,
                    });
                } else if !status.is_success() {
                    metrics.record(MetricsUpdate::RequestErrored {
                        request_id: request_id.clone(),
                        status_code: Some(status.as_u16()),
                        error: format!("upstream returned HTTP {}", status.as_u16()),
                    });
                } else {
                    if output_tokens.is_none() && estimated_input_tokens.unwrap_or(0) > 1 {
                        info!(
                            "proxy metrics completed without output usage request_id={} status={} input_tokens={} body_bytes={} content_type={} trailing_sse_bytes={}",
                            request_id,
                            status.as_u16(),
                            display_optional_u64(input_tokens),
                            body.len(),
                            content_type,
                            if parse_chunks_live {
                                sse_buffer.len()
                            } else {
                                decoded_body.as_deref().map(|b| b.len()).unwrap_or(0)
                            }
                        );
                    }
                    metrics.record(MetricsUpdate::RequestCompleted {
                        request_id: request_id.clone(),
                        status_code: Some(status.as_u16()),
                        input_tokens,
                        output_tokens,
                        selected_model: None,
                    });
                }
            }
        }
    });

    let mut builder = Response::builder().status(status.as_u16());
    if let Some(headers) = builder.headers_mut() {
        *headers = headers_out;
    }
    Ok(builder.body(body_out).unwrap())
}

fn observe_anthropic_sse_chunk(
    bytes: &[u8],
    buffer: &mut String,
    input_tokens: &mut Option<u64>,
    output_tokens: &mut Option<u64>,
    prompt_cache_tokens: &mut Option<u64>,
    prompt_processed_tokens: &mut Option<u64>,
) {
    buffer.push_str(&String::from_utf8_lossy(bytes));
    while let Some(event) = drain_sse_event(buffer) {
        for line in event.lines() {
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let payload = payload.trim();
            if payload.is_empty() || payload == "[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_str::<Value>(payload) else {
                continue;
            };
            usage_from_value(&value).merge_into(
                input_tokens,
                output_tokens,
                prompt_cache_tokens,
                prompt_processed_tokens,
            );
        }
    }
}

struct SseCapture {
    path: PathBuf,
}

impl SseCapture {
    fn new(dir: PathBuf, request_id: &str) -> Self {
        let safe_request_id: String = request_id
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        Self {
            path: dir.join(format!("{safe_request_id}.jsonl")),
        }
    }

    fn raw_path(&self) -> PathBuf {
        self.path.with_extension("raw.sse")
    }
}

fn sse_capture_dir() -> Option<PathBuf> {
    env::var_os("RAYLINE_DEBUG_SSE_CAPTURE_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn capture_anthropic_sse_chunk(bytes: &[u8], buffer: &mut String, capture: &SseCapture) {
    buffer.push_str(&String::from_utf8_lossy(bytes));
    while let Some(event) = drain_sse_event(buffer) {
        capture_anthropic_sse_event(&event, capture);
    }
}

fn capture_anthropic_sse_event(event: &str, capture: &SseCapture) {
    if event.trim().is_empty() {
        return;
    }
    let mut event_name = None;
    let mut payloads = Vec::new();
    for line in event.lines() {
        if let Some(value) = line.strip_prefix("event:") {
            event_name = Some(value.trim().to_owned());
            continue;
        }
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            payloads.push(json!({"done": payload == "[DONE]"}));
            continue;
        }
        match serde_json::from_str::<Value>(payload) {
            Ok(value) => payloads.push(sanitize_sse_payload(&value)),
            Err(error) => payloads.push(json!({
                "parse_error": error.to_string(),
                "bytes": payload.len(),
            })),
        }
    }
    let record = json!({
        "event": event_name,
        "payloads": payloads,
    });
    if let Err(error) = append_sse_capture_record(&capture.path, &record) {
        warn!(
            "failed to write sanitized SSE capture {}: {error}",
            capture.path.display()
        );
    }
}

fn append_sse_capture_record(path: &Path, record: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(file, "{record}")
}

fn write_sse_capture_raw(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, body)
}

fn sanitize_sse_payload(value: &Value) -> Value {
    let mut usage_objects = Vec::new();
    collect_usage_objects(value, &mut usage_objects);
    json!({
        "type": value.get("type").and_then(Value::as_str),
        "top_keys": object_keys(value),
        "message": sanitize_sse_message(value.get("message")),
        "delta": sanitize_sse_delta(value.get("delta")),
        "usage_objects": usage_objects,
    })
}

fn sanitize_sse_message(value: Option<&Value>) -> Value {
    let Some(Value::Object(map)) = value else {
        return Value::Null;
    };
    json!({
        "id": map.get("id").and_then(Value::as_str),
        "model": map.get("model").and_then(Value::as_str),
        "role": map.get("role").and_then(Value::as_str),
        "stop_reason": map.get("stop_reason").and_then(Value::as_str),
        "keys": map.keys().cloned().collect::<Vec<_>>(),
        "content_types": map.get("content").and_then(Value::as_array).map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("type").and_then(Value::as_str))
                .collect::<Vec<_>>()
        }),
    })
}

fn sanitize_sse_delta(value: Option<&Value>) -> Value {
    let Some(Value::Object(map)) = value else {
        return Value::Null;
    };
    json!({
        "type": map.get("type").and_then(Value::as_str),
        "stop_reason": map.get("stop_reason").and_then(Value::as_str),
        "keys": map.keys().cloned().collect::<Vec<_>>(),
        "text_bytes": map
            .get("text")
            .and_then(Value::as_str)
            .map(str::len),
        "thinking_bytes": map
            .get("thinking")
            .and_then(Value::as_str)
            .map(str::len),
        "partial_json_bytes": map
            .get("partial_json")
            .and_then(Value::as_str)
            .map(str::len),
    })
}

fn collect_usage_objects(value: &Value, usage_objects: &mut Vec<Value>) {
    match value {
        Value::Object(map) => {
            if map.keys().any(|key| key.contains("tokens")) {
                usage_objects.push(Value::Object(map.clone()));
            }
            for child in map.values() {
                collect_usage_objects(child, usage_objects);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_usage_objects(item, usage_objects);
            }
        }
        _ => {}
    }
}

fn object_keys(value: &Value) -> Vec<String> {
    value
        .as_object()
        .map(|map| map.keys().cloned().collect())
        .unwrap_or_default()
}

fn drain_sse_event(buffer: &mut String) -> Option<String> {
    let lf = buffer.find("\n\n").map(|idx| (idx, 2));
    let crlf = buffer.find("\r\n\r\n").map(|idx| (idx, 4));
    let (idx, delimiter_len) = match (lf, crlf) {
        (Some(left), Some(right)) => {
            if left.0 <= right.0 {
                left
            } else {
                right
            }
        }
        (Some(value), None) | (None, Some(value)) => value,
        (None, None) => return None,
    };
    let event = buffer[..idx].to_owned();
    buffer.drain(..idx + delimiter_len);
    Some(event)
}

fn record_remote_usage(
    metrics: Option<&SharedMetricsSink>,
    request_id: Option<&str>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    prompt_cache_tokens: Option<u64>,
    prompt_processed_tokens: Option<u64>,
) {
    let (Some(metrics), Some(request_id)) = (metrics, request_id) else {
        return;
    };
    let request_id = request_id.to_owned();
    metrics.record(MetricsUpdate::TokenUsage {
        request_id: request_id.clone(),
        input_tokens,
        output_tokens,
        selected_model: None,
    });
    if input_tokens.is_some() || prompt_cache_tokens.is_some() || prompt_processed_tokens.is_some()
    {
        metrics.record(MetricsUpdate::PromptCache {
            request_id,
            prompt_tokens: input_tokens,
            cache_tokens: prompt_cache_tokens,
            processed_tokens: prompt_processed_tokens,
            prompt_ms: None,
            prompt_tps: None,
        });
    }
}

fn spawn_decoded_metrics_observer(
    content_encoding: Option<String>,
    metrics: Option<SharedMetricsSink>,
    request_id: Option<String>,
    estimated_input_tokens: Option<u64>,
) -> Option<mpsc::Sender<Bytes>> {
    let (Some(metrics), Some(request_id)) = (metrics, request_id) else {
        return None;
    };
    let encodings = normalized_content_encodings(content_encoding.as_deref());
    if encodings.len() != 1 {
        return None;
    }
    let encoding = encodings.into_iter().next()?;
    if encoding == "identity" {
        return None;
    }
    let (tx, rx) = mpsc::channel::<Bytes>(64);
    tokio::spawn(async move {
        if let Err(error) = observe_decoded_metrics_stream(
            rx,
            &encoding,
            metrics,
            request_id,
            estimated_input_tokens,
        )
        .await
        {
            debug!("compressed SSE metrics observer stopped: {error}");
        }
    });
    Some(tx)
}

async fn observe_decoded_metrics_stream(
    rx: mpsc::Receiver<Bytes>,
    encoding: &str,
    metrics: SharedMetricsSink,
    request_id: String,
    estimated_input_tokens: Option<u64>,
) -> Result<()> {
    use futures::StreamExt;

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(Ok::<Bytes, std::io::Error>);
    let reader = StreamReader::new(stream);
    let reader = tokio::io::BufReader::new(reader);
    match encoding {
        "gzip" | "x-gzip" => {
            observe_metrics_from_async_reader(
                async_compression::tokio::bufread::GzipDecoder::new(reader),
                metrics,
                request_id,
                estimated_input_tokens,
            )
            .await
        }
        "br" => {
            observe_metrics_from_async_reader(
                async_compression::tokio::bufread::BrotliDecoder::new(reader),
                metrics,
                request_id,
                estimated_input_tokens,
            )
            .await
        }
        "zstd" => {
            observe_metrics_from_async_reader(
                async_compression::tokio::bufread::ZstdDecoder::new(reader),
                metrics,
                request_id,
                estimated_input_tokens,
            )
            .await
        }
        "deflate" => {
            observe_metrics_from_async_reader(
                async_compression::tokio::bufread::DeflateDecoder::new(reader),
                metrics,
                request_id,
                estimated_input_tokens,
            )
            .await
        }
        other => Err(anyhow!("unsupported streaming content-encoding {other:?}")),
    }
}

async fn observe_metrics_from_async_reader(
    mut reader: impl AsyncRead + Unpin,
    metrics: SharedMetricsSink,
    request_id: String,
    estimated_input_tokens: Option<u64>,
) -> Result<()> {
    let mut sse_buffer = String::new();
    let mut input_tokens = estimated_input_tokens;
    let mut output_tokens = None;
    let mut prompt_cache_tokens = None;
    let mut prompt_processed_tokens = None;
    let mut buf = [0_u8; 8192];
    loop {
        let read = reader.read(&mut buf).await?;
        if read == 0 {
            break;
        }
        let previous_input_tokens = input_tokens;
        let previous_output_tokens = output_tokens;
        let previous_prompt_cache_tokens = prompt_cache_tokens;
        let previous_prompt_processed_tokens = prompt_processed_tokens;
        observe_anthropic_sse_chunk(
            &buf[..read],
            &mut sse_buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
            &mut prompt_processed_tokens,
        );
        if input_tokens != previous_input_tokens
            || output_tokens != previous_output_tokens
            || prompt_cache_tokens != previous_prompt_cache_tokens
            || prompt_processed_tokens != previous_prompt_processed_tokens
        {
            record_remote_usage(
                Some(&metrics),
                Some(&request_id),
                input_tokens,
                output_tokens,
                prompt_cache_tokens,
                prompt_processed_tokens,
            );
        }
    }
    let previous_input_tokens = input_tokens;
    let previous_output_tokens = output_tokens;
    let previous_prompt_cache_tokens = prompt_cache_tokens;
    let previous_prompt_processed_tokens = prompt_processed_tokens;
    observe_anthropic_sse_chunk(
        b"\n\n",
        &mut sse_buffer,
        &mut input_tokens,
        &mut output_tokens,
        &mut prompt_cache_tokens,
        &mut prompt_processed_tokens,
    );
    if input_tokens != previous_input_tokens
        || output_tokens != previous_output_tokens
        || prompt_cache_tokens != previous_prompt_cache_tokens
        || prompt_processed_tokens != previous_prompt_processed_tokens
    {
        record_remote_usage(
            Some(&metrics),
            Some(&request_id),
            input_tokens,
            output_tokens,
            prompt_cache_tokens,
            prompt_processed_tokens,
        );
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ObservedUsage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    prompt_cache_tokens: Option<u64>,
    prompt_processed_tokens: Option<u64>,
}

impl ObservedUsage {
    fn merge_into(
        self,
        input_tokens: &mut Option<u64>,
        output_tokens: &mut Option<u64>,
        prompt_cache_tokens: &mut Option<u64>,
        prompt_processed_tokens: &mut Option<u64>,
    ) {
        if self.input_tokens.is_some() {
            *input_tokens = self.input_tokens;
        }
        if self.output_tokens.is_some() {
            *output_tokens = self.output_tokens;
        }
        if self.prompt_cache_tokens.is_some() {
            *prompt_cache_tokens = self.prompt_cache_tokens;
        }
        if self.prompt_processed_tokens.is_some() {
            *prompt_processed_tokens = self.prompt_processed_tokens;
        }
    }
}

fn usage_from_anthropic_body(body: &[u8]) -> ObservedUsage {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return usage_from_value(&value);
    }
    let mut usage = ObservedUsage::default();
    for line in String::from_utf8_lossy(body).lines() {
        let Some(payload) = line.trim_start().strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(payload) {
            usage_from_value(&value).merge_into(
                &mut usage.input_tokens,
                &mut usage.output_tokens,
                &mut usage.prompt_cache_tokens,
                &mut usage.prompt_processed_tokens,
            );
        }
    }
    usage
}

fn usage_from_value(value: &Value) -> ObservedUsage {
    let mut usage = ObservedUsage::default();
    collect_usage_from_value(value, &mut usage);
    usage
}

fn collect_usage_from_value(value: &Value, usage: &mut ObservedUsage) {
    match value {
        Value::Object(map) => {
            if let Some(input) = total_input_tokens_from_object(map) {
                usage.input_tokens = Some(input);
                if let Some(cache) = cache_read_tokens_from_object(map) {
                    usage.prompt_cache_tokens = Some(cache);
                    usage.prompt_processed_tokens = Some(input.saturating_sub(cache));
                }
            }
            if let Some(output) = map
                .get("output_tokens")
                .or_else(|| map.get("completion_tokens"))
                .and_then(Value::as_u64)
            {
                usage.output_tokens = Some(output);
            }
            for child in map.values() {
                collect_usage_from_value(child, usage);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_usage_from_value(item, usage);
            }
        }
        _ => {}
    }
}

fn total_input_tokens_from_object(map: &serde_json::Map<String, Value>) -> Option<u64> {
    let anthropic_total = token_field(map, "input_tokens")
        .saturating_add(token_field(map, "cache_creation_input_tokens"))
        .saturating_add(token_field(map, "cache_read_input_tokens"));
    if anthropic_total > 0 {
        return Some(anthropic_total);
    }
    map.get("prompt_tokens").and_then(Value::as_u64)
}

fn cache_read_tokens_from_object(map: &serde_json::Map<String, Value>) -> Option<u64> {
    if let Some(tokens) = map.get("cache_read_input_tokens").and_then(Value::as_u64) {
        return Some(tokens);
    }
    map.get("prompt_tokens_details")
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64)
}

fn token_field(map: &serde_json::Map<String, Value>, key: &str) -> u64 {
    map.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn display_optional_u64(value: Option<u64>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "-".to_owned())
}

fn content_encoding_is_identity(content_encoding: Option<&str>) -> bool {
    normalized_content_encodings(content_encoding)
        .iter()
        .all(|encoding| encoding == "identity")
}

fn normalized_content_encodings(content_encoding: Option<&str>) -> Vec<String> {
    content_encoding
        .unwrap_or("")
        .split(',')
        .filter_map(|value| {
            let value = value.trim().to_ascii_lowercase();
            (!value.is_empty()).then_some(value)
        })
        .collect::<Vec<_>>()
}

/// Maximum number of bytes allowed when decompressing an upstream response body
/// for metrics extraction. Prevents a gzip/brotli/zstd bomb from an upstream
/// server (buggy, compromised, or MITM'd) from OOM-ing the daemon.
/// On exceeding this cap, metrics are silently skipped for that request only;
/// the raw downstream stream is never affected.
const MAX_DECODE_BYTES: u64 = 32 * 1024 * 1024; // 32 MiB

/// Decompress `body` according to `content_encoding` for metrics extraction.
///
/// Returns `Some(decoded)` when decompression succeeds within `MAX_DECODE_BYTES`.
/// Returns `None` when:
/// - The decoded output would exceed `MAX_DECODE_BYTES` (cap exceeded; emits one `warn!`).
/// - The encoding is unsupported or decompression fails (emits one `warn!`).
///
/// Returning `None` means "skip metrics for this request" — never an error to the caller.
fn decode_body_for_metrics(body: &[u8], content_encoding: Option<&str>) -> Option<Vec<u8>> {
    let encodings = normalized_content_encodings(content_encoding);
    if encodings.is_empty() || encodings.iter().all(|encoding| encoding == "identity") {
        return Some(body.to_vec());
    }

    let mut decoded = body.to_vec();
    for encoding in encodings.iter().rev() {
        decoded = match encoding.as_str() {
            "identity" => decoded,
            "gzip" | "x-gzip" => {
                match read_bounded(
                    flate2::read::GzDecoder::new(Cursor::new(decoded)),
                    MAX_DECODE_BYTES,
                ) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        warn!(
                            "upstream gzip response body exceeded MAX_DECODE_BYTES ({} MiB); \
                             skipping metrics for this request",
                            MAX_DECODE_BYTES / (1024 * 1024)
                        );
                        return None;
                    }
                    Err(error) => {
                        warn!("failed to decode gzip response body for metrics: {error}");
                        return None;
                    }
                }
            }
            "deflate" => decode_deflate_body(decoded)?,
            "br" => {
                match read_bounded(
                    brotli::Decompressor::new(Cursor::new(decoded), 4096),
                    MAX_DECODE_BYTES,
                ) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        warn!(
                            "upstream brotli response body exceeded MAX_DECODE_BYTES ({} MiB); \
                             skipping metrics for this request",
                            MAX_DECODE_BYTES / (1024 * 1024)
                        );
                        return None;
                    }
                    Err(error) => {
                        warn!("failed to decode brotli response body for metrics: {error}");
                        return None;
                    }
                }
            }
            "zstd" => {
                let decoder = match zstd::stream::read::Decoder::new(Cursor::new(decoded)) {
                    Ok(d) => d,
                    Err(error) => {
                        warn!("failed to initialize zstd decoder for metrics: {error}");
                        return None;
                    }
                };
                match read_bounded(decoder, MAX_DECODE_BYTES) {
                    Ok(Some(bytes)) => bytes,
                    Ok(None) => {
                        warn!(
                            "upstream zstd response body exceeded MAX_DECODE_BYTES ({} MiB); \
                             skipping metrics for this request",
                            MAX_DECODE_BYTES / (1024 * 1024)
                        );
                        return None;
                    }
                    Err(error) => {
                        warn!("failed to decode zstd response body for metrics: {error}");
                        return None;
                    }
                }
            }
            other => {
                warn!(
                    "unsupported content-encoding {other:?} for metrics decode; skipping metrics"
                );
                return None;
            }
        };
    }
    Some(decoded)
}

fn decode_deflate_body(body: Vec<u8>) -> Option<Vec<u8>> {
    match read_bounded(
        flate2::read::ZlibDecoder::new(Cursor::new(body.clone())),
        MAX_DECODE_BYTES,
    ) {
        Ok(Some(decoded)) => Some(decoded),
        Ok(None) => {
            warn!(
                "upstream deflate (zlib) response body exceeded MAX_DECODE_BYTES ({} MiB); \
                 skipping metrics for this request",
                MAX_DECODE_BYTES / (1024 * 1024)
            );
            None
        }
        Err(zlib_error) => {
            match read_bounded(
                flate2::read::DeflateDecoder::new(Cursor::new(body)),
                MAX_DECODE_BYTES,
            ) {
                Ok(Some(decoded)) => Some(decoded),
                Ok(None) => {
                    warn!(
                        "upstream deflate response body exceeded MAX_DECODE_BYTES ({} MiB); \
                         skipping metrics for this request",
                        MAX_DECODE_BYTES / (1024 * 1024)
                    );
                    None
                }
                Err(error) => {
                    warn!(
                        "failed to decode deflate response body for metrics \
                         (zlib failed: {zlib_error}): {error}"
                    );
                    None
                }
            }
        }
    }
}

/// Read from `reader` up to `limit` bytes.
///
/// Returns:
/// - `Ok(Some(bytes))` when the reader reaches EOF before `limit` bytes.
/// - `Ok(None)` when more than `limit` bytes would be produced (cap exceeded).
/// - `Err(e)` on an I/O error.
fn read_bounded(reader: impl Read, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    // Read up to limit+1 bytes so we can detect if the output exceeds the cap.
    let read = reader.take(limit + 1).read_to_end(&mut output)?;
    if read as u64 > limit {
        Ok(None)
    } else {
        Ok(Some(output))
    }
}

/// The router's per-turn decision, captured from the `X-Rayline-*` response
/// headers and persisted to the sidecar file the Rayline status-line reader
/// renders. Lives entirely in the proxy; no cache-prefix concerns apply here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteStatus {
    pub selected_model: String,
    pub virtual_model: Option<String>,
    pub policy: Option<String>,
    pub task_class: Option<String>,
    pub route_id: Option<String>,
}

impl RouteStatus {
    /// Extract the decision from upstream response headers. Returns `None` when
    /// `X-Rayline-Selected-Model` is absent (e.g. pass-through Anthropic
    /// responses), which signals "no sidecar update for this response".
    ///
    /// When the router redirects to the on-device model the selected-model
    /// header is the literal `"local"`; map it to the configured local model id
    /// so the status line shows the real model name instead of `local`.
    fn from_headers(
        headers: &reqwest::header::HeaderMap,
        local_model_id: Option<&str>,
    ) -> Option<Self> {
        let get = |name: &str| {
            headers
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .filter(|s| !s.is_empty())
        };
        let decision_header = |suffix: &str| get(&format!("x-rayline-{suffix}"));
        let raw_selected = decision_header("selected-model")?;
        let selected_model = match (raw_selected.as_str(), local_model_id) {
            ("local", Some(id)) if !id.is_empty() => id.to_string(),
            _ => raw_selected,
        };
        Some(Self {
            selected_model,
            virtual_model: decision_header("virtual-model"),
            policy: decision_header("policy"),
            task_class: decision_header("task-class"),
            route_id: decision_header("route-id"),
        })
    }

    fn to_json(&self) -> Value {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        json!({
            "selected_model": self.selected_model,
            "virtual_model": self.virtual_model,
            "policy": self.policy,
            "task_class": self.task_class,
            "route_id": self.route_id,
            "ts": ts,
        })
    }

    fn to_session_status(&self) -> SessionRouteStatus {
        SessionRouteStatus {
            selected_model: self.selected_model.clone(),
            virtual_model: self.virtual_model.clone(),
            policy: self.policy.clone(),
            task_class: self.task_class.clone(),
            route_id: self.route_id.clone(),
            updated_at_unix: SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs() as i64)
                .unwrap_or(0),
        }
    }

    /// Atomically write the status to `path` (temp file + rename). Best-effort:
    /// any IO error is swallowed so a sidecar problem never affects the proxied
    /// request. The temp filename is keyed by route id (and falls back to the
    /// process id) so concurrent responses do not clobber each other's temp.
    async fn write_to_if_current(
        &self,
        path: &Path,
        generation: Arc<AtomicU64>,
        io: Arc<AsyncMutex<()>>,
        expected_generation: u64,
    ) {
        let _guard = io.lock().await;
        if generation.load(Ordering::SeqCst) != expected_generation {
            return;
        }
        self.write_serialized(path).await;
    }

    async fn write_serialized(&self, path: &Path) {
        let Ok(serialized) = serde_json::to_vec_pretty(&self.to_json()) else {
            return;
        };
        let suffix = self
            .route_id
            .clone()
            .unwrap_or_else(|| std::process::id().to_string());
        let tmp = path.with_extension(format!("tmp.{suffix}"));
        if let Some(parent) = path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        if tokio::fs::write(&tmp, &serialized).await.is_ok() {
            if tokio::fs::rename(&tmp, path).await.is_err() {
                let _ = tokio::fs::remove_file(&tmp).await;
            }
        } else {
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }

    async fn clear(path: &Path) {
        let _ = tokio::fs::remove_file(path).await;
    }
}

pub fn classify_connect_authority(authority: &str) -> RouteDecision {
    if is_anthropic_authority(authority) {
        RouteDecision {
            target: RouteTarget::Anthropic,
            reason: "api_anthropic_intercept",
        }
    } else {
        RouteDecision {
            target: RouteTarget::BlindTunnel,
            reason: "non_anthropic_connect",
        }
    }
}

pub fn classify_anthropic_request(method: &Method, path: &str) -> RouteDecision {
    classify_anthropic_request_for_mode(method, path, ProxyRoutingMode::All)
}

pub fn classify_anthropic_request_for_mode(
    method: &Method,
    path: &str,
    routing_mode: ProxyRoutingMode,
) -> RouteDecision {
    if is_router_routed_path(method, path, routing_mode) {
        RouteDecision {
            target: RouteTarget::Router,
            reason: "router_routed_path",
        }
    } else {
        RouteDecision {
            target: RouteTarget::Anthropic,
            reason: "anthropic_passthrough",
        }
    }
}

struct PreparedAnthropicRoute {
    decision: RouteDecision,
    body: Bytes,
    body_was_rewritten: bool,
    agent_id: Option<String>,
    agent_type: Option<String>,
}

#[cfg(test)]
fn prepare_anthropic_route(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
    routing_mode: ProxyRoutingMode,
) -> PreparedAnthropicRoute {
    prepare_anthropic_route_with_subagent_filter(
        method,
        path,
        headers,
        body,
        routing_mode,
        &[],
        None,
    )
}

fn prepare_anthropic_route_with_subagent_filter(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: Bytes,
    routing_mode: ProxyRoutingMode,
    selective_subagent_ids: &[String],
    resolved_agent_type: Option<&str>,
) -> PreparedAnthropicRoute {
    let agent_id = claude_code_agent_id(headers).map(ToOwned::to_owned);
    let agent_type = resolved_agent_type.map(ToOwned::to_owned);

    if routing_mode == ProxyRoutingMode::All {
        return prepared_anthropic_route(
            classify_anthropic_request_for_mode(method, path, routing_mode),
            body,
            false,
            agent_id,
            agent_type,
        );
    }

    if !is_selective_routable_method_path(method, path) {
        return anthropic_passthrough(body, "selective_passthrough_path", agent_id, agent_type);
    }

    if *method == Method::GET && is_virtual_model_lookup(path) {
        return prepared_anthropic_route(
            RouteDecision {
                target: RouteTarget::Router,
                reason: "selective_virtual_model_lookup",
            },
            body,
            false,
            agent_id,
            agent_type,
        );
    }

    if *method == Method::GET && is_provider_model_lookup(path) {
        return prepared_anthropic_route(
            RouteDecision {
                target: RouteTarget::Router,
                reason: "selective_provider_model_lookup",
            },
            body,
            false,
            agent_id,
            agent_type,
        );
    }

    if *method == Method::GET && is_model_list_path(path) {
        return prepared_anthropic_route(
            RouteDecision {
                target: RouteTarget::Router,
                reason: "selective_model_list",
            },
            body,
            false,
            agent_id,
            agent_type,
        );
    }

    let body_model = request_body_model(&body);
    if is_virtual_model(body_model.as_deref()) {
        return prepared_anthropic_route(
            RouteDecision {
                target: RouteTarget::Router,
                reason: "selective_virtual_model",
            },
            body,
            false,
            agent_id,
            agent_type,
        );
    }

    if path == "/v1/messages" {
        let Some(agent_id_value) = agent_id.as_deref() else {
            return anthropic_passthrough(body, "selective_main_passthrough", agent_id, agent_type);
        };
        if !subagent_filter_allows(
            agent_id_value,
            agent_type.as_deref(),
            selective_subagent_ids,
        ) {
            return anthropic_passthrough(
                body,
                "selective_subagent_passthrough",
                agent_id,
                agent_type,
            );
        }
        return prepared_anthropic_route(
            RouteDecision {
                target: RouteTarget::Router,
                reason: "selective_subagent_header",
            },
            body,
            false,
            agent_id,
            agent_type,
        );
    }

    anthropic_passthrough(body, "selective_main_passthrough", agent_id, agent_type)
}

fn prepared_anthropic_route(
    decision: RouteDecision,
    body: Bytes,
    body_was_rewritten: bool,
    agent_id: Option<String>,
    agent_type: Option<String>,
) -> PreparedAnthropicRoute {
    PreparedAnthropicRoute {
        decision,
        body,
        body_was_rewritten,
        agent_id,
        agent_type,
    }
}

fn anthropic_passthrough(
    body: Bytes,
    reason: &'static str,
    agent_id: Option<String>,
    agent_type: Option<String>,
) -> PreparedAnthropicRoute {
    prepared_anthropic_route(
        RouteDecision {
            target: RouteTarget::Anthropic,
            reason,
        },
        body,
        false,
        agent_id,
        agent_type,
    )
}

fn proxied_sideband_endpoint_id(decision: &RouteDecision, path_and_query: &str) -> Option<String> {
    (decision.target == RouteTarget::Anthropic && decision.reason == "selective_passthrough_path")
        .then(|| path_and_query.to_owned())
}

fn is_selective_routable_method_path(method: &Method, path: &str) -> bool {
    match (method, path) {
        (&Method::POST, "/v1/messages" | "/v1/messages/count_tokens") => true,
        (&Method::GET, p) if is_model_list_path(p) => true,
        (&Method::GET, p) => p
            .strip_prefix("/v1/models/")
            .is_some_and(|model_id| !model_id.is_empty()),
        _ => false,
    }
}

fn is_router_routed_path(method: &Method, path: &str, routing_mode: ProxyRoutingMode) -> bool {
    match routing_mode {
        ProxyRoutingMode::All => match (method, path) {
            (&Method::POST, "/v1/messages" | "/v1/messages/count_tokens") => true,
            (&Method::GET, p) if is_model_list_path(p) => true,
            (&Method::GET, p) => p
                .strip_prefix("/v1/models/")
                .is_some_and(|model_id| !model_id.is_empty()),
            _ => false,
        },
        ProxyRoutingMode::SelectiveSubagents => match (method, path) {
            (&Method::GET, p) if is_model_list_path(p) => true,
            (&Method::GET, p) => is_virtual_model_lookup(p) || is_provider_model_lookup(p),
            _ => false,
        },
    }
}

fn request_body_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("model")
                .and_then(Value::as_str)
                .map(normalize_model_name)
        })
}

fn approximate_input_tokens(body: &[u8]) -> u64 {
    let value = serde_json::from_slice::<Value>(body).unwrap_or(Value::Null);
    let system = value.get("system").map(content_to_text).unwrap_or_default();
    let messages = value
        .get("messages")
        .map(content_to_text)
        .unwrap_or_default();
    ((system.len() + messages.len()) as u64 / 4).max(1)
}

fn content_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(obj) = value.as_object() {
        return obj
            .values()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn normalize_model_name(model: &str) -> String {
    let trimmed = if model.ends_with(']') {
        model.rfind('[').map_or(model, |idx| &model[..idx])
    } else {
        model
    };
    match trimmed.strip_prefix("claude-rayline-router-") {
        Some("balanced") => "rayline-router".to_owned(),
        Some(suffix) => format!("rayline-router-{suffix}"),
        None if trimmed == "rayline-router-balanced" => "rayline-router".to_owned(),
        None => trimmed.to_owned(),
    }
}

fn claude_code_agent_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(CLAUDE_CODE_AGENT_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn rayline_request_id(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn none_if_marker(value: &str) -> Option<String> {
    (value != "<none>").then(|| value.to_owned())
}

fn route_status_target(status: &RouteStatus) -> String {
    if status
        .route_id
        .as_deref()
        .is_some_and(|route_id| route_id.starts_with("local-"))
    {
        "local".to_owned()
    } else {
        "remote".to_owned()
    }
}

fn subagent_filter_allows(
    agent_id: &str,
    agent_type: Option<&str>,
    selective_subagent_ids: &[String],
) -> bool {
    selective_subagent_ids.is_empty()
        || selective_subagent_ids.iter().any(|allowed| {
            allowed == agent_id
                || agent_type.is_some_and(|agent_type| {
                    allowed == agent_type || allowed.eq_ignore_ascii_case(agent_type)
                })
        })
}

/// Resolve the subagent type, briefly retrying to absorb the race between
/// Claude Code writing `agent-<id>.meta.json` and the subagent's first request
/// reaching the proxy. Returns as soon as the type resolves; only loops while
/// it is still unresolved, up to a bounded total wait.
async fn resolve_claude_code_agent_type_with_retry(
    agent_id: &str,
    config_dir: Option<&Path>,
) -> Option<String> {
    let filename = format!("agent-{agent_id}.meta.json");
    retry_until_some(|| {
        resolve_claude_code_agent_type_from_roots(
            &filename,
            &claude_projects_roots_from(config_dir.map(Path::to_owned), claude_home_dir()),
            env::current_dir().ok().as_deref(),
        )
    })
    .await
}

/// Call `resolve` up to `AGENT_TYPE_RESOLVE_MAX_ATTEMPTS` times, returning the
/// first `Some` and sleeping `AGENT_TYPE_RESOLVE_RETRY_DELAY` between misses.
async fn retry_until_some<F>(mut resolve: F) -> Option<String>
where
    F: FnMut() -> Option<String>,
{
    for attempt in 0..AGENT_TYPE_RESOLVE_MAX_ATTEMPTS {
        if let Some(value) = resolve() {
            return Some(value);
        }
        if attempt + 1 < AGENT_TYPE_RESOLVE_MAX_ATTEMPTS {
            tokio::time::sleep(AGENT_TYPE_RESOLVE_RETRY_DELAY).await;
        }
    }
    None
}

/// Resolve the user's home directory cross-platform. `HOME` alone is wrong on
/// Windows: native PowerShell / cmd do not set it (they use `USERPROFILE`), so
/// relying on `HOME` left the Claude projects root empty and every subagent
/// resolved to `<none>` and passed through to the cloud. `dirs::home_dir`
/// resolves the Windows profile dir; the env fallbacks cover unusual setups.
fn claude_home_dir() -> Option<PathBuf> {
    dirs::home_dir()
        .or_else(|| env::var_os("HOME").map(PathBuf::from))
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
        .filter(|path| !path.as_os_str().is_empty())
}

fn claude_projects_roots_from(config_dir: Option<PathBuf>, home: Option<PathBuf>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config_dir) = config_dir.filter(|path| !path.as_os_str().is_empty()) {
        roots.push(config_dir.join("projects"));
    }
    if let Some(home) = home {
        let default = home.join(".claude").join("projects");
        if !roots.iter().any(|root| root == &default) {
            roots.push(default);
        }
    }
    roots
}

fn resolve_claude_code_agent_type_from_roots(
    filename: &str,
    projects_roots: &[PathBuf],
    cwd: Option<&Path>,
) -> Option<String> {
    for projects_root in projects_roots {
        if let Some(cwd) = cwd {
            let project_dir = projects_root.join(claude_project_slug(cwd));
            if let Some(agent_type) = find_agent_type_in_project_dir(&project_dir, filename) {
                return Some(agent_type);
            }
        }
        let Ok(projects) = fs::read_dir(projects_root) else {
            continue;
        };
        for project in projects.flatten() {
            let project_dir = project.path();
            if let Some(agent_type) = find_agent_type_in_project_dir(&project_dir, filename) {
                return Some(agent_type);
            }
        }
    }
    None
}

fn find_agent_type_in_project_dir(project_dir: &Path, filename: &str) -> Option<String> {
    for session in fs::read_dir(project_dir).ok()?.flatten() {
        let meta_path = session.path().join("subagents").join(filename);
        if let Some(agent_type) = read_agent_type_meta(&meta_path) {
            return Some(agent_type);
        }
    }
    None
}

fn read_agent_type_meta(path: &Path) -> Option<String> {
    let value = serde_json::from_slice::<Value>(&fs::read(path).ok()?).ok()?;
    value
        .get("agentType")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn claude_project_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn is_virtual_model(model: Option<&str>) -> bool {
    model.is_some_and(|model| {
        let normalized = normalize_model_name(model);
        normalized == "rayline-router"
    })
}

fn is_virtual_model_lookup(path: &str) -> bool {
    path.strip_prefix("/v1/models/")
        .is_some_and(|model_id| is_virtual_model(Some(model_id)))
}

fn is_provider_model_lookup(path: &str) -> bool {
    path.strip_prefix("/v1/models/")
        .is_some_and(|model_id| !model_id.is_empty() && !is_anthropic_model_id(model_id))
}

fn is_anthropic_model_id(model_id: &str) -> bool {
    normalize_model_name(model_id).starts_with("claude-")
}

fn is_model_list_path(path: &str) -> bool {
    matches!(path, "/v1/models" | "/v1/models/")
}

fn should_drop_header_for_route(name: &HeaderName, target: &RouteTarget) -> bool {
    if is_hop_by_hop(name) || name == hyper::header::HOST {
        return true;
    }
    if *target != RouteTarget::Router {
        return false;
    }
    let lower = name.as_str().to_ascii_lowercase();
    // Drop client-supplied auth, local-routing headers, and resolved-agent-type
    // headers: the proxy is the sole authority on those (set below from live
    // process state), so stale/spoofed client headers cannot influence routing.
    lower == "authorization"
        || lower == "x-api-key"
        || lower.starts_with("x-rayline-local-")
        || lower == RAYLINE_AGENT_TYPE_HEADER
}

fn should_drop_header_for_local_adapter(name: &HeaderName) -> bool {
    if is_hop_by_hop(name) || name == hyper::header::HOST || name == hyper::header::CONTENT_LENGTH {
        return true;
    }
    matches!(
        name.as_str().to_ascii_lowercase().as_str(),
        "authorization" | "x-api-key"
    )
}

fn is_anthropic_authority(authority: &str) -> bool {
    authority.eq_ignore_ascii_case(&format!("{ANTHROPIC_HOST}:443"))
}

/// Pull `usage_doc_id` out of a Location URL's query string.
fn extract_usage_doc_id(location: &str) -> Option<String> {
    let q = location.split_once('?')?.1;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next()?;
        let v = it.next().unwrap_or("");
        if k == "usage_doc_id" && !v.is_empty() {
            return Some(v.to_string());
        }
    }
    None
}

fn rewrite_local_redirect_port(location: &str, adapter_port: Option<u16>) -> Option<String> {
    let adapter_port = adapter_port?;
    for prefix in ["http://127.0.0.1:", "http://localhost:"] {
        let Some(rest) = location.strip_prefix(prefix) else {
            continue;
        };
        let (_, path) = rest.split_once('/')?;
        return Some(format!("{prefix}{adapter_port}/{path}"));
    }
    None
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    is_hop_by_hop_str(name.as_str())
}

fn is_hop_by_hop_str(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

struct LocalCa {
    cert_pem: String,
    key_pem: String,
}

impl LocalCa {
    fn load_or_generate(cert_path: &PathBuf, key_path: &PathBuf) -> Result<Self> {
        let _lock = CaPathLock::acquire(cert_path)?;
        if cert_path.is_file() && key_path.is_file() {
            let ca = Self {
                cert_pem: fs::read_to_string(cert_path)
                    .with_context(|| format!("read CA cert {}", cert_path.display()))?,
                key_pem: fs::read_to_string(key_path)
                    .with_context(|| format!("read CA key {}", key_path.display()))?,
            };
            if ca.is_valid_existing_pair() {
                // Tighten permissions on the happy-load path so that a CA
                // directory/key created by an older release at 0755/0644 is
                // repaired on next load, keeping the SECURITY.md guarantee true
                // for existing installs.
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if let Some(parent) = cert_path.parent() {
                        let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
                    }
                    let _ = fs::set_permissions(key_path, fs::Permissions::from_mode(0o600));
                }
                return Ok(ca);
            }
            warn!(
                "regenerating Rayline proxy CA because the existing certificate/key pair is invalid or legacy"
            );
        }

        if let Some(parent) = cert_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create CA cert dir {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }
        if let Some(parent) = key_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create CA key dir {}", parent.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(parent, fs::Permissions::from_mode(0o700));
            }
        }

        let ca = Self::generate()?;
        write_text_atomic(cert_path, &ca.cert_pem)
            .with_context(|| format!("write CA cert {}", cert_path.display()))?;
        write_private_key(key_path, &ca.key_pem)
            .with_context(|| format!("write CA key {}", key_path.display()))?;
        Ok(ca)
    }

    fn generate() -> Result<Self> {
        use time::{Duration, OffsetDateTime};

        let mut params = CertificateParams::new(vec!["Rayline Local Proxy CA".to_string()])?;
        params.subject_alt_names.clear();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, "Rayline");
        dn.push(DnType::CommonName, "Rayline Local Proxy CA");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        // Backdate not_before by 5 minutes to absorb clock-skew across machines.
        // Cap not_after at 180 days so a leaked or forgotten CA key has a bounded
        // lifetime; load_or_generate regenerates automatically on expiry.
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::minutes(5);
        params.not_after = now + Duration::days(180);
        let key_pair = KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;
        Ok(Self {
            cert_pem: cert.pem(),
            key_pem: key_pair.serialize_pem(),
        })
    }

    fn server_config_for_host(&self, host: &str) -> Result<Arc<ServerConfig>> {
        let ca_key_pair = rcgen::KeyPair::from_pem(&self.key_pem).context("parse CA key")?;
        let ca = rcgen::Issuer::from_ca_cert_pem(&self.cert_pem, ca_key_pair)
            .context("parse local CA certificate")?;

        let mut leaf_params = CertificateParams::new(vec![host.to_string()])?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::OrganizationName, "Rayline");
        dn.push(DnType::CommonName, host);
        leaf_params.distinguished_name = dn;
        leaf_params.is_ca = IsCa::NoCa;
        leaf_params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        let leaf_key_pair = KeyPair::generate()?;
        let leaf = leaf_params.signed_by(&leaf_key_pair, &ca)?;
        let leaf_pem = leaf.pem();
        let leaf_key_pem = leaf_key_pair.serialize_pem();

        let cert_chain = certs_from_pem(&leaf_pem)?
            .into_iter()
            .chain(certs_from_pem(&self.cert_pem)?)
            .collect();
        let key = private_key_from_pem(&leaf_key_pem)?;
        let config = ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
        Ok(Arc::new(config))
    }

    fn is_valid_existing_pair(&self) -> bool {
        match self.validate_existing_pair() {
            Ok(()) => true,
            Err(error) => {
                warn!("invalid Rayline proxy CA pair: {error}");
                false
            }
        }
    }

    fn validate_existing_pair(&self) -> Result<()> {
        self.validate_key_matches_cert()?;
        self.validate_not_expired()?;
        self.server_config_for_host(ANTHROPIC_HOST)
            .context("generate leaf server config from CA")?;
        Ok(())
    }

    /// Return an error if the CA cert is already expired or will expire within 7 days.
    fn validate_not_expired(&self) -> Result<()> {
        let certs = certs_from_pem(&self.cert_pem)?;
        let der = certs
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("CA PEM did not contain a certificate"))?;
        let (_, cert) = x509_parser::parse_x509_certificate(&der)
            .map_err(|error| anyhow!("parse CA certificate for expiry check: {error}"))?;
        let validity = &cert.tbs_certificate.validity;
        if !validity.is_valid() {
            return Err(anyhow!("CA certificate is expired"));
        }
        // Also regenerate when the CA expires within 7 days so clients aren't
        // surprised by a sudden TLS failure.
        let secs_remaining = validity
            .time_to_expiration()
            .map(|d| d.whole_seconds())
            .unwrap_or(0);
        if secs_remaining < 7 * 24 * 3600 {
            return Err(anyhow!(
                "CA certificate expires in less than 7 days ({secs_remaining}s remaining)"
            ));
        }
        Ok(())
    }

    fn validate_key_matches_cert(&self) -> Result<()> {
        let key_pair = rcgen::KeyPair::from_pem(&self.key_pem).context("parse CA key")?;
        let cert_der = certs_from_pem(&self.cert_pem)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("CA PEM did not contain a certificate"))?;
        let (_, cert) = x509_parser::parse_x509_certificate(&cert_der)
            .map_err(|error| anyhow!("parse CA certificate for key validation: {error}"))?;
        if cert.tbs_certificate.subject_pki.raw != key_pair.subject_public_key_info() {
            return Err(anyhow!(
                "CA certificate public key does not match private key"
            ));
        }
        Ok(())
    }
}

struct CaPathLock {
    path: PathBuf,
}

impl CaPathLock {
    fn acquire(cert_path: &Path) -> Result<Self> {
        let lock_path = cert_path.with_extension("lock");
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create CA lock dir {}", parent.display()))?;
        }
        let started = SystemTime::now();
        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(mut file) => {
                    let _ = writeln!(file, "{}", std::process::id());
                    return Ok(Self { path: lock_path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if stale_lock(&lock_path) {
                        let _ = fs::remove_file(&lock_path);
                        continue;
                    }
                    if started.elapsed().unwrap_or_default() > Duration::from_secs(10) {
                        return Err(anyhow!(
                            "timed out waiting for CA lock {}",
                            lock_path.display()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("create CA lock {}", lock_path.display()));
                }
            }
        }
    }
}

impl Drop for CaPathLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn stale_lock(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age > Duration::from_secs(30))
}

fn certs_from_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("parse PEM certificates")
}

fn private_key_from_pem(pem: &str) -> Result<PrivateKeyDer<'static>> {
    PrivateKeyDer::from_pem_slice(pem.as_bytes()).context("parse PEM private key")
}

fn write_private_key(path: &Path, contents: &str) -> Result<()> {
    let tmp = temp_sibling_path(path);
    let mut opts = OpenOptions::new();
    opts.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    std::io::Write::write_all(&mut opts.open(&tmp)?, contents.as_bytes())?;
    replace_file(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn write_text_atomic(path: &Path, contents: &str) -> Result<()> {
    let tmp = temp_sibling_path(path);
    fs::write(&tmp, contents).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    replace_file(&tmp, path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp);
    })?;
    Ok(())
}

fn replace_file(tmp: &Path, path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    if path.exists() {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    fs::rename(tmp, path)
}

fn temp_sibling_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("proxy-ca");
    path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rayline_metrics::MetricsSink as _;

    fn observe_test_sse_chunk(
        bytes: &[u8],
        buffer: &mut String,
        input_tokens: &mut Option<u64>,
        output_tokens: &mut Option<u64>,
        prompt_cache_tokens: &mut Option<u64>,
        prompt_processed_tokens: &mut Option<u64>,
    ) {
        observe_anthropic_sse_chunk(
            bytes,
            buffer,
            input_tokens,
            output_tokens,
            prompt_cache_tokens,
            prompt_processed_tokens,
        );
    }

    #[test]
    fn observes_anthropic_sse_usage_for_remote_metrics() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        let mut prompt_processed_tokens = None;
        observe_test_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":0}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
            &mut prompt_processed_tokens,
        );

        assert_eq!(input_tokens, Some(42));
        assert_eq!(output_tokens, Some(17));
    }

    #[test]
    fn observes_crlf_anthropic_sse_usage_for_remote_metrics() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        let mut prompt_processed_tokens = None;
        observe_test_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":0}}}\r\n\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\r\n\r\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
            &mut prompt_processed_tokens,
        );

        assert_eq!(input_tokens, Some(42));
        assert_eq!(output_tokens, Some(17));
        assert!(buffer.is_empty());
    }

    #[test]
    fn observes_unterminated_anthropic_sse_usage_on_flush() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        let mut prompt_processed_tokens = None;
        observe_test_sse_chunk(
            b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
            &mut prompt_processed_tokens,
        );
        assert_eq!(output_tokens, None);

        observe_test_sse_chunk(
            b"\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
            &mut prompt_processed_tokens,
        );
        assert_eq!(output_tokens, Some(17));
        assert!(buffer.is_empty());
    }

    #[test]
    fn observes_cached_anthropic_sse_usage_for_remote_metrics() {
        let mut buffer = String::new();
        let mut input_tokens = Some(12);
        let mut output_tokens = None;
        let mut prompt_cache_tokens = None;
        let mut prompt_processed_tokens = None;
        observe_test_sse_chunk(
            b"data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\n\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
            &mut buffer,
            &mut input_tokens,
            &mut output_tokens,
            &mut prompt_cache_tokens,
            &mut prompt_processed_tokens,
        );

        assert_eq!(input_tokens, Some(42));
        assert_eq!(output_tokens, Some(17));
        assert_eq!(prompt_cache_tokens, Some(30));
        assert_eq!(prompt_processed_tokens, Some(12));
    }

    #[test]
    fn extracts_usage_from_completed_sse_body() {
        let usage = usage_from_anthropic_body(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
        );

        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(17));
        assert_eq!(usage.prompt_cache_tokens, Some(30));
        assert_eq!(usage.prompt_processed_tokens, Some(12));
    }

    #[test]
    fn extracts_usage_from_gzip_sse_body_after_decode() {
        let body = b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(body).unwrap();
        let compressed = encoder.finish().unwrap();

        let decoded = decode_body_for_metrics(&compressed, Some("gzip")).unwrap();
        let usage = usage_from_anthropic_body(&decoded);

        assert_eq!(usage.input_tokens, Some(42));
        assert_eq!(usage.output_tokens, Some(17));
        assert_eq!(usage.prompt_cache_tokens, Some(30));
        assert_eq!(usage.prompt_processed_tokens, Some(12));
    }

    /// Verifies that a gzip-compressed payload that expands beyond MAX_DECODE_BYTES
    /// causes decode_body_for_metrics to return None (skip metrics) rather than OOM.
    /// RED: before fix, decode_body_for_metrics returns Result<Vec<u8>> (no Option variant).
    #[test]
    fn decompression_is_capped_and_skips_metrics_on_bomb() {
        // 64 MiB of zeros compresses to a tiny gzip stream — a classic gzip bomb pattern.
        let plaintext = vec![0u8; 64 * 1024 * 1024];
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        encoder.write_all(&plaintext).unwrap();
        let bomb = encoder.finish().unwrap();

        // Must return None (cap exceeded) — never OOM.
        assert!(
            decode_body_for_metrics(&bomb, Some("gzip")).is_none(),
            "expected decode_body_for_metrics to return None for oversized decompressed output"
        );
    }

    /// Verifies that a small gzip body still decodes successfully (cap is not over-aggressive).
    #[test]
    fn decompression_succeeds_for_small_gzip_body() {
        let body = b"data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":5}}\n\n";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        use std::io::Write;
        encoder.write_all(body).unwrap();
        let compressed = encoder.finish().unwrap();

        let decoded = decode_body_for_metrics(&compressed, Some("gzip"));
        assert!(
            decoded.is_some(),
            "expected small gzip body to decode successfully"
        );
        assert_eq!(decoded.unwrap(), body);
    }

    #[test]
    fn routes_known_router_paths_to_router() {
        for (method, path) in [
            (Method::POST, "/v1/messages"),
            (Method::POST, "/v1/messages/count_tokens"),
            (Method::GET, "/v1/models"),
            (Method::GET, "/v1/models/"),
            (Method::GET, "/v1/models/claude-sonnet-4"),
        ] {
            assert_eq!(
                classify_anthropic_request(&method, path).target,
                RouteTarget::Router,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn passes_anthropic_admin_and_unknown_paths_through() {
        for (method, path) in [
            (Method::GET, "/v1/mcp_servers"),
            (Method::GET, "/v1/mcp_servers/list"),
            (Method::POST, "/api/oauth/token"),
            (Method::GET, "/v1/sessions"),
            (Method::GET, "/v1/sessions/session-id"),
            (Method::GET, "/api/claude_code/config"),
            (Method::POST, "/v1/unknown_future_endpoint"),
        ] {
            assert_eq!(
                classify_anthropic_request(&method, path).target,
                RouteTarget::Anthropic,
                "{method} {path}"
            );
        }
    }

    #[test]
    fn selective_mode_passes_main_messages_through() {
        let headers = HeaderMap::new();
        let prepared = prepare_anthropic_route(
            &Method::POST,
            "/v1/messages",
            &headers,
            Bytes::from_static(br#"{"model":"claude-sonnet-4-5"}"#),
            ProxyRoutingMode::SelectiveSubagents,
        );

        assert_eq!(prepared.decision.target, RouteTarget::Anthropic);
        assert_eq!(prepared.decision.reason, "selective_main_passthrough");
        assert!(!prepared.body_was_rewritten);
    }

    #[test]
    fn proxied_sideband_metrics_endpoint_uses_path() {
        assert_eq!(
            proxied_sideband_endpoint_id(
                &RouteDecision {
                    target: RouteTarget::Anthropic,
                    reason: "selective_passthrough_path",
                },
                "/api/oauth/token"
            )
            .as_deref(),
            Some("/api/oauth/token")
        );
        assert_eq!(
            proxied_sideband_endpoint_id(
                &RouteDecision {
                    target: RouteTarget::Anthropic,
                    reason: "selective_main_passthrough",
                },
                "/v1/messages"
            ),
            None
        );
        assert_eq!(
            proxied_sideband_endpoint_id(
                &RouteDecision {
                    target: RouteTarget::Router,
                    reason: "selective_subagent_header",
                },
                "/v1/messages"
            ),
            None
        );
    }

    #[test]
    fn local_router_metrics_owner_suppresses_only_router_route_metrics() {
        let mut opts = ProxyOptions::with_ca_paths(
            "",
            PathBuf::from("proxy-ca.pem"),
            PathBuf::from("proxy-ca-key.pem"),
        );
        opts.local_router_owns_metrics = true;

        assert!(!proxy_owns_metrics_for_route(&opts, RouteTarget::Router));
        assert!(proxy_owns_metrics_for_route(&opts, RouteTarget::Anthropic));
    }

    #[test]
    fn selective_mode_routes_subagent_messages_without_rewrite() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("agent-1"),
        );
        let prepared = prepare_anthropic_route(
            &Method::POST,
            "/v1/messages",
            &headers,
            Bytes::from_static(br#"{"model":"claude-sonnet-4-5","messages":[]}"#),
            ProxyRoutingMode::SelectiveSubagents,
        );

        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_subagent_header");
        assert!(!prepared.body_was_rewritten);
        let body: Value = serde_json::from_slice(&prepared.body).unwrap();
        assert_eq!(body["model"], "claude-sonnet-4-5");
        assert_eq!(body["messages"], json!([]));
    }

    #[test]
    fn selective_mode_routes_only_allowlisted_subagent_when_configured() {
        let allowlist = vec!["Explore".to_owned()];
        let mut explore_headers = HeaderMap::new();
        explore_headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("Explore"),
        );
        let prepared = prepare_anthropic_route_with_subagent_filter(
            &Method::POST,
            "/v1/messages",
            &explore_headers,
            Bytes::from_static(br#"{"model":"claude-sonnet-4-5","messages":[]}"#),
            ProxyRoutingMode::SelectiveSubagents,
            &allowlist,
            None,
        );
        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_subagent_header");

        let mut other_headers = HeaderMap::new();
        other_headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("Review"),
        );
        let prepared = prepare_anthropic_route_with_subagent_filter(
            &Method::POST,
            "/v1/messages",
            &other_headers,
            Bytes::from_static(br#"{"model":"claude-sonnet-4-5","messages":[]}"#),
            ProxyRoutingMode::SelectiveSubagents,
            &allowlist,
            None,
        );
        assert_eq!(prepared.decision.target, RouteTarget::Anthropic);
        assert_eq!(prepared.decision.reason, "selective_subagent_passthrough");
    }

    #[test]
    fn selective_mode_keys_route_on_resolved_agent_type_for_opaque_id() {
        let allowlist = vec!["Explore".to_owned()];
        let mut headers = HeaderMap::new();
        // A real Claude Code subagent id is an opaque hash, not the agent name,
        // so it only matches the allowlist once the type is resolved.
        headers.insert(
            CLAUDE_CODE_AGENT_ID_HEADER,
            HeaderValue::from_static("a4a4dba6877819a3a"),
        );
        let body = || Bytes::from_static(br#"{"model":"claude-sonnet-4-5","messages":[]}"#);

        // Unresolved (meta file not written yet) -> passthrough to the cloud.
        let prepared = prepare_anthropic_route_with_subagent_filter(
            &Method::POST,
            "/v1/messages",
            &headers,
            body(),
            ProxyRoutingMode::SelectiveSubagents,
            &allowlist,
            None,
        );
        assert_eq!(prepared.decision.target, RouteTarget::Anthropic);
        assert_eq!(prepared.decision.reason, "selective_subagent_passthrough");
        assert_eq!(prepared.agent_id.as_deref(), Some("a4a4dba6877819a3a"));
        assert_eq!(prepared.agent_type.as_deref(), None);

        // Resolved to an allowlisted type -> route locally through the router.
        let prepared = prepare_anthropic_route_with_subagent_filter(
            &Method::POST,
            "/v1/messages",
            &headers,
            body(),
            ProxyRoutingMode::SelectiveSubagents,
            &allowlist,
            Some("Explore"),
        );
        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_subagent_header");
        assert_eq!(prepared.agent_id.as_deref(), Some("a4a4dba6877819a3a"));
        assert_eq!(prepared.agent_type.as_deref(), Some("Explore"));
    }

    #[tokio::test]
    async fn retry_until_some_resolves_after_initial_misses() {
        let attempts = std::cell::Cell::new(0);
        // Mirrors the race: the meta file is missing for the first two reads,
        // then lands and resolves.
        let resolved = retry_until_some(|| {
            let n = attempts.get();
            attempts.set(n + 1);
            (n >= 2).then(|| "Explore".to_owned())
        })
        .await;
        assert_eq!(resolved.as_deref(), Some("Explore"));
        assert_eq!(attempts.get(), 3);
    }

    #[tokio::test]
    async fn retry_until_some_gives_up_after_max_attempts() {
        let attempts = std::cell::Cell::new(0);
        let resolved = retry_until_some(|| {
            attempts.set(attempts.get() + 1);
            None
        })
        .await;
        assert_eq!(resolved, None);
        assert_eq!(attempts.get(), AGENT_TYPE_RESOLVE_MAX_ATTEMPTS);
    }

    #[test]
    fn selective_subagent_filter_matches_resolved_agent_type() {
        let allowlist = vec!["Explore".to_owned()];

        assert!(subagent_filter_allows(
            "a332089fa2c10afe6",
            Some("Explore"),
            &allowlist
        ));
        assert!(subagent_filter_allows(
            "a332089fa2c10afe6",
            Some("explore"),
            &allowlist
        ));
        assert!(!subagent_filter_allows(
            "a332089fa2c10afe6",
            Some("Review"),
            &allowlist
        ));
    }

    #[test]
    fn claude_projects_roots_prefers_config_dir() {
        let roots = claude_projects_roots_from(
            Some(PathBuf::from("/tmp/claude-custom")),
            Some(PathBuf::from("/home/me")),
        );

        assert_eq!(
            roots,
            vec![
                PathBuf::from("/tmp/claude-custom/projects"),
                PathBuf::from("/home/me/.claude/projects")
            ]
        );
    }

    #[test]
    fn resolves_agent_type_from_config_dir_projects() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = Path::new("/repo/example");
        let meta_dir = tmp
            .path()
            .join("projects")
            .join(claude_project_slug(cwd))
            .join("session-1")
            .join("subagents");
        fs::create_dir_all(&meta_dir).unwrap();
        fs::write(
            meta_dir.join("agent-a332089fa2c10afe6.meta.json"),
            r#"{"agentType":"Explore"}"#,
        )
        .unwrap();
        let roots = claude_projects_roots_from(Some(tmp.path().to_path_buf()), None);

        assert_eq!(
            resolve_claude_code_agent_type_from_roots(
                "agent-a332089fa2c10afe6.meta.json",
                &roots,
                Some(cwd),
            )
            .as_deref(),
            Some("Explore")
        );
    }

    #[test]
    fn claude_project_slug_matches_claude_code_project_dir_shape() {
        assert_eq!(
            claude_project_slug(Path::new("/tmp/rayline/worktrees/example-project")),
            "-tmp-rayline-worktrees-example-project"
        );
    }

    #[test]
    fn selective_mode_routes_virtual_models_without_rewrite() {
        let headers = HeaderMap::new();
        let prepared = prepare_anthropic_route(
            &Method::POST,
            "/v1/messages/count_tokens",
            &headers,
            Bytes::from_static(br#"{"model":"rayline-router"}"#),
            ProxyRoutingMode::SelectiveSubagents,
        );

        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_virtual_model");
        assert!(!prepared.body_was_rewritten);
    }

    #[test]
    fn selective_mode_routes_virtual_model_names_with_ttl_suffix() {
        assert_eq!(normalize_model_name("rayline-router[1m]"), "rayline-router");
        assert_eq!(
            normalize_model_name("claude-rayline-router-balanced"),
            "rayline-router"
        );
        assert_eq!(
            normalize_model_name("claude-rayline-router-fast"),
            "rayline-router-fast"
        );
        assert_eq!(
            normalize_model_name("rayline-router-balanced[1m]"),
            "rayline-router"
        );

        let headers = HeaderMap::new();
        let prepared = prepare_anthropic_route(
            &Method::POST,
            "/v1/messages",
            &headers,
            Bytes::from_static(br#"{"model":"rayline-router[1m]"}"#),
            ProxyRoutingMode::SelectiveSubagents,
        );

        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_virtual_model");
        assert!(!prepared.body_was_rewritten);
    }

    #[test]
    fn selective_mode_routes_virtual_model_discovery_to_router() {
        assert_eq!(
            classify_anthropic_request_for_mode(
                &Method::GET,
                "/v1/models/rayline-router",
                ProxyRoutingMode::SelectiveSubagents,
            )
            .target,
            RouteTarget::Router,
        );
        assert_eq!(
            classify_anthropic_request_for_mode(
                &Method::GET,
                "/v1/models/z-ai-glm-x-preview",
                ProxyRoutingMode::SelectiveSubagents,
            )
            .target,
            RouteTarget::Router,
        );
        assert_eq!(
            classify_anthropic_request_for_mode(
                &Method::GET,
                "/v1/models/claude-sonnet-4-5",
                ProxyRoutingMode::SelectiveSubagents,
            )
            .target,
            RouteTarget::Anthropic,
        );
        assert_eq!(
            classify_anthropic_request_for_mode(
                &Method::GET,
                "/v1/models/gpt-5.5",
                ProxyRoutingMode::SelectiveSubagents,
            )
            .target,
            RouteTarget::Router,
        );
        assert_eq!(
            classify_anthropic_request_for_mode(
                &Method::GET,
                "/v1/models/z-ai/glm-x-preview",
                ProxyRoutingMode::SelectiveSubagents,
            )
            .target,
            RouteTarget::Router,
        );
        assert_eq!(
            classify_anthropic_request_for_mode(
                &Method::GET,
                "/v1/models",
                ProxyRoutingMode::SelectiveSubagents,
            )
            .target,
            RouteTarget::Router,
        );

        let headers = HeaderMap::new();
        let prepared = prepare_anthropic_route(
            &Method::GET,
            "/v1/models",
            &headers,
            Bytes::new(),
            ProxyRoutingMode::SelectiveSubagents,
        );
        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_model_list");

        let prepared = prepare_anthropic_route(
            &Method::GET,
            "/v1/models/gpt-5.5",
            &headers,
            Bytes::new(),
            ProxyRoutingMode::SelectiveSubagents,
        );
        assert_eq!(prepared.decision.target, RouteTarget::Router);
        assert_eq!(prepared.decision.reason, "selective_provider_model_lookup");
    }

    #[test]
    fn connect_classification_only_intercepts_anthropic() {
        assert_eq!(
            classify_connect_authority("api.anthropic.com:443").target,
            RouteTarget::Anthropic
        );
        assert_eq!(
            classify_connect_authority("example.com:443").target,
            RouteTarget::BlindTunnel
        );
    }

    #[test]
    fn proxy_basic_auth_carries_only_a_valid_opaque_launch_id() {
        use base64::Engine as _;

        let mut headers = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("rayline:launch_abc-123");
        headers.insert(
            hyper::header::PROXY_AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert_eq!(proxy_launch_id(&headers).as_deref(), Some("launch_abc-123"));

        let invalid = base64::engine::general_purpose::STANDARD.encode("rayline:launch/unsafe");
        headers.insert(
            hyper::header::PROXY_AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {invalid}")).unwrap(),
        );
        assert_eq!(proxy_launch_id(&headers), None);
    }

    fn classify(method: Method, target: &str) -> ProxyAction {
        classify_proxy_request(&method, &target.parse::<Uri>().unwrap())
    }

    #[test]
    fn forward_proxy_requests_are_re_originated_not_405() {
        // axios forward-proxies HTTPS as an absolute-form request target; a
        // CONNECT-only proxy that 405s this is exactly the firecrawl-mcp bug.
        assert_eq!(
            classify(Method::GET, "https://api.firecrawl.dev/v2/scrape"),
            ProxyAction::ForwardAbsolute
        );
        assert_eq!(
            classify(Method::POST, "https://api.firecrawl.dev/v2/scrape"),
            ProxyAction::ForwardAbsolute
        );
        // cleartext absolute-form (e.g. npm registry probes) is forwarded too.
        assert_eq!(
            classify(Method::GET, "http://registry.npmjs.org/firecrawl-mcp"),
            ProxyAction::ForwardAbsolute
        );
    }

    #[test]
    fn classify_proxy_request_routes_anthropic_connect_and_health() {
        assert_eq!(classify(Method::GET, "/healthz"), ProxyAction::Healthz);
        assert_eq!(
            classify(Method::CONNECT, "api.anthropic.com:443"),
            ProxyAction::Connect
        );
        // forward-proxied Anthropic still goes through the Rayline router path.
        assert_eq!(
            classify(Method::POST, "https://api.anthropic.com/v1/messages"),
            ProxyAction::ForwardAnthropic
        );
        // origin-form non-CONNECT (no authority) is unservable.
        assert_eq!(classify(Method::GET, "/v1/messages"), ProxyAction::Reject);
    }

    #[test]
    fn forward_proxied_healthz_is_not_shadowed_by_local_health() {
        // Only the origin-form probe is local health; an upstream /healthz URL
        // carries an authority and must be forwarded, not intercepted.
        assert_eq!(classify(Method::GET, "/healthz"), ProxyAction::Healthz);
        assert_eq!(
            classify(Method::GET, "https://api.firecrawl.dev/healthz"),
            ProxyAction::ForwardAbsolute
        );
        assert_eq!(
            classify(Method::GET, "https://api.anthropic.com/healthz"),
            ProxyAction::ForwardAnthropic
        );
    }

    #[test]
    fn forward_proxy_drops_proxy_connection_header() {
        // axios sends `Proxy-Connection: keep-alive`; it must not leak upstream.
        assert!(is_hop_by_hop(&HeaderName::from_static("proxy-connection")));
    }

    #[test]
    fn local_available_now_gates_on_health_flag() {
        let mut opts =
            ProxyOptions::with_ca_paths("rayline-test-router-key", "/tmp/c.pem", "/tmp/k.pem");
        opts.local_available = true;
        // No health flag wired → honour the static capability.
        assert!(opts.local_available_now());
        // Healthy flag → available.
        opts.local_healthy = Some(Arc::new(AtomicBool::new(true)));
        assert!(opts.local_available_now());
        // Unhealthy flag → NOT available, even though statically capable.
        opts.local_healthy = Some(Arc::new(AtomicBool::new(false)));
        assert!(!opts.local_available_now());
        // Statically incapable → never available regardless of health.
        opts.local_available = false;
        opts.local_healthy = Some(Arc::new(AtomicBool::new(true)));
        assert!(!opts.local_available_now());
    }

    #[test]
    fn router_route_drops_claude_auth_and_hop_by_hop_headers() {
        assert!(should_drop_header_for_route(
            &HeaderName::from_static("authorization"),
            &RouteTarget::Router
        ));
        assert!(should_drop_header_for_route(
            &HeaderName::from_static("x-api-key"),
            &RouteTarget::Router
        ));
        // Client-supplied local-routing headers are dropped so the proxy stays
        // the sole authority on local availability.
        for h in [
            "x-rayline-local-available",
            "x-rayline-local-hint",
            "x-rayline-local-model-id",
            "x-rayline-local-custom",
            RAYLINE_AGENT_TYPE_HEADER,
        ] {
            assert!(should_drop_header_for_route(
                &HeaderName::from_bytes(h.as_bytes()).unwrap(),
                &RouteTarget::Router
            ));
        }
        assert!(!should_drop_header_for_route(
            &HeaderName::from_static("anthropic-version"),
            &RouteTarget::Router
        ));
        assert!(!should_drop_header_for_route(
            &HeaderName::from_static("authorization"),
            &RouteTarget::Anthropic
        ));
        assert!(should_drop_header_for_route(
            &HeaderName::from_static("connection"),
            &RouteTarget::Anthropic
        ));
        assert!(!should_drop_header_for_route(
            &HeaderName::from_static("accept-encoding"),
            &RouteTarget::Anthropic
        ));
    }

    #[test]
    fn local_redirect_stashes_router_key_not_claude_auth() {
        let cache = new_auth_cache();
        let opts = ProxyOptions {
            port: DEFAULT_PORT,
            router_url: DEFAULT_ROUTER_URL.to_string(),
            router_api_key: "rayline-test-router-key".to_string(),
            local_available: true,
            local_healthy: None,
            local_model_id: Some("local-model".to_string()),
            local_adapter_port: None,
            custom_mode: false,
            auth_cache: Some(cache.clone()),
            ca_cert_path: PathBuf::from("/tmp/unused-cert.pem"),
            ca_key_path: PathBuf::from("/tmp/unused-key.pem"),
            anthropic_url: DEFAULT_ANTHROPIC_URL.to_string(),
            connect_overrides: HashMap::new(),
            upstream_ca_path: None,
            route_status_path: None,
            routing_mode: ProxyRoutingMode::All,
            selective_subagent_ids: Vec::new(),
            local_router_owns_metrics: false,
            metrics: None,
            subscription_pool: None,
            session_status_dir: None,
            claude_config_dir: None,
        };
        let state = AppState {
            opts: Arc::new(opts),
            http: reqwest::Client::new(),
            local_http: reqwest::Client::new(),
            ca: Arc::new(LocalCa::generate().unwrap()),
            route_status_generation: Arc::new(AtomicU64::new(0)),
            route_status_io: Arc::new(AsyncMutex::new(())),
            session_status_generation: Arc::new(AtomicU64::new(0)),
            session_status_io: Arc::new(AsyncMutex::new(())),
        };

        stash_router_auth_for_local_redirect(
            &state,
            Some("http://127.0.0.1:20808/api/v1/messages?usage_doc_id=doc-1"),
        );

        let guard = cache.lock().unwrap();
        let headers = guard.get("doc-1").unwrap();
        assert_eq!(
            headers.get("x-api-key"),
            Some(&"rayline-test-router-key".to_string())
        );
        assert!(!headers.contains_key("authorization"));
    }

    #[test]
    fn local_ca_can_generate_leaf_server_config() {
        let ca = LocalCa::generate().unwrap();
        let config = ca.server_config_for_host(ANTHROPIC_HOST).unwrap();
        drop(config);
    }

    #[test]
    fn local_ca_regenerates_mismatched_cert_and_key_pair() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("proxy-ca.pem");
        let key_path = dir.path().join("proxy-ca.key");
        let ca_a = LocalCa::generate().unwrap();
        let ca_b = LocalCa::generate().unwrap();
        write_text_atomic(&cert_path, &ca_a.cert_pem).unwrap();
        write_private_key(&key_path, &ca_b.key_pem).unwrap();

        let loaded = LocalCa::load_or_generate(&cert_path, &key_path).unwrap();

        loaded.validate_existing_pair().unwrap();
        assert_ne!(loaded.cert_pem, ca_a.cert_pem);
        assert_ne!(loaded.key_pem, ca_b.key_pem);
    }

    #[test]
    fn write_text_atomic_replaces_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("proxy-ca.pem");
        write_text_atomic(&path, "old").unwrap();

        write_text_atomic(&path, "new").unwrap();

        assert_eq!(fs::read_to_string(path).unwrap(), "new");
    }

    #[test]
    fn rewrites_local_redirect_to_actual_adapter_port() {
        assert_eq!(
            rewrite_local_redirect_port(
                "http://127.0.0.1:20808/api/v1/messages?usage_doc_id=doc-1",
                Some(31080),
            ),
            Some("http://127.0.0.1:31080/api/v1/messages?usage_doc_id=doc-1".to_string()),
        );
        assert_eq!(
            rewrite_local_redirect_port(
                "https://api.anthropic.com/v1/messages?usage_doc_id=doc-1",
                Some(31080),
            ),
            None,
        );
        assert_eq!(
            rewrite_local_redirect_port(
                "http://127.0.0.1:20808/api/v1/messages?usage_doc_id=doc-1",
                None,
            ),
            None,
        );
    }

    #[test]
    fn load_upstream_ca_parses_self_signed_pem() {
        // rcgen lets us produce a valid x509 cert at test time without shipping
        // a fixture file. We then PEM-encode it, write to a tempfile, and load
        // it back through the public helper.
        use rcgen::{CertificateParams, KeyPair};
        let params = CertificateParams::new(vec!["test-corp-ca.invalid".into()]).unwrap();
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        let pem = cert.pem();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corp-ca.pem");
        std::fs::write(&path, pem).unwrap();

        let certs = load_upstream_ca_bundle(&path).expect("should parse one cert");
        assert_eq!(certs.len(), 1);
    }

    #[test]
    fn load_upstream_ca_reports_missing_file_clearly() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.pem");
        let err = load_upstream_ca_bundle(&missing).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does-not-exist.pem"),
            "error should name the path: {msg}"
        );
    }

    #[test]
    fn load_upstream_ca_rejects_empty_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.pem");
        std::fs::write(&path, b"# only a comment, no CERTIFICATE block\n").unwrap();
        let err = load_upstream_ca_bundle(&path).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("no certificates found") || msg.contains("empty.pem"),
            "empty PEM should produce a clear error: {msg}"
        );
    }

    fn header_map(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            headers.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn route_status_extracts_selected_model_and_metadata() {
        let headers = header_map(&[
            ("x-rayline-selected-model", "glm-4.6"),
            ("x-rayline-virtual-model", "rayline-router"),
            ("x-rayline-policy", "balanced"),
            ("x-rayline-task-class", "debugging"),
            ("x-rayline-route-id", "route-123"),
        ]);
        let status = RouteStatus::from_headers(&headers, None).unwrap();
        assert_eq!(status.selected_model, "glm-4.6");
        assert_eq!(status.virtual_model.as_deref(), Some("rayline-router"));
        assert_eq!(status.policy.as_deref(), Some("balanced"));
        assert_eq!(status.task_class.as_deref(), Some("debugging"));
        assert_eq!(status.route_id.as_deref(), Some("route-123"));
    }

    #[test]
    fn route_status_returns_none_without_selected_model() {
        let headers = header_map(&[("x-rayline-policy", "balanced")]);
        assert!(RouteStatus::from_headers(&headers, None).is_none());
    }

    #[test]
    fn route_status_maps_local_to_configured_model_id() {
        let headers = header_map(&[("x-rayline-selected-model", "local")]);
        let status = RouteStatus::from_headers(&headers, Some("qwen3-coder")).unwrap();
        assert_eq!(status.selected_model, "qwen3-coder");
    }

    #[test]
    fn route_status_keeps_local_literal_without_local_model_id() {
        let headers = header_map(&[("x-rayline-selected-model", "local")]);
        let status = RouteStatus::from_headers(&headers, None).unwrap();
        assert_eq!(status.selected_model, "local");
    }

    #[tokio::test]
    async fn launch_status_merges_route_and_later_assignment_updates() {
        let dir = tempfile::tempdir().unwrap();
        let generation = Arc::new(AtomicU64::new(0));
        let io = Arc::new(AsyncMutex::new(()));
        let mut assignment = SessionStatusSnapshot {
            schema: rayline_subscriptions::SESSION_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            assignment: rayline_subscriptions::SessionAssignmentStatus {
                primary_account_id: "a".to_owned(),
                current_account_id: "a".to_owned(),
                current_model_family: "sonnet".to_owned(),
                kind: rayline_subscriptions::SessionAssignmentKind::Primary,
                reason: SessionAssignmentReason::BalancedNewLaunch,
                assigned_at_unix: 100,
                last_seen_at_unix: 100,
            },
            capacity: rayline_subscriptions::SessionCapacityStatus {
                usage_snapshot_fresh: true,
                effective_headroom: Some(0.8),
                bottleneck: None,
                applicable: Vec::new(),
                eligible_accounts: 2,
                total_accounts: 2,
            },
            placement: rayline_subscriptions::SessionPlacementStatus {
                strategy: "balanced_sessions".to_owned(),
                score: Some(0.8),
                active_global_leases: 1,
                active_model_leases: 1,
            },
            route: None,
            updated_at_unix: 100,
        };
        write_session_status_snapshot(
            dir.path(),
            "launch",
            &assignment,
            Arc::clone(&generation),
            Arc::clone(&io),
        )
        .await;

        let route = SessionRouteStatus {
            selected_model: "glm-4.6".to_owned(),
            virtual_model: Some("rayline-router".to_owned()),
            policy: Some("balanced".to_owned()),
            task_class: Some("debugging".to_owned()),
            route_id: Some("route-1".to_owned()),
            updated_at_unix: 101,
        };
        write_session_route_status(
            dir.path(),
            "launch",
            &route,
            Arc::clone(&generation),
            Arc::clone(&io),
        )
        .await;

        assignment.assignment.current_account_id = "b".to_owned();
        assignment.assignment.kind = rayline_subscriptions::SessionAssignmentKind::ModelOverride;
        assignment.updated_at_unix = 102;
        write_session_status_snapshot(dir.path(), "launch", &assignment, generation, io).await;

        let path = dir
            .path()
            .join(format!("{}.json", derive_status_id("launch")));
        let merged: SessionStatusSnapshot =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(merged.assignment.current_account_id, "b");
        assert_eq!(
            merged
                .route
                .as_ref()
                .map(|route| route.selected_model.as_str()),
            Some("glm-4.6")
        );
        assert_eq!(merged.updated_at_unix, 102);
    }

    #[tokio::test]
    async fn write_to_round_trips_and_leaves_no_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("route-status.json");
        let status = RouteStatus {
            selected_model: "glm-4.6".to_string(),
            virtual_model: Some("rayline-router".to_string()),
            policy: Some("balanced".to_string()),
            task_class: Some("debugging".to_string()),
            route_id: Some("route-123".to_string()),
        };
        status.write_serialized(&path).await;

        let raw = std::fs::read_to_string(&path).unwrap();
        let parsed: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(parsed["selected_model"], "glm-4.6");
        assert_eq!(parsed["policy"], "balanced");
        assert!(parsed["ts"].as_u64().unwrap() > 0);

        // Atomic write must not leave the temp file behind.
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[tokio::test]
    async fn write_to_if_current_skips_stale_generation_waiting_on_io() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("route-status.json");
        let generation = Arc::new(AtomicU64::new(0));
        let io = Arc::new(AsyncMutex::new(()));
        let status = RouteStatus {
            selected_model: "glm-4.6".to_string(),
            virtual_model: Some("rayline-router".to_string()),
            policy: Some("balanced".to_string()),
            task_class: Some("debugging".to_string()),
            route_id: Some("route-123".to_string()),
        };
        let guard = io.lock().await;
        let write_task = tokio::spawn({
            let status = status.clone();
            let path = path.clone();
            let generation = generation.clone();
            let io = io.clone();
            async move {
                status.write_to_if_current(&path, generation, io, 0).await;
            }
        });

        generation.fetch_add(1, Ordering::SeqCst);
        drop(guard);
        write_task.await.unwrap();

        assert!(!path.exists(), "stale generation should not write status");
    }

    /// Golden contract: the PRODUCTION resolver (`resolve_claude_code_agent_type_from_roots`)
    /// must find `agentType` in a real captured Claude Code agent meta file.
    ///
    /// This test FAILS if Claude Code renames the field, restructures the JSON, or changes
    /// the path layout — that is the intent.  The fixture at
    /// `tests/fixtures/agent-test.meta.json` was sanitized from an actual capture
    /// (description scrubbed; agentType value kept verbatim).
    ///
    /// Path layout written to a temp dir:
    ///   <projects_root>/<project-dir>/<session-dir>/subagents/agent-<id>.meta.json
    ///
    /// The agent-id header sent by Claude Code is `x-claude-code-agent-id`.
    #[test]
    fn golden_contract_pins_cc_agent_metadata_shape() {
        // The header name Claude Code uses to identify the subagent.
        assert_eq!(
            CLAUDE_CODE_AGENT_ID_HEADER, "x-claude-code-agent-id",
            "header constant must match the documented CC header name"
        );

        let fixture_content = include_str!("../tests/fixtures/agent-test.meta.json");
        let agent_id = "aaea8a9732b057638";
        let filename = format!("agent-{agent_id}.meta.json");

        // Build the path layout the production resolver walks:
        //   <projects_root>/<project-dir>/<session-dir>/subagents/agent-<id>.meta.json
        let tmp = std::env::temp_dir().join(format!(
            "rayline-golden-contract-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let projects_root = tmp.clone();
        let project_dir = projects_root.join("some-project-dir");
        let session_dir = project_dir.join("some-session-dir");
        let subagents_dir = session_dir.join("subagents");
        fs::create_dir_all(&subagents_dir).expect("create temp fixture dir");
        let meta_path = subagents_dir.join(&filename);
        fs::write(&meta_path, fixture_content).expect("write fixture");

        // Call the PRODUCTION resolver — not raw JSON parsing.
        let result = resolve_claude_code_agent_type_from_roots(&filename, &[projects_root], None);

        // Clean up before asserting so failure doesn't leave temp files.
        let _ = fs::remove_dir_all(&tmp);

        assert_eq!(
            result,
            Some("general-purpose".to_owned()),
            "production resolver must find agentType=general-purpose from fixture; \
             update fixture if CC changes the field name or value"
        );
    }

    /// When `RouterMetrics` receives a `RoutingUncertain` event the
    /// `routing_uncertain` counter must be incremented. This pins the metrics
    /// plumbing without requiring the full proxy HTTP machinery.
    #[test]
    fn routing_uncertain_counter_increments_on_event() {
        let metrics = rayline_metrics::RouterMetrics::new("test");
        assert_eq!(metrics.snapshot().totals.routing_uncertain, 0);
        metrics.record(MetricsUpdate::RoutingUncertain {
            agent_id: "abc123".to_owned(),
        });
        assert_eq!(metrics.snapshot().totals.routing_uncertain, 1);
        metrics.record(MetricsUpdate::RoutingUncertain {
            agent_id: "def456".to_owned(),
        });
        assert_eq!(metrics.snapshot().totals.routing_uncertain, 2);
    }

    /// Pins the behavior of the extracted helper `emit_if_agent_type_unresolved`:
    /// when `agent_id` was present but type resolution exhausted, it warns and
    /// increments `routing_uncertain`.
    ///
    /// Coverage gap (known residual): this test does NOT pin the helper's
    /// *invocation* from `forward_anthropic_request` — deleting that call site
    /// in the async path would not fail any unit test here. Pinning it would
    /// require async/mock-server integration infrastructure, deferred as
    /// out-of-scope for this observability task.
    ///
    /// Scenario: `agent_id` was present in the request (agent_id_present=true)
    /// but resolution exhausted all retries (agent_type=None).
    #[test]
    fn proxy_branch_increments_routing_uncertain_when_agent_type_unresolved() {
        let metrics = rayline_metrics::RouterMetrics::new("test-proxy-branch");
        assert_eq!(metrics.snapshot().totals.routing_uncertain, 0);

        // agent_id present, type not resolved → must warn and increment.
        let incremented = emit_if_agent_type_unresolved(
            true,
            None,
            "agent-xyz".to_owned(),
            Some(&(metrics.clone() as Arc<dyn rayline_metrics::MetricsSink>)),
        );
        assert!(
            incremented,
            "must return true when the uncertain branch fires"
        );
        assert_eq!(
            metrics.snapshot().totals.routing_uncertain,
            1,
            "counter must increment via the proxy branch helper"
        );

        // agent_id present, type resolved → must NOT increment.
        let incremented = emit_if_agent_type_unresolved(
            true,
            Some("Explore"),
            "agent-xyz".to_owned(),
            Some(&(metrics.clone() as Arc<dyn rayline_metrics::MetricsSink>)),
        );
        assert!(!incremented, "must return false when type is resolved");
        assert_eq!(
            metrics.snapshot().totals.routing_uncertain,
            1,
            "counter must not increment when type is resolved"
        );

        // agent_id absent → must NOT increment even if type is also absent.
        let incremented = emit_if_agent_type_unresolved(
            false,
            None,
            "<none>".to_owned(),
            Some(&(metrics.clone() as Arc<dyn rayline_metrics::MetricsSink>)),
        );
        assert!(!incremented, "must return false when agent_id is absent");
        assert_eq!(
            metrics.snapshot().totals.routing_uncertain,
            1,
            "counter must not increment when agent_id is absent"
        );
    }

    /// CA validity window must be ≤ 180 days.  rcgen's default is ~2100 years;
    /// we need to see a bounded window so TLS interception doesn't silently
    /// "work forever" after the CA is compromised or forgotten.
    #[test]
    fn ca_has_bounded_validity() {
        use x509_parser::parse_x509_certificate;

        let ca = LocalCa::generate().unwrap();
        let certs = certs_from_pem(&ca.cert_pem).unwrap();
        let der = certs.into_iter().next().unwrap();
        let (_, cert) = parse_x509_certificate(&der).unwrap();
        let validity = cert.tbs_certificate.validity;

        let not_before = validity.not_before.timestamp();
        let not_after = validity.not_after.timestamp();
        let window_secs = not_after - not_before;

        // Window must be positive and at most 180 days + 10 min (5 min skew each side).
        let max_secs: i64 = 180 * 24 * 3600 + 600;
        assert!(
            window_secs > 0,
            "CA validity window must be positive, got {window_secs}s"
        );
        assert!(
            window_secs <= max_secs,
            "CA validity window {window_secs}s exceeds 180 days + 10 min ({max_secs}s); \
             rcgen default is unbounded — set params.not_after"
        );
    }

    /// `load_or_generate` must regenerate the CA when the cert on disk is expired.
    /// This prevents the 180-day time-bomb: after expiry TLS interception would
    /// silently break without this check.
    #[test]
    fn load_or_generate_regenerates_expired_ca() {
        use rcgen::{
            BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair,
            KeyUsagePurpose,
        };
        use time::{Duration, OffsetDateTime};

        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("ca.pem");
        let key_path = dir.path().join("ca-key.pem");

        // Generate a CA that expired 1 second ago.
        let mut params = CertificateParams::new(vec!["Expired Test CA".to_string()]).unwrap();
        params.subject_alt_names.clear();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "Expired Test CA");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let now = OffsetDateTime::now_utc();
        params.not_before = now - Duration::hours(1);
        params.not_after = now - Duration::seconds(1); // already expired
        let key_pair = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        std::fs::write(&cert_path, cert.pem()).unwrap();
        std::fs::write(&key_path, key_pair.serialize_pem()).unwrap();

        // load_or_generate must detect the expiry and regenerate a valid CA.
        let ca = LocalCa::load_or_generate(&cert_path, &key_path).unwrap();
        let certs = certs_from_pem(&ca.cert_pem).unwrap();
        let der = certs.into_iter().next().unwrap();
        let (_, cert_parsed) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert!(
            cert_parsed.tbs_certificate.validity.is_valid(),
            "regenerated CA must be currently valid"
        );
    }

    /// `load_or_generate` must create the CA directory with mode 0700 so that
    /// other users on a shared machine cannot read the local CA private key.
    #[cfg(unix)]
    #[test]
    fn ca_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let ca_dir = dir.path().join("ca");
        let cert_path = ca_dir.join("proxy-ca.pem");
        let key_path = ca_dir.join("proxy-ca-key.pem");

        // Directory doesn't exist yet; load_or_generate must create it with 0700.
        LocalCa::load_or_generate(&cert_path, &key_path).unwrap();

        let mode = std::fs::metadata(&ca_dir).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o700,
            "CA dir must be 0700, got 0o{:o}",
            mode & 0o777
        );
    }

    /// `load_or_generate` must repair loose permissions (0755 dir / 0644 key)
    /// on the happy-load path (valid existing CA, no regeneration).
    /// Before the fix this test fails: perms stay 0755/0644.
    /// After the fix it passes: load_or_generate tightens to 0700/0600.
    #[cfg(unix)]
    #[test]
    fn load_or_generate_tightens_loose_perms_on_existing_ca() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let ca_dir = dir.path().join("ca");
        let cert_path = ca_dir.join("proxy-ca.pem");
        let key_path = ca_dir.join("proxy-ca-key.pem");

        // Create a valid CA on disk (generates with correct perms).
        LocalCa::load_or_generate(&cert_path, &key_path).unwrap();

        // Deliberately loosen permissions to simulate an older install.
        std::fs::set_permissions(&ca_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Reload — must reuse the existing CA (not regenerate) and tighten perms.
        let loaded = LocalCa::load_or_generate(&cert_path, &key_path).unwrap();
        // Sanity: the reused CA must still be valid.
        loaded.validate_existing_pair().unwrap();

        let dir_mode = std::fs::metadata(&ca_dir).unwrap().permissions().mode();
        assert_eq!(
            dir_mode & 0o777,
            0o700,
            "CA dir must be tightened to 0700 on load, got 0o{:o}",
            dir_mode & 0o777
        );
        let key_mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(
            key_mode & 0o777,
            0o600,
            "CA key must be tightened to 0600 on load, got 0o{:o}",
            key_mode & 0o777
        );
    }
}
