//! Local ARC: the encoder the local process runs when routing decisions are
//! made on this machine instead of by a Rayline router.
//!
//! The encoder is a separate binary shipped beside `rayline-core`. It binds a
//! free loopback port, reports it as the first stdout line
//! (`{"listening": PORT}`), and exits when its stdin closes, so it never
//! outlives the local process.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Notify;

/// Where the local encoder's files are.
#[derive(Debug, Clone)]
pub struct EncoderFiles {
    /// The encoder binary.
    pub binary: PathBuf,
    /// The ARC encoder weights in GGUF.
    pub model: PathBuf,
    /// Directory for saved conversation states, so a restart resumes instead
    /// of re-encoding.
    pub state_dir: PathBuf,
}

/// The context, in tokens, the encoder starts with. It holds every
/// conversation in one KV cache, resident for the whole context (~6 KB a
/// token), so this keeps ~0.8 GB less than the full one; a longer
/// conversation restarts the encoder at [`FULL_CONTEXT_TOKENS`]. Both are rounded up to a multiple
/// of the encode chunk.
const CONTEXT_TOKENS: u32 = 131_072;
/// The longest context, in tokens, the encoder holds for one conversation.
const FULL_CONTEXT_TOKENS: u32 = 270_336;
/// Tokens per encode chunk when no release names one (a decide service).
pub const DEFAULT_BUDGET_TOKENS: u32 = 8_192;
/// How long a stopping encoder may take to save its sessions.
const SAVE_TIMEOUT: Duration = Duration::from_secs(120);

/// A running encoder. Dropping it kills the process.
pub struct Encoder {
    child: Child,
    /// Held open: the encoder stops (saving its sessions) when it closes.
    parent_pipe: Option<ChildStdin>,
    /// `http://127.0.0.1:PORT`.
    pub url: String,
    files: EncoderFiles,
    timeout: Duration,
    launch: Launch,
    context: u32,
    /// The decider's client; it follows the encoder across a restart.
    client: rayline_arc_decide::LocalEncoder,
    /// Raised by the client when the encoder refuses an input longer than
    /// its context.
    full: Arc<Notify>,
}

#[derive(Deserialize)]
struct Listening {
    listening: u16,
}

/// How every start of one encoder is configured.
#[derive(Debug, Clone, Copy)]
struct Launch {
    /// Tokens per encode chunk; the encoder also saves a shared-prefix
    /// state at this many tokens (`--checkpoint`).
    budget: u32,
}

/// `tokens` rounded up to a multiple of `chunk`.
fn whole_chunks(tokens: u32, chunk: u32) -> u32 {
    tokens.div_ceil(chunk).saturating_mul(chunk)
}

/// Start the encoder process at `context` and wait until it answers
/// `/health`; returns it, its parent pipe, and its URL.
async fn spawn(
    files: &EncoderFiles,
    launch: Launch,
    context: u32,
    timeout: Duration,
) -> Result<(Child, ChildStdin, String)> {
    std::fs::create_dir_all(&files.state_dir)
        .with_context(|| format!("create {}", files.state_dir.display()))?;
    let mut command = Command::new(&files.binary);
    command.args(["--checkpoint", &launch.budget.to_string()]);
    let mut child = command
        .arg("--model")
        .arg(&files.model)
        .args(["--budget", &launch.budget.to_string()])
        .args(["--ctx", &context.to_string()])
        .args(["--ubatch", "2048", "--host", "127.0.0.1", "--port", "0"])
        .arg("--state-dir")
        .arg(&files.state_dir)
        .arg("--parent-pipe")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("start the ARC encoder {}", files.binary.display()))?;
    let parent_pipe = child.stdin.take().context("encoder stdin unavailable")?;
    let stdout = child.stdout.take().context("encoder stdout unavailable")?;
    let url = tokio::time::timeout(timeout, async {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).await?;
        let Listening { listening } = serde_json::from_str(line.trim()).context(
            "the ARC encoder did not report its port (it may have failed to load the model)",
        )?;
        anyhow::Ok(format!("http://127.0.0.1:{listening}"))
    })
    .await
    .context("the ARC encoder did not start in time")??;
    wait_healthy(&url, timeout).await?;
    Ok((child, parent_pipe, url))
}

