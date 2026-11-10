//! Supervise the execution core and forward its native HTTP/SSE surfaces.
//! This path never enters the static router or local model adapter.

use std::convert::Infallible;
use std::io::{self, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use futures::TryStreamExt;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rand::RngCore;
use rayline_local_router::{InboundGuard, ModelCapabilities};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinSet;

const CONTRACT_VERSION: u32 = 1;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
/// The execution core's own request body limit.
const MAX_REQUEST_BYTES: usize = 64 << 20;
/// Wait before the one retry of a turn whose Decisions call found the routing
/// service unavailable or busy (a scale-to-zero cell answers 502 while it
/// starts), when the service names no `retry-after`. Jittered by
/// [`jittered`] so many sessions do not resend in step.
const DECISIONS_RETRY_DELAY: Duration = Duration::from_secs(5);
/// The longest `retry-after` rld waits out itself. A longer one is passed to
/// the harness without an rld retry.
const DECISIONS_MAX_RETRY_WAIT: Duration = Duration::from_secs(30);
/// `retry-after` the harness gets for a retryable failure when the service
/// named none.
const DEFAULT_HARNESS_RETRY_AFTER: Duration = Duration::from_secs(30);

#[derive(clap::Args, Debug)]
pub struct Args {
    /// Execution core binary. Built and versioned separately from rld.
    #[arg(long)]
    core: PathBuf,
    /// Credential-free execution-core configuration.
    #[arg(long)]
    config: PathBuf,
    /// Loopback gateway port; zero chooses a free port.
    #[arg(long, default_value_t = 20815)]
    port: u16,
    /// Shared deadline for core initialization and readiness, including profile checks.
    #[arg(long, default_value_t = 120, value_parser = clap::value_parser!(u64).range(1..=3600))]
    startup_timeout_secs: u64,
    /// Private file receiving this instance's gateway token.
    #[arg(long)]
    token_file: PathBuf,
    /// Private directory file receiving non-secret startup metadata.
    #[arg(long)]
    ready_file: PathBuf,
    /// Router URL of a configured Rayline environment that may receive
    /// Decisions requests, in addition to the built-in routers and loopback.
    #[arg(long = "router-url")]
    router_urls: Vec<String>,
    /// Rayline environment whose router key Decisions uses; named in the
    /// sign-in hint when Decisions refuses that key.
    #[arg(long)]
    env_name: Option<String>,
    /// Model that answers Claude Code's auto-mode classifier, through the
    /// profile's cloud Messages router.
    #[arg(long, default_value = crate::hybrid_classifier::DEFAULT_CLASSIFIER_MODEL)]
    classifier_model: String,
}

#[derive(Deserialize)]
struct CoreHandshake {
    contract_version: u32,
    listen_url: String,
    pid: u32,
}

#[derive(Serialize)]
struct Ready {
    contract_version: u32,
    ready: bool,
    listen_url: String,
    pid: u32,
    core_pid: u32,
    instance_id: String,
    credential_env: Vec<String>,
    capabilities: ModelCapabilities,
}

struct Gateway {
    core_url: String,
    core_token: String,
    inbound: InboundGuard,
    client: reqwest::Client,
    port: u16,
    core_pid: u32,
    instance_id: String,
    capabilities: ModelCapabilities,
    shutdown: mpsc::Sender<()>,
    /// The command that renews the router key Decisions uses.
    login_command: String,
    /// Where Claude Code's auto-mode classifier goes instead of the core.
    classifier_pin: Option<crate::hybrid_classifier::CloudPin>,
}

type Body = http_body_util::combinators::BoxBody<Bytes, io::Error>;

pub async fn run(args: Args) -> Result<()> {
    check_decisions_url(&args.config, &args.router_urls)?;
    let credential_env = credential_env_names(&args.config)?;
    let classifier_pin = crate::hybrid_classifier::cloud_pin(
        &serde_json::from_slice(&std::fs::read(&args.config)?)?,
        &args.classifier_model,
    );
    let startup_timeout = Duration::from_secs(args.startup_timeout_secs);
    let startup_deadline = tokio::time::Instant::now() + startup_timeout;
    // Register before spawning so cancellation during startup also reaps the child.
    let mut signals = Box::pin(shutdown_signal()?);
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, args.port)).await?;
    let port = listener.local_addr()?.port();
    let core_token = token();
    let gateway_token = token();
    write_private_new(&args.token_file, gateway_token.as_bytes())?;
    let mut cleanup = StateFiles {
        token: args.token_file.clone(),
        ready: None,
    };
    let mut child = Command::new(&args.core)
        .arg("--config")
        .arg(&args.config)
        .args(["--listen", "127.0.0.1:0"])
        .env("RAYLINE_EXEC_AUTH_TOKEN", &core_token)
        .env("RAYLINE_EXEC_PARENT_PIPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .context("start ARC execution core")?;
    // Keep the pipe open even while Child::wait closes its own stdin handle.
    // The core exits on EOF if rld is terminated without running cleanup.
    let mut parent_pipe = Some(
        child
            .stdin
            .take()
            .context("execution core parent pipe unavailable")?,
    );
    let outcome = async {
        let handshake = tokio::select! {
            result = tokio::time::timeout_at(startup_deadline, read_handshake(&mut child)) => {
                result.context("execution core startup timed out")??
            }
            result = &mut signals => { result?; return Ok(()); }
        };
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let health = tokio::select! {
            response = tokio::time::timeout_at(startup_deadline, async {
                client.get(format!("{}/healthz", handshake.listen_url))
                    .bearer_auth(&core_token).timeout(startup_timeout).send().await?
                    .error_for_status()?.json::<serde_json::Value>().await
            }) => response.context("execution core readiness timed out")??,
            result = &mut signals => { result?; return Ok(()); }
        };
        let instance_id = validate_health(&health)?.to_owned();
        let capabilities = model_capabilities(&health)?;
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel(1);
        let state = Arc::new(Gateway {
            core_url: handshake.listen_url,
            core_token,
            inbound: InboundGuard::new(gateway_token).context("generate gateway token")?,
            client,
            port,
            core_pid: handshake.pid,
            instance_id: instance_id.clone(),
            capabilities: capabilities.clone(),
            shutdown: shutdown_tx,
            login_command: login_command(args.env_name.as_deref()),
            classifier_pin,
        });
        let ready = Ready {
            contract_version: CONTRACT_VERSION,
            ready: true,
            listen_url: format!("http://127.0.0.1:{port}"),
            pid: std::process::id(),
            core_pid: handshake.pid,
            instance_id,
            credential_env,
            capabilities,
        };
        write_private_new(&args.ready_file, &serde_json::to_vec(&ready)?)?;
        cleanup.ready = Some(args.ready_file.clone());
        tracing::info!(
            port,
            core_pid = handshake.pid,
            "hybrid execution gateway ready"
        );
        tokio::select! {
            result = serve(listener, state) => result,
            status = child.wait() => bail!("execution core exited: {}", status?),
            result = &mut signals => result,
            _ = shutdown_rx.recv() => Ok(()),
        }
    }
    .await;
    // Dropping serve aborts connection tasks before the core is stopped.
    stop_child(&mut child, &mut parent_pipe)
        .await
        .context("stop execution core")?;
    drop(cleanup);
    outcome
}

