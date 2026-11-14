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
/// How long a stopping encoder may take to save its sessions (about 0.2 s
/// per 21k-token session) before it is killed. `rayline local stop` waits
/// for this and the core's stop.
const SAVE_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a turn waits for its local decision before it routes by
/// fallback. The core gives up on a Decisions request after about 90
/// seconds and fails the turn, so this ends well inside that: a hung
/// encoder costs the turn its decision, not the turn.
const DECISION_DEADLINE: Duration = Duration::from_secs(60);
/// The most decisions computed at once. A decision past its deadline keeps
/// its worker until it finishes; beyond this many, a turn routes by
/// fallback at once, so stuck work never piles up.
const MAX_DECISIONS_IN_FLIGHT: usize = 2;
/// The most times the encoder is restarted after exiting within
/// [`RESTART_WINDOW`]; past it, decisions route by fallback until the local
/// process restarts.
const MAX_RESTARTS: usize = 3;
const RESTART_WINDOW: Duration = Duration::from_secs(600);

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

    /// Keep the encoder running; never returns. The first time it refuses
    /// an input as longer than its context, it is restarted once at the
    /// full context: it saves its sessions as it stops, and the client
    /// resumes them. One that exits is restarted after a backoff, at most
    /// [`MAX_RESTARTS`] times in [`RESTART_WINDOW`]. Decisions asked while
    /// it is down fail as `backend_unavailable` and route by fallback.
    pub async fn supervise(&mut self) {
        let mut exits: std::collections::VecDeque<tokio::time::Instant> = Default::default();
        loop {
            let grow = tokio::select! {
                status = self.child.wait() => {
                    let code = status.ok().and_then(|status| status.code());
                    tracing::warn!(?code, "local ARC encoder exited; decisions route by fallback");
                    false
                }
                () = self.full.notified(), if self.context < self.full_context() => true,
            };
            let context = if grow {
                self.full_context()
            } else {
                self.context
            };
            let mut attempt = if grow { Ok(()) } else { Err(()) };
            loop {
                if attempt.is_err() {
                    let now = tokio::time::Instant::now();
                    exits.retain(|at| now.duration_since(*at) < RESTART_WINDOW);
                    if exits.len() == MAX_RESTARTS {
                        tracing::error!(
                            restarts = MAX_RESTARTS,
                            "local ARC encoder keeps failing; decisions route by fallback \
                             until the local process restarts"
                        );
                        return std::future::pending().await;
                    }
                    exits.push_back(now);
                    tokio::time::sleep(Duration::from_secs(1 << exits.len())).await;
                }
                match self.restart(context).await {
                    Ok(()) => break,
                    Err(error) => {
                        tracing::warn!(%error, "local ARC encoder restart failed");
                        attempt = Err(());
                    }
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

/// Supervise the encoder; never resolves.
pub async fn supervise_encoder(encoder: &mut Option<Encoder>) {
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

/// Decides in this process with a loaded release and the local encoder.
struct LocalDecider {
    release: Arc<rayline_arc_decide::Release>,
    encoder: rayline_arc_decide::LocalEncoder,
    /// [`MAX_DECISIONS_IN_FLIGHT`] permits; a decision holds one until its
    /// work ends, also past its deadline.
    in_flight: Arc<tokio::sync::Semaphore>,
}

/// The refusal for a decision that failed: one the turn routes by fallback
/// (503) unless the request is malformed or the policy declines it.
fn refusal(error: &rayline_arc_decide::DecideError) -> rayline_arc_routes::Refusal {
    if error.routes_by_fallback() {
        rayline_arc_routes::Refusal::with_status(503, error.code(), error.to_string())
    } else {
        rayline_arc_routes::Refusal::new(error.code(), error.to_string())
    }
}

impl LocalDecider {
    /// The caller price rows this release reads. Rows for models it doesn't
    /// know are reported unused in the answer, not sent to the decision,
    /// which refuses them.
    fn read_prices(
        &self,
        prices: Option<rayline_arc_decide::RoutingPrices>,
    ) -> Option<rayline_arc_decide::RoutingPrices> {
        let mut prices = prices?;
        prices
            .rates
            .retain(|key, _| self.release.reads_price_key(key));
        (!prices.rates.is_empty()).then_some(prices)
    }

    async fn decide_body(
        &self,
        body: &str,
    ) -> Result<serde_json::Value, rayline_arc_routes::Refusal> {
        let unavailable = |code: &str, message: &str| {
            rayline_arc_routes::Refusal::with_status(503, code, message)
        };
        let mut input: rayline_arc_decide::DecideInput = serde_json::from_str(body)
            .map_err(|_| rayline_arc_routes::Refusal::new("invalid_request", "decide body"))?;
        input.routing_prices = self.read_prices(input.routing_prices.take());
        let permit = self
            .in_flight
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable("busy", "local decisions are all in use"))?;
        let (release, encoder) = (self.release.clone(), self.encoder.clone());
        // The decision encodes and scores on the CPU and blocks on the
        // encoder; keep it off the async workers. Past the deadline its
        // answer is not waited for: the turn routes by fallback, and the
        // work finishes unread, holding its permit until then.
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            rayline_arc_decide::decide(&release, &input, &encoder)
        });
        let output = tokio::time::timeout(DECISION_DEADLINE, task)
            .await
            .map_err(|_| {
                rayline_arc_routes::Refusal::with_status(
                    504,
                    "timeout",
                    "the local decision did not finish in time",
                )
            })?
            .map_err(|_| unavailable("internal_error", "the local decision failed"))?
            .map_err(|error| refusal(&error))?;
        serde_json::to_value(output)
            .map_err(|_| unavailable("internal_error", "the local decision's answer"))
    }
}

impl rayline_arc_routes::Decider for LocalDecider {
    fn decide<'a>(&'a self, body: &'a str) -> rayline_arc_routes::DecideFuture<'a> {
        Box::pin(self.decide_body(body))
    }

    fn action_prices(
        &self,
        routing_prices: Option<&rayline_arc_routes::RoutingPrices>,
    ) -> Vec<serde_json::Value> {
        // The routes service's caller table, as the decider reads it.
        let prices = routing_prices
            .and_then(|prices| serde_json::to_value(prices).ok())
            .and_then(|prices| serde_json::from_value(prices).ok());
        let Ok(rows) =
            rayline_arc_decide::action_prices(&self.release, self.read_prices(prices).as_ref())
        else {
            return Vec::new();
        };
        rows.into_iter()
            .filter_map(|(action_id, legs, price_key)| {
                let legs = serde_json::json!({
                    "input": legs.input?, "cache_read": legs.cache_read?,
                    "cache_write": legs.cache_write?, "output": legs.output?,
                });
                Some(serde_json::json!({
                    "action_id": action_id, "price_per_mtok": legs, "price_key": price_key,
                }))
            })
            .collect()
    }
}

/// The salt every episode hash is mixed with, kept in `state_dir` so a
/// conversation names the same encoder session after a restart, and its
/// saved state resumes. Made once, owner-only.
fn session_salt(state_dir: &Path) -> Result<String> {
    let path = state_dir.join("episode-salt");
    match std::fs::read_to_string(&path) {
        Ok(salt) if salt.len() == 32 && salt.bytes().all(|b| b.is_ascii_hexdigit()) => {
            return Ok(salt);
        }
        Ok(_) => bail!("{} is not a salt; remove it", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("read {}", path.display())),
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("create {}", state_dir.display()))?;
    let salt = format!("{:032x}", rand::random::<u128>());
    crate::hybrid::write_private_new(&path, salt.as_bytes())?;
    Ok(salt)
}

/// Serve the Routes API for the core on a free loopback port, deciding with
/// `package` and the release in `release_dir` through `encoder`; any other
/// path goes to `upstream` (the environment's router). Returns its
/// `/v1/routes` URL.
pub async fn serve_routes(
    package: &Path,
    alias: &str,
    release_dir: &Path,
    encoder: &Encoder,
    upstream: &str,
    access: RoutesAccess,
) -> Result<(String, tokio::task::JoinHandle<Result<()>>)> {
    let manifest = std::fs::read(package).with_context(|| format!("read {}", package.display()))?;
    let catalog = rayline_arc_routes::Catalog::from_manifest(&manifest, alias)?;
    let mut config = rayline_arc_routes::RouterConfig::new(catalog, upstream);
    config.salt = session_salt(&encoder.files.state_dir)?;
    config.access_token = Some(access.token);
    config.upstream_authorization = access.upstream_authorization;
    let dir = release_dir.to_path_buf();
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
    let decider = Arc::new(LocalDecider {
        release: Arc::new(release),
        encoder: encoder.client(),
        in_flight: Arc::new(tokio::sync::Semaphore::new(MAX_DECISIONS_IN_FLIGHT)),
    });
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

    const DEFAULT_BUDGET_TOKENS: u32 = 8_192;

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
                () = encoder.supervise() => panic!("supervision ended"),
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
    fn only_a_bad_request_or_a_declining_policy_refuses_the_turn() {
        use rayline_arc_decide::DecideError;
        let falls_back = |error: DecideError| {
            let refusal = refusal(&error);
            matches!(refusal.status, 502..=504)
        };
        for error in [
            DecideError::Unsupported {
                reason: "request_format_unsupported",
            },
            DecideError::Projection("tokenizer".into()),
            DecideError::TooLarge("text".into()),
            DecideError::NotEncodable("image".into()),
            DecideError::Readouts("down".into()),
            DecideError::Internal("shape".into()),
        ] {
            assert!(falls_back(error.clone()), "{error:?}");
        }
        let declined = refusal(&DecideError::Refused {
            reason: "held_action_unavailable",
        });
        assert_eq!(
            (declined.status, declined.code.as_str()),
            (400, "held_action_unavailable")
        );
        assert!(!falls_back(DecideError::Invalid { field: "selection" }));
    }

    #[test]
    fn the_episode_salt_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!("arc-salt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let first = session_salt(&dir).expect("made");
        assert_eq!(session_salt(&dir).expect("read"), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("episode-salt"))
                .expect("salt file")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(dir.join("episode-salt"), "not a salt").expect("overwrite");
        assert!(session_salt(&dir).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
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