async fn wait_healthy(url: &str, timeout: Duration) -> Result<()> {
    let client = reqwest::Client::builder().no_proxy().build()?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match client.get(format!("{url}/health")).send().await {
            Ok(response) if response.status().is_success() => return Ok(()),
            _ if tokio::time::Instant::now() >= deadline => {
                bail!("the ARC encoder at {url} is not healthy")
            }
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
}

impl Encoder {
    /// Start the encoder with `budget` tokens per encode chunk (the
    /// release's stage-one chunk size) and wait until it answers `/health`.
    pub async fn start(files: &EncoderFiles, budget: u32, timeout: Duration) -> Result<Self> {
        Self::start_at(files, budget, timeout, whole_chunks(CONTEXT_TOKENS, budget)).await
    }

    async fn start_at(
        files: &EncoderFiles,
        budget: u32,
        timeout: Duration,
        context: u32,
    ) -> Result<Self> {
        anyhow::ensure!(budget > 0, "the encode chunk must hold at least one token");
        let launch = Launch { budget };
        tracing::info!(budget, "starting the local ARC encoder");
        let (child, parent_pipe, url) = spawn(files, launch, context, timeout).await?;
        let full = Arc::new(Notify::new());
        let raise = full.clone();
        let client = rayline_arc_decide::LocalEncoder::new(&url)
            .map_err(anyhow::Error::msg)?
            .on_context_full(move || raise.notify_one());
        Ok(Self {
            child,
            parent_pipe: Some(parent_pipe),
            url,
            files: files.clone(),
            timeout,
            launch,
            context,
            client,
            full,
        })
    }

    /// The decider's client for this encoder.
    pub fn client(&self) -> rayline_arc_decide::LocalEncoder {
        self.client.clone()
    }

    /// The context a full-context restart uses.
    fn full_context(&self) -> u32 {
        whole_chunks(FULL_CONTEXT_TOKENS, self.launch.budget)
    }

    /// Its process id, for status output.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// Run until the encoder exits. The first time it refuses an input as
    /// longer than its context, it is restarted once at the full context:
    /// it saves its sessions as it stops, and the client resumes them.
    /// Decisions asked during the restart fail as `backend_unavailable`.
    pub async fn supervise(&mut self) -> Result<std::process::ExitStatus> {
        loop {
            tokio::select! {
                status = self.child.wait() => return Ok(status?),
                () = self.full.notified(), if self.context < self.full_context() => {
                    self.restart(self.full_context()).await?;
                }
            }
        }
    }

    /// Stop the encoder so it saves its sessions: close its parent pipe
    /// (it exits when the pipe closes, on every platform) and wait for it,
    /// killing it only when it doesn't exit in time.
    pub async fn stop(&mut self) -> Result<()> {
        drop(self.parent_pipe.take());
        if tokio::time::timeout(SAVE_TIMEOUT, self.child.wait())
            .await
            .is_err()
        {
            tracing::warn!("local ARC encoder did not stop in time; killing it");
            self.child.kill().await?;
        }
        Ok(())
    }

    async fn restart(&mut self, context: u32) -> Result<()> {
        tracing::info!(
            from = self.context,
            to = context,
            "local ARC encoder context full; restarting it"
        );
        self.stop().await?;
        let (child, parent_pipe, url) =
            spawn(&self.files, self.launch, context, self.timeout).await?;
        self.client.set_url(&url).map_err(anyhow::Error::msg)?;
        // Replacing the child drops the old handle; it has exited.
        self.child = child;
        self.parent_pipe = Some(parent_pipe);
        self.url = url;
        self.context = context;
        tracing::info!(url = %self.url, pid = ?self.pid(), "local ARC encoder ready");
        Ok(())
    }
}

/// Supervise the encoder until it exits; never resolves without one.
pub async fn wait_encoder(encoder: &mut Option<Encoder>) -> Result<std::process::ExitStatus> {
    match encoder {
        Some(encoder) => encoder.supervise().await,
        None => std::future::pending().await,
    }
}

/// The env var the core reads the local routes service's token from.
pub const ROUTES_TOKEN_ENV: &str = "RAYLINE_ARC_ROUTES_TOKEN";

/// Who may use the local routes service: callers presenting `token`. Paths
/// it forwards upstream carry `upstream_authorization` instead.
pub struct RoutesAccess {
    pub token: String,
    pub upstream_authorization: Option<String>,
}

impl RoutesAccess {
    /// A fresh token; upstream calls use the credential the core config
    /// names (`decisions.auth_env`), read from this process's environment.
    pub fn for_core(config: &Path) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(config)?)?;
        let upstream_authorization = value["decisions"]["auth_env"]
            .as_str()
            .and_then(|name| std::env::var(name).ok())
            .filter(|key| !key.is_empty())
            .map(|key| format!("Bearer {key}"));
        Ok(Self {
            token: format!(
                "{:032x}{:032x}",
                rand::random::<u128>(),
                rand::random::<u128>()
            ),
            upstream_authorization,
        })
    }
}