/// Decisions requests carry execution context and the router key, so the core
/// is started only when `decisions.url` is a Rayline router or loopback. This
/// repeats the launcher's check on the file the core will actually read.
fn check_decisions_url(path: &Path, router_urls: &[String]) -> Result<()> {
    rayline_local_router::decisions_url::check_config_bytes(
        &std::fs::read(path)?,
        router_urls.iter().map(String::as_str),
    )
    .map_err(anyhow::Error::msg)
}

fn credential_env_names(path: &Path) -> Result<Vec<String>> {
    // Read only environment references; the core owns full config validation.
    #[derive(Default, Deserialize)]
    struct Auth {
        auth_env: Option<String>,
    }
    #[derive(Deserialize)]
    struct References {
        #[serde(default)]
        decisions: Auth,
        #[serde(default)]
        deployments: Vec<Auth>,
    }
    let references: References = serde_json::from_slice(&std::fs::read(path)?)?;
    let mut names: Vec<_> = std::iter::once(references.decisions)
        .chain(references.deployments)
        .filter_map(|auth| auth.auth_env)
        .filter(|name| !name.is_empty())
        .collect();
    if names.iter().any(|name| name.contains(['=', '\0'])) {
        bail!("credential environment reference is not a valid variable name");
    }
    names.sort();
    names.dedup();
    Ok(names)
}

async fn read_handshake(child: &mut Child) -> Result<CoreHandshake> {
    let stdout = child
        .stdout
        .take()
        .context("execution core stdout unavailable")?;
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    // Bound startup output; diagnostics belong on stderr.
    use tokio::io::AsyncReadExt;
    let count = (&mut reader).take(8193).read_line(&mut line).await?;
    if count == 0 || count > 8192 || !line.ends_with('\n') {
        bail!("execution core did not emit a bounded startup JSON line");
    }
    let handshake: CoreHandshake =
        serde_json::from_str(&line).context("invalid execution core handshake")?;
    if handshake.contract_version != CONTRACT_VERSION || child.id() != Some(handshake.pid) {
        bail!("execution core contract version or process identity mismatch");
    }
    let url = reqwest::Url::parse(&handshake.listen_url)?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|p| p == 0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        bail!("execution core handshake must name an HTTP loopback origin");
    }
    // Keep draining so an accidental later stdout write cannot block inference.
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut reader, &mut tokio::io::sink()).await;
    });
    Ok(handshake)
}

async fn serve(listener: TcpListener, state: Arc<Gateway>) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let state = state.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(handle(state, request).await) }
                    });
                    if let Err(error) = auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), service).await {
                        tracing::debug!(%error, "hybrid client connection closed");
                    }
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

