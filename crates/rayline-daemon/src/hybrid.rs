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
/// service unavailable (a scale-to-zero cell answers 502 while it starts).
const DECISIONS_RETRY_DELAY: Duration = Duration::from_secs(5);

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
}

type Body = http_body_util::combinators::BoxBody<Bytes, io::Error>;

pub async fn run(args: Args) -> Result<()> {
    check_decisions_url(&args.config, &args.router_urls)?;
    let credential_env = credential_env_names(&args.config)?;
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
    let _parent_pipe = child
        .stdin
        .take()
        .context("execution core parent pipe unavailable")?;
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
    stop_child(&mut child)
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
        if failure == Some(DecisionsFailure::Unavailable) && !retried {
            // Nothing ran: the core decides before it dispatches, and treats an
            // identical resend like the harness's own retry.
            retried = true;
            tracing::info!("Decisions unavailable; retrying the turn once");
            tokio::time::sleep(DECISIONS_RETRY_DELAY).await;
            continue;
        }
        return buffered_response(state, status, &headers, error_body, failure);
    }
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
    /// Decisions was unreachable or answered 502/503/504 (e.g. a cold cell).
    Unavailable,
}

/// Classify the core's `decision_failed` error, in either wire format
/// (`error.message`). The messages are the core's own (`Decide`).
fn decisions_failure(body: &[u8]) -> Option<DecisionsFailure> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let message = value.pointer("/error/message")?.as_str()?;
    if message == "Decisions request failed" {
        return Some(DecisionsFailure::Unavailable);
    }
    let status = message.strip_prefix("Decisions request refused (HTTP ")?;
    match status.get(..3)? {
        "401" | "403" => Some(DecisionsFailure::Auth),
        "502" | "503" | "504" => Some(DecisionsFailure::Unavailable),
        _ => None,
    }
}

fn login_command(env_name: Option<&str>) -> String {
    match env_name {
        Some(env) if !env.is_empty() => format!("rayline --env {env} auth login"),
        _ => "rayline auth login".to_owned(),
    }
}

/// The core's error response, with a Decisions failure explained: what to
/// run for a refused key, and that an unavailable service is retryable.
fn buffered_response(
    state: &Gateway,
    status: StatusCode,
    headers: &reqwest::header::HeaderMap,
    body: Bytes,
    failure: Option<DecisionsFailure>,
) -> Result<Response<Body>> {
    let (status, body, retry_after) = match failure.and_then(|failure| {
        explain_decisions_failure(&body, failure, &state.login_command).map(|body| (failure, body))
    }) {
        Some((DecisionsFailure::Unavailable, body)) => {
            (StatusCode::SERVICE_UNAVAILABLE, body, true)
        }
        Some((DecisionsFailure::Auth, body)) => (status, body, false),
        None => (status, body, false),
    };
    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        if !hop_header(name.as_str()) && name.as_str() != "content-length" {
            response = response.header(name, value);
        }
    }
    if retry_after {
        response = response.header("retry-after", "30");
    }
    Ok(response.body(Full::new(body).map_err(|never| match never {}).boxed())?)
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
            "{message}: Decisions rejected the Rayline router key. Run `{login_command}`, then `rayline hybrid stop` and `rayline hybrid start` so the runtime uses the new key."
        ),
        DecisionsFailure::Unavailable => format!(
            "{message}: the Rayline routing service is unavailable or still starting (a development cell scales to zero and takes about a minute to start). This is retryable: send the turn again shortly."
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
    #[cfg(not(unix))]
    {
        Ok(async {
            tokio::signal::ctrl_c().await?;
            Ok(())
        })
    }
}

async fn stop_child(child: &mut Child) -> Result<()> {
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
    #[cfg(not(unix))]
    {
        child.start_kill()?;
    }
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
                "Decisions request refused (HTTP 502)",
                "Decisions request refused (HTTP 503)",
                "Decisions request failed",
            ] {
                assert_eq!(
                    decisions_failure(&encode(message)),
                    Some(DecisionsFailure::Unavailable),
                    "{message}"
                );
            }
            for message in [
                "Decisions request refused (HTTP 400)",
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
            message.contains("`rayline --env dev auth login`"),
            "{message}"
        );
        // The wire shape is kept.
        assert_eq!(value["type"], "error");
        assert_eq!(value["error"]["type"], "invalid_request_error");
        assert_eq!(login_command(None), "rayline auth login");
    }

    #[test]
    fn unavailable_failure_says_retryable() {
        let explained = explain_decisions_failure(
            &openai("Decisions request refused (HTTP 502)"),
            DecisionsFailure::Unavailable,
            "rayline auth login",
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_slice(&explained).unwrap();
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("retryable"), "{message}");
        assert_eq!(value["error"]["code"], "decision_failed");
    }
}