/// Where the routes service gets each decision.
pub enum DecideWith {
    /// In this process: the release in this directory, with readouts from
    /// the local encoder this client reaches.
    Release {
        dir: PathBuf,
        encoder: rayline_arc_decide::LocalEncoder,
    },
    /// An external decide service (development).
    Service(String),
}

/// Decides in this process with a loaded release and the local encoder.
struct LocalDecider {
    release: std::sync::Arc<rayline_arc_decide::Release>,
    encoder: rayline_arc_decide::LocalEncoder,
}

impl LocalDecider {
    /// Decide `body`, its `request` member replaced by `request` when given
    /// (the text as sent: the encoder input depends on its key order).
    async fn decide_text(
        &self,
        body: &serde_json::Value,
        request: Option<&str>,
    ) -> Result<serde_json::Value, rayline_arc_routes::Refusal> {
        let refuse = |code: &str, message: String| rayline_arc_routes::Refusal::new(code, message);
        let text = rayline_arc_routes::decide_body_text(body, request)?;
        let mut input: rayline_arc_decide::DecideInput = serde_json::from_str(&text)
            .map_err(|error| refuse("invalid_request", format!("decide body: {error}")))?;
        let release = self.release.clone();
        // Caller price rows for models this release doesn't know are
        // reported unused in the answer, not sent to the decision, which
        // refuses them.
        if let Some(prices) = input.routing_prices.as_mut() {
            prices.rates.retain(|key, _| release.reads_price_key(key));
            if prices.rates.is_empty() {
                input.routing_prices = None;
            }
        }
        let encoder = self.encoder.clone();
        // The decision encodes and scores on the CPU and blocks on the
        // encoder; keep it off the async workers.
        let output = tokio::task::spawn_blocking(move || {
            rayline_arc_decide::decide(&release, &input, &encoder)
        })
        .await
        .map_err(|error| refuse("internal_error", format!("decide task: {error}")))?
        .map_err(|error| match error {
            // The encoder is down or restarting: a server-side outage, as
            // the policy service answers it, not a bad request.
            rayline_arc_decide::DecideError::Readouts(_) => {
                rayline_arc_routes::Refusal::with_status(503, error.code(), error.to_string())
            }
            error => refuse(error.code(), error.to_string()),
        })?;
        serde_json::to_value(output)
            .map_err(|error| refuse("internal_error", format!("decide answer: {error}")))
    }
}

impl rayline_arc_routes::Decider for LocalDecider {
    fn decide<'a>(&'a self, body: &'a serde_json::Value) -> rayline_arc_routes::DecideFuture<'a> {
        Box::pin(self.decide_text(body, None))
    }

    fn decide_with_text<'a>(
        &'a self,
        body: &'a serde_json::Value,
        request: &'a str,
    ) -> rayline_arc_routes::DecideFuture<'a> {
        Box::pin(self.decide_text(body, Some(request)))
    }
}