async fn handle(state: Arc<Gateway>, request: Request<Incoming>) -> Response<Body> {
    // Reuse the existing browser/DNS-rebinding guard, then authenticate even health.
    if let Some(response) = rayline_local_router::reject_untrusted_request(
        state.port,
        request.method(),
        request.uri(),
        request.headers(),
    ) {
        return response;
    }
    if let Err(error) = state.inbound.check(&Method::POST, "/", request.headers()) {
        return error_response(error.status, error.message);
    }
    if request.method() == Method::GET && request.uri().path() == "/healthz" {
        // Check core health on demand; status cannot report a dead child as healthy.
        let health = async {
            let health: serde_json::Value = state
                .client
                .get(format!("{}/healthz", state.core_url))
                .bearer_auth(&state.core_token)
                .timeout(Duration::from_secs(2))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            if validate_health(&health)? != state.instance_id {
                bail!("execution core instance changed");
            }
            if model_capabilities(&health)? != state.capabilities {
                bail!("execution core capabilities changed");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        return match health {
            Ok(()) => json_response(
                StatusCode::OK,
                json!({
                    "ready": true, "contract_version": CONTRACT_VERSION,
                    "pid": std::process::id(), "core_pid": state.core_pid,
                    "instance_id": state.instance_id,
                    "capabilities": state.capabilities,
                }),
            ),
            _ => error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                "execution core is unavailable",
            ),
        };
    }
    if request.method() == Method::POST && request.uri().path() == "/_rayline/shutdown" {
        // Permit the response to flush before listener and active requests close.
        let shutdown = state.shutdown.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let _ = shutdown.send(()).await;
        });
        return json_response(
            StatusCode::OK,
            json!({"stopping":true,
            "pid":std::process::id(), "instance_id":state.instance_id}),
        );
    }
    match forward(&state, request).await {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(%error, "execution core request failed");
            error_response(StatusCode::BAD_GATEWAY, "execution core request failed")
        }
    }
}

fn validate_health(health: &serde_json::Value) -> Result<&str> {
    if health.get("ready") != Some(&json!(true))
        || health.get("contract_version") != Some(&json!(CONTRACT_VERSION))
    {
        bail!("execution core is not ready or has an incompatible contract");
    }
    health
        .get("instance_id")
        .and_then(serde_json::Value::as_str)
        .filter(|id| !id.is_empty())
        .context("execution core health has no instance identity")
}

fn model_capabilities(health: &serde_json::Value) -> Result<ModelCapabilities> {
    let capabilities: ModelCapabilities = serde_json::from_value(
        health
            .get("capabilities")
            .context("execution core health has no model capabilities")?
            .clone(),
    )?;
    capabilities.validate().map_err(anyhow::Error::msg)?;
    Ok(capabilities)
}

async fn forward(state: &Gateway, request: Request<Incoming>) -> Result<Response<Body>> {
    let (parts, body) = request.into_parts();
    // Buffered (within the core's own limit) so a turn can be sent again.
    let body = match http_body_util::Limited::new(body, MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) if error.is::<http_body_util::LengthLimitError>() => {
            return Ok(error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request body exceeds the execution core's 64 MiB limit",
            ));
        }
        Err(error) => bail!("read request body: {error}"),
    };
    if let Some(pin) = &state.classifier_pin
        && parts.method == Method::POST
        && crate::hybrid_classifier::is_classifier(parts.uri.path(), &body)
        && let Some(response) = send_classifier_to_cloud(state, pin, &parts, &body).await?
    {
        return Ok(response);
    }
    let mut retried = false;
    loop {
        let upstream = send_to_core(state, &parts, body.clone()).await?;
        if upstream.status() != StatusCode::BAD_GATEWAY {
            return stream_response(upstream);
        }
        // A refused decision arrives before any output, as one small JSON error.
        let status = upstream.status();
        let headers = upstream.headers().clone();
        let error_body = upstream.bytes().await?;
        let failure = decisions_failure(&error_body);
        let hint = RetryHint::from_headers(&headers);
        // Only the core's own Decisions status and code are logged (a
        // provider's error text may quote the prompt); the route id is the
        // service's, when it sent one.
        let detail = decisions_detail(&error_body);
        let route_id = headers
            .get("x-rayline-route-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-");
        if !retried
            && let Some(failure) = failure
            && let Some(delay) = rld_retry_delay(failure, hint)
        {
            // Nothing ran: the core decides before it dispatches, and treats an
            // identical resend like the harness's own retry.
            retried = true;
            tracing::info!(
                ?failure,
                %detail,
                route_id,
                delay_ms = delay.as_millis() as u64,
                "Decisions unavailable or busy; retrying the turn once"
            );
            tokio::time::sleep(delay).await;
            continue;
        }
        // Every 502 before any output is logged: the core's Decisions
        // failures are not all classified, and a Messages error loses its code.
        tracing::warn!(
            ?failure,
            %detail,
            route_id,
            resent = retried,
            "the core failed the turn before any output; returning the error to the harness"
        );
        return buffered_response(state, status, &headers, error_body, failure, hint);
    }
}