/// Serve the Routes API for the core on a free loopback port, deciding with
/// `package` as `decide` says; any other path goes to `upstream` (the
/// environment's router). Returns its `/v1/routes` URL.
pub async fn serve_routes(
    package: &Path,
    alias: &str,
    decide: DecideWith,
    upstream: &str,
    access: RoutesAccess,
) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
    use std::sync::Arc;
    let manifest = std::fs::read(package).with_context(|| format!("read {}", package.display()))?;
    let catalog = rayline_arc_routes::Catalog::from_manifest(&manifest, alias)?;
    let mut config = rayline_arc_routes::RouterConfig::new(catalog, upstream);
    config.salt = format!("{:032x}", rand::random::<u128>());
    config.access_token = Some(access.token);
    config.upstream_authorization = access.upstream_authorization;
    let decider: Arc<dyn rayline_arc_routes::Decider> = match decide {
        DecideWith::Service(url) => Arc::new(rayline_arc_routes::HttpDecider::new(&url)?),
        DecideWith::Release { dir, encoder } => {
            let release = tokio::task::spawn_blocking(move || {
                let release = rayline_arc_decide::Release::load(&dir)
                    .with_context(|| format!("load the ARC release in {}", dir.display()))?;
                // A public release carries no prices: its per-model card, if any.
                let card = dir.join("prices.json");
                if card.is_file() {
                    return release
                        .with_model_prices(&card)
                        .with_context(|| format!("load the price card {}", card.display()));
                }
                anyhow::Ok(release)
            })
            .await??;
            Arc::new(LocalDecider {
                release: Arc::new(release),
                encoder,
            })
        }
    };
    let router = Arc::new(rayline_arc_routes::Router::new(config, decider)?);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let task = tokio::spawn(async move { Ok(router.serve(listener).await?) });
    tracing::info!(port, "local ARC routes service ready");
    Ok((format!("http://127.0.0.1:{port}/v1/routes"), task))
}

/// Wait until the routes service stops; never resolves without one.
pub async fn wait_routes(routes: &mut Option<tokio::task::JoinHandle<Result<()>>>) -> Result<()> {
    match routes {
        Some(task) => task.await?,
        None => std::future::pending().await,
    }
}