/// Sends an auto-mode classifier request to the cloud pin the way the core
/// dispatches a model-pin turn. None (the core takes the request) when the
/// pin's credential is not in this process's environment.
async fn send_classifier_to_cloud(
    state: &Gateway,
    pin: &crate::hybrid_classifier::CloudPin,
    parts: &hyper::http::request::Parts,
    body: &[u8],
) -> Result<Option<Response<Body>>> {
    let Some(key) = std::env::var(&pin.auth_env)
        .ok()
        .filter(|key| !key.is_empty())
    else {
        return Ok(None);
    };
    let Some(pinned) = crate::hybrid_classifier::pinned_body(body, &pin.wire_model) else {
        return Ok(None);
    };
    let mut outgoing = state
        .client
        .post(&pin.url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("anthropic-version", "2023-06-01")
        .header("x-workshop-required-model", &pin.wire_model);
    for name in ["anthropic-beta", "x-claude-code-session-id"] {
        if let Some(value) = parts.headers.get(name) {
            outgoing = outgoing.header(name, value);
        }
    }
    if let Some(session) = parts.headers.get("x-claude-code-session-id") {
        outgoing = outgoing.header("x-conversation-id", session);
    }
    let upstream = outgoing.bearer_auth(key).body(pinned).send().await?;
    let status = upstream.status();
    tracing::info!(
        model = %pin.wire_model,
        status = status.as_u16(),
        "auto-mode classifier sent to the cloud pin"
    );
    if !status.is_success() {
        // The router's message can echo request content, so only its error
        // type is logged.
        let headers = upstream.headers().clone();
        let error_body = upstream.bytes().await?;
        let error_type = serde_json::from_slice::<serde_json::Value>(&error_body)
            .ok()
            .and_then(|error| {
                error
                    .pointer("/error/type")
                    .and_then(serde_json::Value::as_str)
                    .filter(|kind| {
                        kind.len() <= 64
                            && kind
                                .bytes()
                                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                    })
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        tracing::warn!(status = status.as_u16(), %error_type, "cloud pin refused the auto-mode classifier");
        return buffered_response(
            state,
            status,
            &headers,
            error_body,
            None,
            RetryHint::default(),
        )
        .map(Some);
    }
    stream_response(upstream).map(Some)
}

async fn send_to_core(
    state: &Gateway,
    parts: &hyper::http::request::Parts,
    body: Bytes,
) -> reqwest::Result<reqwest::Response> {
    let path = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/");
    let mut outgoing = state
        .client
        .request(parts.method.clone(), format!("{}{path}", state.core_url));
    for (name, value) in &parts.headers {
        if !hop_header(name.as_str())
            && !matches!(
                name.as_str(),
                "authorization" | "x-api-key" | "host" | "content-length"
            )
        {
            outgoing = outgoing.header(name, value);
        }
    }
    if !parts.headers.contains_key("x-rayline-session-id") {
        for name in [
            "x-conversation-id",
            "x-parent-session-id",
            "x-session-affinity",
            "x-session-id",
            "x-claude-code-session-id",
        ] {
            if let Some(value) = parts.headers.get(name) {
                outgoing = outgoing.header("x-rayline-session-id", value);
                break;
            }
        }
    }
    if !parts.headers.contains_key("x-rayline-branch-id") {
        let branch = parts
            .headers
            .get("thread-id")
            .or_else(|| parts.headers.get("x-claude-code-agent-id"))
            .or_else(|| {
                parts
                    .headers
                    .contains_key("x-parent-session-id")
                    .then(|| {
                        parts
                            .headers
                            .get("x-session-affinity")
                            .or_else(|| parts.headers.get("x-session-id"))
                    })
                    .flatten()
            });
        outgoing = outgoing.header(
            "x-rayline-branch-id",
            branch
                .cloned()
                .unwrap_or_else(|| hyper::header::HeaderValue::from_static("main")),
        );
    }
    outgoing
        .bearer_auth(&state.core_token)
        .body(body)
        .send()
        .await
}

fn stream_response(upstream: reqwest::Response) -> Result<Response<Body>> {
    let mut response = Response::builder().status(upstream.status());
    for (name, value) in upstream.headers() {
        if !hop_header(name.as_str()) {
            response = response.header(name, value);
        }
    }
    // Direct stream: downstream cancellation drops reqwest's response immediately.
    let stream = upstream
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(io::Error::other);
    Ok(response.body(BodyExt::boxed(StreamBody::new(stream)))?)
}

/// Why the core could not get a decision, from its error message.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DecisionsFailure {
    /// Decisions refused the router key.
    Auth,
    /// Decisions was unreachable, timed out, or answered 408, 409 or 5xx
    /// (e.g. a cold cell): it may clear on a resend.
    Unavailable,
    /// Decisions answered 429: the caller is over its admission share.
    Throttled,
    /// Decisions answered another 4xx (e.g. 400 `unknown_price_model`, 422
    /// `decide_refused`). Final: the same turn would be refused again.
    Refused,
}

/// The core's `decision_failed` message (`error.message`, either wire
/// format), cut to a log-sized length. The messages are the core's own
/// (`Decide`): a status and a code, never request content.
fn decisions_message(body: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = value.pointer("/error/message")?.as_str()?;
    Some(message.chars().take(160).collect())
}

/// The part of a core 502's message that is safe to log: the routing
/// service's status and code from the core's own `Decide` messages, else
/// `"other"`, since any other message may be a provider's text.
fn decisions_detail(body: &[u8]) -> String {
    let other = || "other".to_owned();
    let Some(message) = decisions_message(body) else {
        return other();
    };
    if message == "Decisions request failed" {
        return "request failed".to_owned();
    }
    let Some(rest) = message.strip_prefix("Decisions request refused (HTTP ") else {
        return other();
    };
    let status = rest.get(..3).unwrap_or_default();
    // The code is the service's contract code, `^[a-z][a-z0-9_]{0,63}$`.
    let code = rest
        .get(3..)
        .and_then(|rest| rest.strip_prefix(", "))
        .and_then(|rest| rest.split(')').next())
        .filter(|code| {
            code.len() <= 64
                && code.starts_with(|c: char| c.is_ascii_lowercase())
                && code
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        });
    match (
        status.chars().all(|c| c.is_ascii_digit()) && status.len() == 3,
        code,
    ) {
        (true, Some(code)) if !code.is_empty() => format!("refused {status} {code}"),
        (true, _) => format!("refused {status}"),
        _ => other(),
    }
}

/// Classify the core's `decision_failed` error, in either wire format
/// (`error.message`). The messages are the core's own (`Decide`).
fn decisions_failure(body: &[u8]) -> Option<DecisionsFailure> {
    let message = decisions_message(body)?;
    let message = message.as_str();
    if message == "Decisions request failed" {
        return Some(DecisionsFailure::Unavailable);
    }
    let status = message.strip_prefix("Decisions request refused (HTTP ")?;
    match status.get(..3)? {
        "401" | "403" => Some(DecisionsFailure::Auth),
        "429" => Some(DecisionsFailure::Throttled),
        // A timeout or a conflict may clear on a resend, like a 5xx.
        "408" | "409" => Some(DecisionsFailure::Unavailable),
        code if code.starts_with('5') => Some(DecisionsFailure::Unavailable),
        code if code.starts_with('4') => Some(DecisionsFailure::Refused),
        _ => None,
    }
}

/// The routing service's retry advice, as the core passes it on with its 502:
/// `retry-after` (delta-seconds) and `x-should-retry` (`true`/`false`, the
/// Anthropic and OpenAI SDKs' own header). Absent from a core that does not
/// forward them, and then rld's defaults apply.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct RetryHint {
    retry_after: Option<Duration>,
    should_retry: Option<bool>,
}

impl RetryHint {
    fn from_headers(headers: &reqwest::header::HeaderMap) -> Self {
        let text = |name: &str| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
        };
        Self {
            // Only the delta-seconds form; an HTTP date is ignored.
            retry_after: text("retry-after")
                .and_then(|value| value.parse::<u64>().ok())
                .map(Duration::from_secs),
            should_retry: match text("x-should-retry") {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            },
        }
    }
}

/// How long rld waits before its one resend of a failed turn, or None when it
/// does not resend: only an unavailable or busy service is worth another try,
/// never when the service says not to, and never past
/// [`DECISIONS_MAX_RETRY_WAIT`] (the harness gets that wait instead).
fn rld_retry_delay(failure: DecisionsFailure, hint: RetryHint) -> Option<Duration> {
    if !matches!(
        failure,
        DecisionsFailure::Unavailable | DecisionsFailure::Throttled
    ) || hint.should_retry == Some(false)
    {
        return None;
    }
    match hint.retry_after {
        Some(wait) if wait > DECISIONS_MAX_RETRY_WAIT => None,
        Some(wait) => Some(wait.max(Duration::from_secs(1))),
        None => Some(jittered(DECISIONS_RETRY_DELAY, rand::random::<f64>())),
    }
}

/// `base` scaled into [0.5, 1.5) by `unit` in [0, 1).
fn jittered(base: Duration, unit: f64) -> Duration {
    base.mul_f64(0.5 + unit.clamp(0.0, 1.0))
}

fn login_command(env_name: Option<&str>) -> String {
    match env_name {
        Some(env) if !env.is_empty() => format!("raydock --env {env} auth login"),
        _ => "raydock auth login".to_owned(),
    }
}