/// The execution-core configuration with Decisions sent to `routes_url`
/// (the local routes service) instead of the router.
///
/// With `auth_env`, the core authenticates to it with the token in that env
/// var instead of the router credential.
pub fn with_local_decisions(
    config: &Path,
    routes_url: &str,
    auth_env: Option<&str>,
    out: &Path,
) -> Result<()> {
    let mut value: serde_json::Value = serde_json::from_slice(&std::fs::read(config)?)?;
    let decisions = value
        .get_mut("decisions")
        .and_then(serde_json::Value::as_object_mut)
        .context("the core config has no `decisions`")?;
    decisions.insert("url".to_owned(), serde_json::json!(routes_url));
    if let Some(auth_env) = auth_env {
        decisions.insert("auth_env".to_owned(), serde_json::json!(auth_env));
    }
    // A previous run's copy: this file is only ever written here.
    match std::fs::remove_file(out) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("remove {}", out.display())),
    }
    crate::hybrid::write_private_new(out, &serde_json::to_vec(&value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs the encoder binary and weights:
    /// `ARC_ENCODER_BIN=... ARC_ENCODER_MODEL=... cargo test -p rayline-daemon
    /// -- --ignored encoder_restarts`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs ARC_ENCODER_BIN and ARC_ENCODER_MODEL"]
    async fn encoder_restarts_at_full_context_and_resumes_sessions() {
        use rayline_arc_decide::{ReadoutRequest, Readouts};
        let var = |name| PathBuf::from(std::env::var_os(name).expect(name));
        let state = std::env::temp_dir().join(format!("arc-grow-{}", std::process::id()));
        let files = EncoderFiles {
            binary: var("ARC_ENCODER_BIN"),
            model: var("ARC_ENCODER_MODEL"),
            state_dir: state.clone(),
        };
        let mut encoder = Encoder::start_at(
            &files,
            DEFAULT_BUDGET_TOKENS,
            Duration::from_secs(120),
            3 * DEFAULT_BUDGET_TOKENS,
        )
        .await
        .expect("the encoder starts");
        let first_pid = encoder.pid();
        // The encoder runs with --checkpoint at the chunk size.
        let health: serde_json::Value = reqwest::get(format!("{}/health", encoder.url))
            .await
            .expect("health")
            .json()
            .await
            .expect("health JSON");
        assert_eq!(health["checkpoint"], DEFAULT_BUDGET_TOKENS);
        let client = encoder.client().with_stats();
        let tokens: Arc<Vec<u32>> =
            Arc::new((0..32_000u32).map(|i| 1_000 + i * 7 % 90_000).collect());
        let read = |client: rayline_arc_decide::LocalEncoder, upto: usize| {
            let tokens = tokens.clone();
            tokio::task::spawn_blocking(move || {
                client.readouts(&ReadoutRequest {
                    episode: Some("grow"),
                    tokens: &tokens[..upto],
                    offsets: &[upto - 1],
                })
            })
        };
        // Fits: 20k tokens of a 24k context, long enough to be saved (16k).
        let (first, second) = (20_000, 32_000);
        read(client.clone(), first)
            .await
            .expect("join")
            .expect("rows within the context");
        let refused = read(client.clone(), second).await.expect("join");
        assert!(refused.is_err(), "32k tokens exceed a 24k context");
        client.take_stats();

        let grown = tokio::time::timeout(Duration::from_secs(300), async {
            tokio::select! {
                status = encoder.supervise() => panic!("the encoder stopped: {status:?}"),
                rows = async {
                    loop {
                        if let Ok(rows) = read(client.clone(), second).await.expect("join") {
                            break rows;
                        }
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                } => rows,
            }
        })
        .await
        .expect("the encoder grows in time");
        assert_eq!(grown.len(), 1);
        assert_ne!(encoder.pid(), first_pid);
        assert_eq!(encoder.context, encoder.full_context());
        // Only the new tokens went to the encoder: the session resumed from
        // its saved state instead of being encoded again.
        let sent: Vec<usize> = client.take_stats().iter().map(|s| s.tokens).collect();
        assert_eq!(sent, [second - first], "the saved session resumed");
        // Stopping saves the session too: a new encoder resumes it.
        encoder.stop().await.expect("the encoder stops");
        let encoder = Encoder::start_at(
            &files,
            DEFAULT_BUDGET_TOKENS,
            Duration::from_secs(120),
            encoder.full_context(),
        )
        .await
        .expect("the encoder starts again");
        let client = encoder.client().with_stats();
        let more = second + 2_000;
        let tokens: Arc<Vec<u32>> =
            Arc::new((0..more as u32).map(|i| 1_000 + i * 7 % 90_000).collect());
        tokio::task::spawn_blocking({
            let client = client.clone();
            move || {
                client.readouts(&ReadoutRequest {
                    episode: Some("grow"),
                    tokens: &tokens,
                    offsets: &[more - 1],
                })
            }
        })
        .await
        .expect("join")
        .expect("rows after a stop");
        // A new process's client sends the whole input; the encoder
        // restores what it saved at stop instead of encoding it again.
        let restored: Vec<Option<usize>> = client
            .take_stats()
            .iter()
            .map(|s| s.restored_tokens)
            .collect();
        assert!(
            restored.iter().flatten().any(|tokens| *tokens >= second),
            "the session saved at stop resumed: restored {restored:?}"
        );
        drop(encoder);
        let _ = std::fs::remove_dir_all(&state);
    }

    #[test]
    fn contexts_are_whole_encode_chunks() {
        assert_eq!(whole_chunks(131_072, 8_192), 131_072);
        assert_eq!(whole_chunks(270_336, 8_192), 270_336);
        assert_eq!(whole_chunks(131_072, 10_000), 140_000);
    }

    #[test]
    fn local_decisions_replace_only_the_url() {
        let dir = std::env::temp_dir().join(format!("arc-local-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let config = dir.join("core.json");
        std::fs::write(
            &config,
            br#"{"decisions":{"url":"https://api.rayline.ai/v1/routes","model":"arc-swift-1.0"},"deployments":[]}"#,
        )
        .expect("write config");
        let out = dir.join("core-local.json");
        with_local_decisions(&config, "http://127.0.0.1:9/v1/routes", None, &out).expect("rewrite");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&out).expect("read")).expect("json");
        assert_eq!(value["decisions"]["url"], "http://127.0.0.1:9/v1/routes");
        assert_eq!(value["decisions"]["model"], "arc-swift-1.0");
        assert_eq!(value["deployments"], serde_json::json!([]));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