/// The core's error response, with a Decisions failure explained: what to
/// run for a refused key, and that an unavailable service is retryable.
///
/// An explained failure always carries `x-should-retry: false`: rld has
/// already spent the turn's one resend (or the service ruled one out), and
/// harness SDKs otherwise resend a 5xx or 429 up to about ten times, each a
/// fresh lookup on the shared routing service. A retryable one also carries
/// `retry-after` (the service's own, else 30 s) for a user or tool that
/// sends the turn again by hand.
fn buffered_response(
    state: &Gateway,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Bytes,
    failure: Option<DecisionsFailure>,
    hint: RetryHint,
) -> Result<Response<Body>> {
    let explained = failure.and_then(|failure| {
        explain_decisions_failure(&body, failure, &state.login_command).map(|body| (failure, body))
    });
    let mut response;
    let body = match explained {
        Some((failure, body)) => {
            let (status, retryable) = harness_status(failure);
            response = Response::builder().status(status);
            for (name, value) in headers {
                if !hop_header(name.as_str())
                    && !matches!(
                        name.as_str(),
                        "content-length" | "retry-after" | "x-should-retry"
                    )
                {
                    response = response.header(name, value);
                }
            }
            response = response.header("x-should-retry", "false");
            if retryable {
                // At least the second rld itself waits.
                let wait = hint
                    .retry_after
                    .unwrap_or(DEFAULT_HARNESS_RETRY_AFTER)
                    .max(Duration::from_secs(1));
                response = response.header("retry-after", wait.as_secs().to_string());
            }
            body
        }
        None => {
            response = Response::builder().status(status);
            for (name, value) in headers {
                if !hop_header(name.as_str()) && name.as_str() != "content-length" {
                    response = response.header(name, value);
                }
            }
            body
        }
    };
    Ok(response.body(Full::new(body).map_err(|never| match never {}).boxed())?)
}

/// The status a harness gets for a Decisions failure, and whether it carries
/// `retry-after`. Only an unavailable or throttled service is worth sending
/// again: harness SDKs (Claude Code, the OpenAI and Anthropic SDKs) retry 5xx
/// and 429 with backoff, up to about ten times, so passing a refusal through
/// as the core's 502 turns one refused turn into a stream of identical
/// refused lookups on the shared routing service.
fn harness_status(failure: DecisionsFailure) -> (StatusCode, bool) {
    match failure {
        DecisionsFailure::Unavailable => (StatusCode::SERVICE_UNAVAILABLE, true),
        DecisionsFailure::Throttled => (StatusCode::TOO_MANY_REQUESTS, true),
        // Not 401, which a harness reads as its own (gateway) credential
        // being wrong.
        DecisionsFailure::Auth => (StatusCode::FORBIDDEN, false),
        // A 4xx no SDK retries; the message says why the service refused.
        DecisionsFailure::Refused => (StatusCode::BAD_REQUEST, false),
    }
}

fn explain_decisions_failure(
    body: &[u8],
    failure: DecisionsFailure,
    login_command: &str,
) -> Option<Bytes> {
    let mut value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = value.pointer("/error/message")?.as_str()?.to_owned();
    let explained = match failure {
        DecisionsFailure::Auth => format!(
            "{message}: Decisions rejected the Rayline router key. Run `{login_command}`, then `raydock hybrid stop` and `raydock hybrid start` so the runtime uses the new key."
        ),
        DecisionsFailure::Unavailable => format!(
            "{message}: the Rayline routing service could not decide this turn right now (it may be unavailable, or a development cell may still be starting, which takes about a minute). This is retryable: send the turn again after the retry-after interval."
        ),
        DecisionsFailure::Throttled => format!(
            "{message}: the Rayline routing service is busy. Send the turn again after the retry-after interval."
        ),
        DecisionsFailure::Refused => format!(
            "{message}: the Rayline routing service refused this turn, and sending it again will not change that. If it names a model or price, check `hybrid.local_registry_model` in settings and run `raydock hybrid stop` and `raydock hybrid start`."
        ),
    };
    *value.pointer_mut("/error/message")? = serde_json::Value::String(explained);
    Some(Bytes::from(value.to_string()))
}

fn hop_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(value.to_string()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response.headers_mut().insert(
        "content-type",
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn error_response(status: StatusCode, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({"error":{"type":"hybrid_gateway_error","message":message}}),
    )
}

fn token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn write_private_new(path: &Path, bytes: &[u8]) -> Result<()> {
    // Publish a fully written file atomically without replacing an existing instance.
    let pending = path.with_extension(format!("pending-{}", token()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&pending)
        .with_context(|| format!("create {}", path.display()))?;
    let outcome = (|| {
        // Owner-only before any content is written; the hard link shares it.
        #[cfg(windows)]
        rayline_local_router::windows_state::restrict_to_owner(&pending, false)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::hard_link(&pending, path)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(pending);
    outcome
}

struct StateFiles {
    token: PathBuf,
    ready: Option<PathBuf>,
}
impl Drop for StateFiles {
    fn drop(&mut self) {
        if let Some(path) = &self.ready {
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_file(&self.token);
    }
}

fn shutdown_signal() -> Result<impl std::future::Future<Output = Result<()>>> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        Ok(async move {
            tokio::select! { _ = term.recv() => {}, _ = interrupt.recv() => {} };
            Ok(())
        })
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
        let mut interrupt = ctrl_c()?;
        let mut brk = ctrl_break()?;
        let mut close = ctrl_close()?;
        let mut shutdown = ctrl_shutdown()?;
        // `raydock hybrid` runs rld without a console it can signal, so it
        // asks for a stop through this event instead.
        let event = rayline_local_router::windows_state::StopEvent::create()
            .context("create hybrid stop event")?;
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
        // A plain thread: a blocking-pool task would hold up runtime shutdown.
        std::thread::spawn(move || {
            let _ = stop_tx.send(event.wait());
        });
        Ok(async move {
            tokio::select! {
                _ = interrupt.recv() => {},
                _ = brk.recv() => {},
                _ = close.recv() => {},
                _ = shutdown.recv() => {},
                result = stop_rx => { result.context("hybrid stop event")??; },
            };
            Ok(())
        })
    }
}

async fn stop_child(
    child: &mut Child,
    parent_pipe: &mut Option<tokio::process::ChildStdin>,
) -> Result<()> {
    if child.try_wait()?.is_some() {
        return Ok(());
    }
    // Signal only the child we own, then reap it. Inference servers are external.
    #[cfg(unix)]
    {
        if let Some(pid) = child.id() {
            let result = Command::new("kill")
                .args(["-TERM", &pid.to_string()])
                .status()
                .await?;
            if !result.success() && child.try_wait()?.is_none() {
                bail!("could not signal execution core");
            }
        }
    }
    // Windows has no SIGTERM: closing the parent pipe is the core's request
    // to exit. It is killed only if it does not exit in time.
    #[cfg(windows)]
    drop(parent_pipe.take());
    #[cfg(not(windows))]
    let _ = parent_pipe;
    match tokio::time::timeout(SHUTDOWN_TIMEOUT, child.wait()).await {
        Ok(result) => {
            result?;
        }
        Err(_) => {
            child.kill().await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anthropic(message: &str) -> Vec<u8> {
        json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
            .to_string()
            .into_bytes()
    }

    fn openai(message: &str) -> Vec<u8> {
        json!({"error": {"type": "invalid_request_error", "code": "decision_failed", "message": message}})
            .to_string()
            .into_bytes()
    }

    #[test]
    fn only_the_cores_decisions_status_and_code_are_logged() {
        let body = |message: &str| {
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
                .to_string()
                .into_bytes()
        };
        assert_eq!(
            decisions_detail(&body(
                "Decisions request refused (HTTP 422, projection_refused)"
            )),
            "refused 422 projection_refused"
        );
        assert_eq!(
            decisions_detail(&body("Decisions request refused (HTTP 504)")),
            "refused 504"
        );
        assert_eq!(
            decisions_detail(&body(
                "Decisions request refused (HTTP 422, policy_v2_refused)"
            )),
            "refused 422 policy_v2_refused"
        );
        assert_eq!(
            decisions_detail(&body("Decisions request refused (HTTP 422, 2bad)")),
            "refused 422"
        );
        assert_eq!(
            decisions_detail(&body("Decisions request failed")),
            "request failed"
        );
        // Anything else may be a provider's text, which can quote the prompt.
        assert_eq!(
            decisions_detail(&body("Rejected input: Decisions secret text")),
            "other"
        );
        assert_eq!(
            decisions_detail(&body("Decisions request refused (HTTP 422, Secret text)")),
            "refused 422"
        );
        assert_eq!(decisions_detail(&body("unexpected EOF")), "other");
        assert_eq!(decisions_detail(b"not json"), "other");
    }

    #[test]
    fn classifies_the_cores_decisions_failures() {
        for encode in [anthropic, openai] {
            assert_eq!(
                decisions_failure(&encode("Decisions request refused (HTTP 401)")),
                Some(DecisionsFailure::Auth)
            );
            assert_eq!(
                decisions_failure(&encode(
                    "Decisions request refused (HTTP 403, code policy_x)"
                )),
                Some(DecisionsFailure::Auth)
            );
            for message in [
                "Decisions request refused (HTTP 500)",
                "Decisions request refused (HTTP 502)",
                "Decisions request refused (HTTP 503)",
                "Decisions request refused (HTTP 504)",
                "Decisions request refused (HTTP 408)",
                "Decisions request refused (HTTP 409, code conflict)",
                "Decisions request failed",
            ] {
                assert_eq!(
                    decisions_failure(&encode(message)),
                    Some(DecisionsFailure::Unavailable),
                    "{message}"
                );
            }
            assert_eq!(
                decisions_failure(&encode("Decisions request refused (HTTP 429)")),
                Some(DecisionsFailure::Throttled)
            );
            for message in [
                "Decisions request refused (HTTP 400)",
                "Decisions request refused (HTTP 400, unknown_price_model)",
                "Decisions request refused (HTTP 422, code decide_refused)",
                "Decisions request refused (HTTP 404)",
            ] {
                assert_eq!(
                    decisions_failure(&encode(message)),
                    Some(DecisionsFailure::Refused),
                    "{message}"
                );
            }
            for message in [
                "Decisions request refused (HTTP 302)",
                "selected endpoint request failed",
            ] {
                assert_eq!(decisions_failure(&encode(message)), None, "{message}");
            }
        }
        assert_eq!(decisions_failure(b"not json"), None);
    }

    #[test]
    fn auth_failure_names_the_login_command() {
        let explained = explain_decisions_failure(
            &anthropic("Decisions request refused (HTTP 401)"),
            DecisionsFailure::Auth,
            &login_command(Some("dev")),
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&explained).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(
            message.starts_with("Decisions request refused (HTTP 401): "),
            "{message}"
        );
        assert!(
            message.contains("`raydock --env dev auth login`"),
            "{message}"
        );
        // The wire shape is kept.
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(login_command(None), "raydock auth login");
    }

    #[test]
    fn only_unavailable_or_throttled_decisions_reach_the_harness_as_retryable() {
        assert_eq!(
            harness_status(DecisionsFailure::Unavailable),
            (StatusCode::SERVICE_UNAVAILABLE, true)
        );
        assert_eq!(
            harness_status(DecisionsFailure::Throttled),
            (StatusCode::TOO_MANY_REQUESTS, true)
        );
        for failure in [DecisionsFailure::Auth, DecisionsFailure::Refused] {
            let (status, retry_after) = harness_status(failure);
            assert!(!retry_after, "{failure:?}");
            // Harness SDKs retry 408, 409, 429 and 5xx; none of those.
            assert!(status.is_client_error(), "{failure:?}");
            assert!(
                ![408, 409, 429].contains(&status.as_u16()),
                "{failure:?}: {status}"
            );
        }
    }

    #[test]
    fn reads_the_services_retry_advice() {
        let mut headers = reqwest::header::HeaderMap::new();
        assert_eq!(RetryHint::from_headers(&headers), RetryHint::default());
        headers.insert("retry-after", " 7 ".parse().unwrap());
        headers.insert("x-should-retry", "false".parse().unwrap());
        assert_eq!(
            RetryHint::from_headers(&headers),
            RetryHint {
                retry_after: Some(Duration::from_secs(7)),
                should_retry: Some(false)
            }
        );
        // An HTTP date, a negative or fractional delay, and anything but
        // true/false are ignored.
        for (retry_after, should_retry) in [
            ("Wed, 21 Oct 2026 07:28:00 GMT", "False"),
            ("-1", "1"),
            ("1.5", ""),
        ] {
            headers.insert("retry-after", retry_after.parse().unwrap());
            headers.insert("x-should-retry", should_retry.parse().unwrap());
            assert_eq!(RetryHint::from_headers(&headers), RetryHint::default());
        }
    }

    #[test]
    fn rld_resends_only_an_unavailable_or_busy_turn_the_service_allows() {
        let hint = |retry_after: Option<u64>, should_retry: Option<bool>| RetryHint {
            retry_after: retry_after.map(Duration::from_secs),
            should_retry,
        };
        for failure in [DecisionsFailure::Unavailable, DecisionsFailure::Throttled] {
            assert_eq!(
                rld_retry_delay(failure, hint(Some(3), None)),
                Some(Duration::from_secs(3))
            );
            assert_eq!(
                rld_retry_delay(failure, hint(Some(3), Some(true))),
                Some(Duration::from_secs(3))
            );
            // At least a second, at most the cap.
            assert_eq!(
                rld_retry_delay(failure, hint(Some(0), None)),
                Some(Duration::from_secs(1))
            );
            assert_eq!(
                rld_retry_delay(failure, hint(Some(30), None)),
                Some(DECISIONS_MAX_RETRY_WAIT)
            );
            assert_eq!(rld_retry_delay(failure, hint(Some(31), None)), None);
            assert_eq!(rld_retry_delay(failure, hint(Some(3), Some(false))), None);
            assert_eq!(rld_retry_delay(failure, hint(None, Some(false))), None);
            let default = rld_retry_delay(failure, hint(None, None)).unwrap();
            assert!(
                default >= DECISIONS_RETRY_DELAY / 2 && default < DECISIONS_RETRY_DELAY * 3 / 2,
                "{default:?}"
            );
        }
        for failure in [DecisionsFailure::Auth, DecisionsFailure::Refused] {
            for advice in [hint(None, None), hint(Some(1), Some(true))] {
                assert_eq!(rld_retry_delay(failure, advice), None, "{failure:?}");
            }
        }
    }

    #[test]
    fn jitter_spans_half_to_one_and_a_half_times_the_base() {
        let base = Duration::from_secs(4);
        assert_eq!(jittered(base, 0.0), Duration::from_secs(2));
        assert_eq!(jittered(base, 0.5), Duration::from_secs(4));
        assert!(jittered(base, 0.999_999) < Duration::from_secs(6));
        assert_eq!(jittered(base, -3.0), Duration::from_secs(2));
    }

    #[test]
    fn refused_failure_says_not_to_resend() {
        let explained = explain_decisions_failure(
            &anthropic("Decisions request refused (HTTP 400, unknown_price_model)"),
            DecisionsFailure::Refused,
            "raydock auth login",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&explained).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("unknown_price_model"), "{message}");
        assert!(message.contains("will not change"), "{message}");
    }

    #[test]
    fn unavailable_failure_says_retryable() {
        let explained = explain_decisions_failure(
            &openai("Decisions request refused (HTTP 502)"),
            DecisionsFailure::Unavailable,
            "raydock auth login",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&explained).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("retryable"), "{message}");
        assert_eq!(value["error"]["code"], "decision_failed");
    }
}
