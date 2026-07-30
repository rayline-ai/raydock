// Local-router runtime.
//
// Manages a llama-server child, serves the Anthropic↔OpenAI adapter on
// 127.0.0.1:20808, and runs a header-injecting reverse proxy on
// 127.0.0.1:20809 in front of the cloud router. Together these
// let Claude Code (unmodified) route eligible turns to a local model.
//
// Also exposes a cloud-only transparent proxy mode for Claude Code OAuth
// compatibility.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use std::{convert::Infallible, net::SocketAddr};

use anyhow::{Context, Result, anyhow};
use bytes::Bytes;
use clap::{Parser, Subcommand, ValueEnum};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_hf::DownloadProgress;
use rayline_metrics::{
    LlamaPerfSnapshot, MetricsSink, MetricsUpdate, RouterMetrics, SharedMetricsSink, now_unix_ms,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{info, warn};

mod statusline;

// Pinned llama.cpp release. Kept to the newest build that is >=7 days old
// (supply-chain hygiene) and past the Metal Gated-DeltaNet kernel (PR #20361,
// ~Mar 2026) that accelerates Qwen3-Next/qwen35moe recurrent layers.
const DEFAULT_LLAMA_TAG: &str = "b9585";
const DEFAULT_CTX_SIZE: u32 = 131072;
const DEFAULT_LOCAL_MODEL_ID: &str = "qwen3.6-35b-a3b-q4-k-m";
const DEFAULT_PROXY_CA_CERT: &str = "proxy-ca.pem";
const DEFAULT_PROXY_CA_KEY: &str = "proxy-ca-key.pem";
const RAYLINE_DAEMON_BIN_NAME: &str = "rld";
const RAYLINE_CONFIG_DIR_NAME: &str = "rayline";
const RAYLINE_DOT_CONFIG_DIR: &str = ".rayline";
const RAYLINE_ROUTER_DEFAULT_URL: &str = "https://api.rayline.ai";
const MODEL_REPO_ENV: &str = "RAYLINE_MODEL_REPO";
const MODEL_FILE_ENV: &str = "RAYLINE_MODEL_FILE";
const MODEL_REVISION_ENV: &str = "RAYLINE_MODEL_REVISION";
const MODEL_SHA256_ENV: &str = "RAYLINE_MODEL_SHA256";
const LLAMA_TAG_ENV: &str = "RAYLINE_LLAMA_TAG";
const CTX_SIZE_ENV: &str = "RAYLINE_CTX_SIZE";
const ADAPTER_PORT_ENV: &str = "RAYLINE_ADAPTER_PORT";
const INJECTOR_PORT_ENV: &str = "RAYLINE_INJECTOR_PORT";
const PROXY_PORT_ENV: &str = "RAYLINE_PROXY_PORT";
const ROUTER_URL_ENV: &str = "RAYLINE_ROUTER_URL";
const DECISION_PLANE_ENV: &str = "RAYLINE_DECISION_PLANE";
const LOCAL_ROUTER_PORT_ENV: &str = "RAYLINE_LOCAL_ROUTER_PORT";
const LOCAL_ROUTER_CONFIG_PATH_ENV: &str = "RAYLINE_ROUTER_CONFIG";
const LOCAL_MODEL_ID_ENV: &str = "RAYLINE_LOCAL_MODEL_ID";
const NO_LOCAL_MODEL_ENV: &str = "RAYLINE_NO_LOCAL_MODEL";
const ADAPTER_UPSTREAM_MODEL_ENV: &str = "RAYLINE_ADAPTER_UPSTREAM_MODEL";
const ADAPTER_UPSTREAM_URL_ENV: &str = "RAYLINE_ADAPTER_UPSTREAM_URL";
const DATA_DIR_ENV: &str = "RAYLINE_DATA_DIR";
const PROXY_CA_CERT_PATH_ENV: &str = "RAYLINE_PROXY_CA_CERT_PATH";
const PROXY_CA_KEY_PATH_ENV: &str = "RAYLINE_PROXY_CA_KEY_PATH";
const UPSTREAM_CA_FILE_ENV: &str = "RAYLINE_UPSTREAM_CA_FILE";
const ROUTE_STATUS_PATH_ENV: &str = "RAYLINE_ROUTE_STATUS_PATH";
const ANTHROPIC_URL_ENV: &str = "RAYLINE_ANTHROPIC_URL";
const PROXY_ROUTING_MODE_ENV: &str = "RAYLINE_PROXY_ROUTING_MODE";
const METRICS_PORT_ENV: &str = "RAYLINE_METRICS_PORT";
const METRICS_URL_ENV: &str = "RAYLINE_METRICS_URL";
const SUBSCRIPTION_CONFIG_ENV: &str = "RAYLINE_SUBSCRIPTION_CONFIG";
const SUBSCRIPTION_POOL_ENV: &str = "RAYLINE_SUBSCRIPTION_POOL";

/// Per-line marker for structured progress events emitted to stderr. The
/// The launcher parses these to render a live progress bar while
/// it waits for healthz. The plain `RAYLINE_PROGRESS ` prefix keeps the line
/// human-readable in the log file too.
const PROGRESS_MARKER: &str = "RAYLINE_PROGRESS";

/// llama-server health-wait budget after spawn. Sized for slow disks /
/// large models (35B Q4 mmap is ~7-20s on NVMe, can be minutes on HDD).
/// Independent of the launcher-side router deadline; kept large enough that
/// the launcher's deadline is always the binding constraint.
const LLAMA_HEALTH_TIMEOUT: Duration = Duration::from_secs(1800);
const DAEMON_VERSION: &str = env!("RAYLINE_DAEMON_VERSION");
#[allow(dead_code)]
const DAEMON_CHANNEL: &str = env!("RAYLINE_DAEMON_CHANNEL");

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
    /// Run llama-server + adapter + injector under one supervisor (foreground).
    Serve(ServeArgs),
    /// Run only the transparent Claude Code HTTPS proxy (foreground).
    Proxy(ProxyArgs),
    /// Render the router's per-turn picked model for a Claude Code status line.
    Statusline(StatuslineArgs),
    /// Inspect cached GGUFs.
    Models {
        #[command(subcommand)]
        cmd: ModelsCmd,
    },
}

#[derive(clap::Args, Debug, Clone)]
struct StatuslineArgs {
    /// Path to the router-decision sidecar written by the proxy. Defaults to
    /// Defaults to the brand-specific router status sidecar path.
    #[arg(long, env = ROUTE_STATUS_PATH_ENV)]
    route_status_path: Option<PathBuf>,
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// List cached GGUFs.
    List,
}

#[derive(clap::Args, Debug, Clone)]
struct ServeArgs {
    /// HuggingFace repo holding the GGUF (e.g. `unsloth/Qwen3.5-35B-A3B-GGUF`).
    /// Required for the bundled-llama path; unused (and optional) when
    /// `--upstream-url` selects a custom endpoint.
    #[arg(long, env = MODEL_REPO_ENV)]
    model_repo: Option<String>,

    /// GGUF filename inside the repo (e.g. `Qwen3.5-35B-A3B-Q4_K_M.gguf`).
    /// Required for the bundled-llama path; unused (and optional) when
    /// `--upstream-url` selects a custom endpoint.
    #[arg(long, env = MODEL_FILE_ENV)]
    model_file: Option<String>,

    /// HuggingFace revision/commit. Resolved from `main` if unset.
    #[arg(long, env = MODEL_REVISION_ENV)]
    model_revision: Option<String>,

    /// Expected SHA256 for the GGUF. Requires `--model-revision` when set.
    #[arg(long, env = MODEL_SHA256_ENV)]
    model_sha256: Option<String>,

    /// llama.cpp release tag (download source).
    #[arg(long, env = LLAMA_TAG_ENV, default_value = DEFAULT_LLAMA_TAG)]
    llama_tag: String,

    /// Context size in tokens.
    #[arg(long, env = CTX_SIZE_ENV, default_value_t = DEFAULT_CTX_SIZE)]
    ctx_size: u32,

    /// Adapter listen port.
    #[arg(long, env = ADAPTER_PORT_ENV, default_value_t = rayline_adapter::DEFAULT_PORT)]
    adapter_port: u16,

    /// Injector listen port.
    #[arg(long, env = INJECTOR_PORT_ENV, default_value_t = rayline_injector::DEFAULT_PORT)]
    injector_port: u16,

    /// Optional transparent proxy listen port. In hosted mode this also
    /// requires the router API key environment variable.
    #[arg(long, env = PROXY_PORT_ENV)]
    proxy_port: Option<u16>,

    /// Router base URL the injector forwards to. In local mode this is derived
    /// from --local-router-port unless explicitly set.
    #[arg(long, env = ROUTER_URL_ENV)]
    router_url: Option<String>,

    /// Decision plane to use. Rayline defaults to the static local router.
    #[arg(long, env = DECISION_PLANE_ENV, default_value = "local")]
    decision_plane: DecisionPlaneArg,

    /// Local static router listen port when --decision-plane=local.
    #[arg(long, env = LOCAL_ROUTER_PORT_ENV, default_value_t = rayline_local_router::DEFAULT_PORT)]
    local_router_port: u16,

    /// Static local router JSON config path.
    #[arg(long, env = LOCAL_ROUTER_CONFIG_PATH_ENV)]
    router_config_path: Option<PathBuf>,

    /// Local model id to advertise to the classifier.
    #[arg(long, env = LOCAL_MODEL_ID_ENV, default_value = DEFAULT_LOCAL_MODEL_ID)]
    local_model_id: String,

    /// Override the model id the adapter sends to the local OpenAI-compatible
    /// server. Defaults to `model_repo`.
    #[arg(long, env = ADAPTER_UPSTREAM_MODEL_ENV)]
    upstream_model: Option<String>,

    /// Custom upstream endpoint root (LM Studio / Ollama / llama.cpp). When set,
    /// the daemon skips the bundled llama-server download/spawn and points the
    /// adapter at this URL (the adapter appends `/v1/messages`); the injector
    /// advertises custom mode so the router only delegates exploration subagents.
    #[arg(long, env = ADAPTER_UPSTREAM_URL_ENV)]
    upstream_url: Option<String>,

    /// Codex may-local: the local router advertises `x-rayline-local-*` on its
    /// native OpenAI Responses forward to the hosted RCR and follows the RCR's
    /// 307 to the on-device adapter (the codex, proxy-less analogue of the
    /// proxy's may-local advertisement).
    #[arg(long, hide = true)]
    may_local_advertise: bool,

    /// Codex may-local: the hosted RCR URL the adapter posts its
    /// `/v1/usage/update` callback to. The RCR issues the `usage_doc_id` in its
    /// 307, so its placeholder row can only be closed against the RCR — not the
    /// local router (loopback decision plane, stub OK handler). When unset the
    /// adapter uses the decision-plane `router_url`.
    #[arg(long, hide = true)]
    usage_callback_url: Option<String>,

    /// Data directory for llama-server binary + logs.
    #[arg(long, env = DATA_DIR_ENV)]
    data_dir: Option<PathBuf>,

    /// CA certificate path to write/reuse when --proxy-port is set.
    #[arg(long, env = PROXY_CA_CERT_PATH_ENV)]
    ca_cert_path: Option<PathBuf>,

    /// CA private key path to write/reuse when --proxy-port is set.
    #[arg(long, env = PROXY_CA_KEY_PATH_ENV)]
    ca_key_path: Option<PathBuf>,

    /// PEM file of extra root CAs to trust on upstream TLS connections,
    /// layered on top of the OS trust store. Use this when running behind a
    /// corporate MITM gateway whose CA is not in the OS keychain.
    #[arg(long, env = UPSTREAM_CA_FILE_ENV)]
    upstream_ca_path: Option<PathBuf>,

    /// Path to write the router's per-turn decision sidecar (JSON), consumed
    /// by the launcher-installed Claude Code status line.
    #[arg(long, env = ROUTE_STATUS_PATH_ENV)]
    route_status_path: Option<PathBuf>,

    /// Routing policy for the transparent Claude Code proxy.
    #[arg(long, env = PROXY_ROUTING_MODE_ENV, default_value = "selective-subagents")]
    proxy_routing_mode: ProxyRoutingModeArg,

    /// Local metrics-control listen port for `rayline router top`.
    #[arg(long, env = METRICS_PORT_ENV, default_value_t = rayline_metrics::DEFAULT_METRICS_PORT, hide = true)]
    metrics_port: u16,

    /// Run config-only: do not download/spawn a bundled llama-server and do not
    /// require `--upstream-url`/`--model-repo`. Used when the static router config
    /// routes only to named endpoints (no `"local"` route). The local router still
    /// serves; local availability is advertised as unavailable.
    #[arg(long, env = NO_LOCAL_MODEL_ENV)]
    no_local_model: bool,

    /// Multi-subscription registry used by Anthropic passthrough routes.
    #[arg(long, env = SUBSCRIPTION_CONFIG_ENV)]
    subscription_config: Option<PathBuf>,

    /// Pool name in --subscription-config (defaults to "default").
    #[arg(long, env = SUBSCRIPTION_POOL_ENV)]
    subscription_pool: Option<String>,
}

#[derive(clap::Args, Debug, Clone)]
struct ProxyArgs {
    /// Transparent proxy listen port.
    #[arg(long, env = PROXY_PORT_ENV, default_value_t = rayline_proxy::DEFAULT_PORT)]
    proxy_port: u16,

    /// Router base URL.
    #[arg(long, env = ROUTER_URL_ENV)]
    router_url: Option<String>,

    /// Anthropic API base URL. Primarily useful for synthetic tests.
    #[arg(long, env = ANTHROPIC_URL_ENV, default_value = rayline_proxy::DEFAULT_ANTHROPIC_URL, hide = true)]
    anthropic_url: String,

    /// CA certificate path to write/reuse.
    #[arg(long, env = PROXY_CA_CERT_PATH_ENV)]
    ca_cert_path: Option<PathBuf>,

    /// CA private key path to write/reuse.
    #[arg(long, env = PROXY_CA_KEY_PATH_ENV)]
    ca_key_path: Option<PathBuf>,

    /// PEM file of extra root CAs to trust on upstream TLS connections,
    /// layered on top of the OS trust store. Use this when running behind a
    /// corporate MITM gateway (Netskope, Zscaler, Palo Alto) whose CA is not
    /// in the OS keychain. The bundle may contain one or more CERTIFICATE
    /// blocks.
    #[arg(long, env = UPSTREAM_CA_FILE_ENV)]
    upstream_ca_path: Option<PathBuf>,

    /// Path to write the router's per-turn decision sidecar (JSON), consumed
    /// by the launcher-installed Claude Code status line.
    #[arg(long, env = ROUTE_STATUS_PATH_ENV)]
    route_status_path: Option<PathBuf>,

    /// Routing policy for the transparent Claude Code proxy.
    #[arg(long, env = PROXY_ROUTING_MODE_ENV, default_value = "selective-subagents")]
    proxy_routing_mode: ProxyRoutingModeArg,

    /// Static local router JSON config path. Hidden for proxy-only mode; the
    /// launcher passes it so selective-subagents can exactly match named agents.
    #[arg(long, env = LOCAL_ROUTER_CONFIG_PATH_ENV, hide = true)]
    router_config_path: Option<PathBuf>,

    /// Advertise local routing metadata to the decision plane.
    #[arg(long, hide = true)]
    local_available: bool,

    /// Local model id to advertise to the decision plane.
    #[arg(long, env = LOCAL_MODEL_ID_ENV, hide = true)]
    local_model_id: Option<String>,

    /// Local adapter port used to rewrite local redirects.
    #[arg(long, env = ADAPTER_PORT_ENV, hide = true)]
    local_adapter_port: Option<u16>,

    /// Mark the local endpoint as a user-managed custom endpoint.
    #[arg(long, hide = true)]
    local_custom: bool,

    /// Main daemon metrics-control URL for isolated proxy reporting.
    #[arg(long, env = METRICS_URL_ENV, hide = true)]
    metrics_url: Option<String>,

    /// Local router records routed request metrics; proxy records passthrough only.
    #[arg(long, hide = true)]
    local_router_owns_metrics: bool,

    /// Metrics-control listen port for `rayline router top` when the proxy
    /// self-hosts metrics (i.e. when --metrics-url is not set).
    #[arg(long, env = METRICS_PORT_ENV, default_value_t = rayline_metrics::DEFAULT_METRICS_PORT, hide = true)]
    metrics_port: u16,

    /// Multi-subscription registry used by Anthropic passthrough routes.
    #[arg(long, env = SUBSCRIPTION_CONFIG_ENV)]
    subscription_config: Option<PathBuf>,

    /// Pool name in --subscription-config (defaults to "default").
    #[arg(long, env = SUBSCRIPTION_POOL_ENV)]
    subscription_pool: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ProxyRoutingModeArg {
    All,
    SelectiveSubagents,
}

impl From<ProxyRoutingModeArg> for rayline_proxy::ProxyRoutingMode {
    fn from(value: ProxyRoutingModeArg) -> Self {
        match value {
            ProxyRoutingModeArg::All => Self::All,
            ProxyRoutingModeArg::SelectiveSubagents => Self::SelectiveSubagents,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum DecisionPlaneArg {
    Hosted,
    Local,
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
        Cmd::Serve(args) => run_serve(args).await,
        Cmd::Proxy(args) => run_proxy(args).await,
        Cmd::Statusline(args) => {
            statusline::run(resolve_route_status_path(args.route_status_path));
            Ok(())
        }
        Cmd::Models {
            cmd: ModelsCmd::List,
        } => run_models_list(),
    }
}

fn run_models_list() -> Result<()> {
    for entry in rayline_hf::scan_hf_cache_gguf() {
        println!(
            "{}\t{}\t{}",
            entry.repo,
            entry.filename,
            entry.path.display()
        );
    }
    Ok(())
}

fn llama_perf_log_observer(metrics: SharedMetricsSink) -> rayline_llama::LogObserver {
    Arc::new(move |line| {
        if let Some(update) = llama_perf_update_from_log_line(line) {
            metrics.record(update);
        }
    })
}

fn llama_perf_update_from_log_line(line: &str) -> Option<MetricsUpdate> {
    if line.contains("prompt processing") || line.contains("prompt eval time") {
        return tokens_per_second_before(line, "tokens per second").map(|rate| {
            MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
                prefill_tokens_per_second: Some(rate),
                generation_tokens_per_second: None,
                updated_at_unix_ms: now_unix_ms(),
            })
        });
    }
    if line.contains("eval time") {
        return tokens_per_second_before(line, "tokens per second").map(|rate| {
            MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
                prefill_tokens_per_second: None,
                generation_tokens_per_second: Some(rate),
                updated_at_unix_ms: now_unix_ms(),
            })
        });
    }
    if line.contains("n_decoded") && line.contains("tg =") {
        return tokens_per_second_before(line, "t/s").map(|rate| {
            MetricsUpdate::LlamaPerf(LlamaPerfSnapshot {
                prefill_tokens_per_second: None,
                generation_tokens_per_second: Some(rate),
                updated_at_unix_ms: now_unix_ms(),
            })
        });
    }
    None
}

fn tokens_per_second_before(line: &str, marker: &str) -> Option<f64> {
    let before_marker = line.split(marker).next()?;
    before_marker
        .rsplit(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .find(|part| !part.is_empty())
        .and_then(|part| part.parse::<f64>().ok())
}

/// The ports `serve` binds for routing; metrics must not take one of them.
fn serve_data_ports(args: &ServeArgs) -> Vec<u16> {
    let mut ports = vec![
        args.adapter_port,
        args.injector_port,
        args.local_router_port,
    ];
    ports.extend(args.proxy_port);
    ports
}

async fn run_serve(args: ServeArgs) -> Result<()> {
    let metrics = RouterMetrics::new("rayline-router");
    let metrics_sink: SharedMetricsSink = metrics.clone();
    // Best-effort, as in proxy mode: a metrics bind failure (another rld may
    // hold the port) must not stop the router from serving (#82). Metrics never
    // takes one of serve's own data ports, or the router's bind would fail.
    let data_ports = serve_data_ports(&args);
    if data_ports.contains(&args.metrics_port) {
        warn!(
            "router metrics disabled: port {} is one of this router's data ports",
            args.metrics_port
        );
    } else {
        match bind_metrics_control(args.metrics_port).await {
            Ok(listener) => spawn_metrics_control(metrics, listener),
            Err(error) => warn!("router metrics disabled: {error:#}"),
        }
    }

    let data_dir = args
        .data_dir
        .clone()
        .or_else(|| {
            dirs::home_dir().map(|home| {
                home.join(RAYLINE_DOT_CONFIG_DIR)
                    .join(RAYLINE_DAEMON_BIN_NAME)
            })
        })
        .ok_or_else(|| anyhow!("Could not resolve data dir"))?;
    std::fs::create_dir_all(&data_dir).context("create data dir")?;
    let hosted_router_url = args
        .router_url
        .as_deref()
        .unwrap_or(RAYLINE_ROUTER_DEFAULT_URL);
    let router_url = match args.decision_plane {
        DecisionPlaneArg::Hosted => hosted_router_url.to_owned(),
        DecisionPlaneArg::Local => format!("http://127.0.0.1:{}", args.local_router_port),
    };

    // Custom upstream mode: skip the bundled llama-server entirely (no GGUF /
    // binary download, no spawn, no health watchdog). The user owns their
    // server's uptime; `rl local test` is the documented pre-flight. We always
    // advertise local-available (`local_available: None`) and flip the injector
    // into custom mode. Otherwise: the bundled-llama path (1-3 + watchdog).
    let (adapter_target, local_available, custom_mode, manager) = if let Some(upstream_url) =
        args.upstream_url.as_deref()
    {
        // A custom upstream serves a specific model, and the adapter rewrites the
        // request body's `model` to it — so without an explicit model we'd
        // silently substitute the bundled id (`local_model_id`) and the user's
        // server would reject every turn. Require it (fail fast) instead.
        let upstream_model = args.upstream_model.as_deref().filter(|m| !m.is_empty());
        if upstream_model.is_none() {
            return Err(anyhow!(
                "--upstream-model is required with --upstream-url (the request model is rewritten \
                 to it). Pass --upstream-model <NAME>."
            ));
        }
        let target = normalize_upstream_target(upstream_url);
        info!(
            "custom upstream mode — adapter target {} model {}",
            target,
            upstream_model.unwrap_or("?")
        );
        (target, None, true, None)
    } else if args.no_local_model {
        // Config-only mode: no bundled model. The static router forwards to its
        // named endpoints directly (the adapter / `"local"` route is unused), so
        // advertise local-unavailable and point the adapter at a dead target.
        info!("config-only mode — no bundled local model; routing via configured endpoints");
        (
            "http://127.0.0.1:1".to_owned(),
            Some(Arc::new(AtomicBool::new(false))),
            false,
            None,
        )
    } else {
        // 1. Resolve / download GGUF. The bundled-llama path needs the GGUF
        // coordinates; they are optional on the CLI only so custom mode can
        // omit them.
        let model_repo = args
            .model_repo
            .as_deref()
            .ok_or_else(|| anyhow!("--model-repo is required without --upstream-url"))?;
        let model_file = args
            .model_file
            .as_deref()
            .ok_or_else(|| anyhow!("--model-file is required without --upstream-url"))?;
        let gguf_path = resolve_or_download_gguf(
            model_repo,
            model_file,
            args.model_revision.as_deref(),
            args.model_sha256.as_deref(),
        )
        .await?;
        info!("model GGUF ready at {}", gguf_path.display());

        // 2. Ensure llama-server binary is present.
        let manager = rayline_llama::LlamaServerManager::new(data_dir.clone());
        ensure_llama_binary(&manager, &args.llama_tag).await?;

        // 3. Spawn llama-server, wait for health.
        let llama_port = {
            let manager = manager.clone();
            let gguf_path = gguf_path.clone();
            let ctx = args.ctx_size;
            let log_observer = llama_perf_log_observer(metrics_sink.clone());
            tokio::task::spawn_blocking(move || {
                rayline_llama::start_server_with_log_observer(
                    &manager,
                    &gguf_path.to_string_lossy(),
                    ctx,
                    &rayline_llama::ServerOptions::default(),
                    Some(log_observer),
                )
                .map_err(|e| anyhow!(e))
            })
            .await??
        };
        info!("llama-server spawned on port {}", llama_port);

        if let Err(e) = install_shutdown_handler(manager.clone()) {
            let _ = rayline_llama::stop_server(&manager);
            return Err(e);
        }

        let healthy = {
            let port = llama_port;
            tokio::task::spawn_blocking(move || {
                rayline_llama::wait_for_health(port, LLAMA_HEALTH_TIMEOUT)
            })
            .await?
        };
        if !healthy {
            let _ = rayline_llama::stop_server(&manager);
            return Err(anyhow!(
                "llama-server failed to become healthy in {}s",
                LLAMA_HEALTH_TIMEOUT.as_secs()
            ));
        }
        info!("llama-server healthy on port {}", llama_port);

        // Health watchdog: poll the local model and flip a shared flag so the
        // injector/proxy advertise local-unavailable when it's down. The cloud
        // router then serves those turns instead of Claude Code retry-looping
        // against a dead adapter; it flips back when the model recovers.
        // `check_health` is blocking, so run it on a dedicated OS thread.
        let local_healthy = Arc::new(AtomicBool::new(true));
        {
            let flag = local_healthy.clone();
            let port = llama_port;
            thread::spawn(move || {
                let mut last = true;
                loop {
                    thread::sleep(Duration::from_secs(2));
                    let up = rayline_llama::check_health(port);
                    flag.store(up, Ordering::Relaxed);
                    if up != last {
                        if up {
                            info!(
                                "local model healthy again on port {port}; resuming local routing"
                            );
                        } else {
                            warn!("local model unhealthy on port {port}; routing turns to cloud");
                        }
                        last = up;
                    }
                }
            });
        }

        (
            format!("http://127.0.0.1:{llama_port}"),
            Some(local_healthy),
            false,
            Some(manager),
        )
    };

    // 4. Run adapter + injector, and optionally transparent proxy, concurrently.
    let upstream_model = args
        .upstream_model
        .clone()
        .or_else(|| args.model_repo.clone())
        .unwrap_or_else(|| args.local_model_id.clone());
    let auth_cache = rayline_proxy::new_auth_cache();
    // The adapter's /v1/usage/update callback must reach whoever owns the
    // usage_doc_id. On the local decision plane that is normally the local router
    // itself, but under Codex may-local the doc id comes from the HOSTED RCR's
    // 307 — so use the explicit callback URL when provided, else the decision
    // plane's router_url.
    let adapter_router_url = args
        .usage_callback_url
        .as_deref()
        .unwrap_or(&router_url)
        .trim_end_matches('/')
        .to_string();
    let adapter_opts = rayline_adapter::AdapterOptions {
        port: args.adapter_port,
        target: adapter_target,
        upstream_model,
        router_url: adapter_router_url,
        auth_cache: Some(auth_cache.clone()),
        metrics: Some(metrics_sink.clone()),
        collect_llama_progress: !custom_mode,
    };
    let injector_opts = rayline_injector::InjectorOptions {
        port: args.injector_port,
        router_url: router_url.trim_end_matches('/').to_string(),
        local_model_id: args.local_model_id.clone(),
        auth_cache: Some(auth_cache),
        local_available: local_available.clone(),
        custom_mode,
    };
    let proxy_opts = if let Some(proxy_port) = args.proxy_port {
        let router_api_key = if args.decision_plane == DecisionPlaneArg::Local {
            String::new()
        } else {
            router_api_key()?
        };
        if args.decision_plane == DecisionPlaneArg::Hosted && router_api_key.is_empty() {
            return Err(anyhow!(
                "{} is required when --proxy-port is set",
                router_api_key_env_var()
            ));
        }
        let (ca_cert_path, ca_key_path) =
            resolve_proxy_ca_paths(args.ca_cert_path.clone(), args.ca_key_path.clone())?;
        let mut opts =
            rayline_proxy::ProxyOptions::with_ca_paths(router_api_key, ca_cert_path, ca_key_path);
        opts.port = proxy_port;
        opts.router_url = router_url.trim_end_matches('/').to_string();
        opts.local_available = true;
        opts.local_model_id = Some(args.local_model_id.clone());
        opts.local_adapter_port = Some(args.adapter_port);
        opts.custom_mode = custom_mode;
        opts.auth_cache = Some(adapter_opts.auth_cache.as_ref().unwrap().clone());
        opts.upstream_ca_path = args.upstream_ca_path.clone();
        opts.local_healthy = local_available.clone();
        opts.route_status_path = Some(resolve_route_status_path(args.route_status_path.clone()));
        opts.routing_mode = args.proxy_routing_mode.into();
        opts.selective_subagent_ids = selective_subagent_ids(args.router_config_path.as_deref());
        opts.local_router_owns_metrics = args.decision_plane == DecisionPlaneArg::Local;
        opts.metrics = Some(metrics_sink.clone());
        if let Some(subscription) = load_subscription_pool(
            args.subscription_config.as_deref(),
            args.subscription_pool.as_deref(),
            rayline_proxy::DEFAULT_ANTHROPIC_URL,
        )
        .await?
        {
            opts.subscription_pool = Some(subscription.runtime);
            opts.claude_config_dir = Some(subscription.control_config_dir);
        }
        Some(opts)
    } else {
        None
    };
    let local_router_opts = if args.decision_plane == DecisionPlaneArg::Local {
        Some(rayline_local_router::LocalRouterOptions {
            port: args.local_router_port,
            local_adapter_port: args.adapter_port,
            local_model_id: args.local_model_id.clone(),
            config_path: args.router_config_path.clone(),
            metrics: Some(metrics_sink.clone()),
            may_local_advertise: args.may_local_advertise,
        })
    } else {
        None
    };

    // Stop the bundled llama-server on exit; a no-op in custom mode (no manager).
    let stop_llama = |manager: &Option<rayline_llama::LlamaServerManager>| {
        if let Some(m) = manager {
            let _ = rayline_llama::stop_server(m);
        }
    };

    if let Some(proxy_opts) = proxy_opts {
        info!(
            "{} proxy ready target — HTTPS_PROXY=http://127.0.0.1:{} NODE_EXTRA_CA_CERTS={}",
            RAYLINE_DAEMON_BIN_NAME,
            proxy_opts.port,
            proxy_opts.ca_cert_path.display()
        );
        if let Some(local_router_opts) = local_router_opts {
            tokio::select! {
                r = rayline_local_router::serve(local_router_opts) => {
                    stop_llama(&manager);
                    r.context("local router exited")
                }
                r = rayline_adapter::serve(adapter_opts) => {
                    stop_llama(&manager);
                    r.context("adapter exited")
                }
                r = rayline_injector::serve(injector_opts) => {
                    stop_llama(&manager);
                    r.context("injector exited")
                }
                r = rayline_proxy::serve(proxy_opts) => {
                    stop_llama(&manager);
                    r.context("proxy exited")
                }
            }
        } else {
            tokio::select! {
                r = rayline_adapter::serve(adapter_opts) => {
                    stop_llama(&manager);
                    r.context("adapter exited")
                }
                r = rayline_injector::serve(injector_opts) => {
                    stop_llama(&manager);
                    r.context("injector exited")
                }
                r = rayline_proxy::serve(proxy_opts) => {
                    stop_llama(&manager);
                    r.context("proxy exited")
                }
            }
        }
    } else {
        info!(
            "{} ready — point Claude Code at: ANTHROPIC_BASE_URL=http://127.0.0.1:{}",
            RAYLINE_DAEMON_BIN_NAME, args.injector_port
        );
        if let Some(local_router_opts) = local_router_opts {
            tokio::select! {
                r = rayline_local_router::serve(local_router_opts) => {
                    stop_llama(&manager);
                    r.context("local router exited")
                }
                r = rayline_adapter::serve(adapter_opts) => {
                    stop_llama(&manager);
                    r.context("adapter exited")
                }
                r = rayline_injector::serve(injector_opts) => {
                    stop_llama(&manager);
                    r.context("injector exited")
                }
            }
        } else {
            tokio::select! {
                r = rayline_adapter::serve(adapter_opts) => {
                    stop_llama(&manager);
                    r.context("adapter exited")
                }
                r = rayline_injector::serve(injector_opts) => {
                    stop_llama(&manager);
                    r.context("injector exited")
                }
            }
        }
    }
}

async fn run_proxy(args: ProxyArgs) -> Result<()> {
    let router_url = args
        .router_url
        .as_deref()
        .unwrap_or(RAYLINE_ROUTER_DEFAULT_URL);
    let router_api_key = router_api_key()?;
    if router_api_key.is_empty() && !args.local_available {
        return Err(anyhow!(
            "{} is required for `{} proxy`",
            router_api_key_env_var(),
            RAYLINE_DAEMON_BIN_NAME
        ));
    }
    if args.local_available && args.local_model_id.is_none() {
        return Err(anyhow!(
            "--local-model-id is required when --local-available is set"
        ));
    }
    if args.local_available && args.local_adapter_port.is_none() {
        return Err(anyhow!(
            "--local-adapter-port is required when --local-available is set"
        ));
    }

    let (ca_cert_path, ca_key_path) =
        resolve_proxy_ca_paths(args.ca_cert_path.clone(), args.ca_key_path.clone())?;
    let mut opts =
        rayline_proxy::ProxyOptions::with_ca_paths(router_api_key, ca_cert_path, ca_key_path);
    opts.port = args.proxy_port;
    opts.router_url = router_url.trim_end_matches('/').to_string();
    opts.anthropic_url = args.anthropic_url.trim_end_matches('/').to_string();
    opts.local_available = args.local_available;
    opts.local_model_id = args.local_model_id.clone();
    opts.local_adapter_port = args.local_adapter_port;
    opts.custom_mode = args.local_available && args.local_custom;
    opts.upstream_ca_path = args.upstream_ca_path.clone();
    opts.route_status_path = Some(resolve_route_status_path(args.route_status_path.clone()));
    opts.routing_mode = args.proxy_routing_mode.into();
    opts.selective_subagent_ids = selective_subagent_ids(args.router_config_path.as_deref());
    opts.local_router_owns_metrics = args.local_router_owns_metrics;
    if let Some(subscription) = load_subscription_pool(
        args.subscription_config.as_deref(),
        args.subscription_pool.as_deref(),
        &opts.anthropic_url,
    )
    .await?
    {
        opts.subscription_pool = Some(subscription.runtime);
        opts.claude_config_dir = Some(subscription.control_config_dir);
    }
    // Forward to a serve daemon when one owns metrics; otherwise self-host so
    // `rayline top` works for cloud-only and isolated proxy sessions too.
    opts.metrics = match proxy_metrics_plan(args.metrics_url.as_deref(), args.metrics_port) {
        ProxyMetricsPlan::Forward(url) => {
            Some(Arc::new(HttpMetricsSink::new(&url)) as SharedMetricsSink)
        }
        ProxyMetricsPlan::SelfHost(metrics_port) => {
            let metrics = RouterMetrics::new("rayline-proxy");
            let sink: SharedMetricsSink = metrics.clone();
            // Best-effort: a metrics bind failure must not take down the proxy
            // data path. Degrade to no metrics for this session instead.
            match bind_metrics_control(metrics_port).await {
                Ok(listener) => {
                    spawn_metrics_control(metrics, listener);
                    Some(sink)
                }
                Err(error) => {
                    warn!(
                        "proxy metrics disabled: could not bind metrics control on \
                         127.0.0.1:{metrics_port}: {error}"
                    );
                    None
                }
            }
        }
    };

    info!(
        "{} proxy ready target — HTTPS_PROXY=http://127.0.0.1:{} NODE_EXTRA_CA_CERTS={}",
        RAYLINE_DAEMON_BIN_NAME,
        opts.port,
        opts.ca_cert_path.display()
    );
    rayline_proxy::serve(opts).await
}

async fn load_subscription_pool(
    config_path: Option<&Path>,
    pool_id: Option<&str>,
    anthropic_base_url: &str,
) -> Result<Option<LoadedSubscriptionPool>> {
    let Some(config_path) = config_path else {
        if pool_id.is_some() {
            return Err(anyhow!(
                "--subscription-pool requires --subscription-config"
            ));
        }
        return Ok(None);
    };
    let metadata = std::fs::symlink_metadata(config_path)
        .with_context(|| format!("inspect subscription pool config {}", config_path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(anyhow!(
            "subscription pool config {} must be a regular file, not a symlink",
            config_path.display()
        ));
    }
    if metadata.len() > 1024 * 1024 {
        return Err(anyhow!(
            "subscription pool config {} exceeds 1 MiB",
            config_path.display()
        ));
    }
    let bytes = std::fs::read(config_path).with_context(|| {
        format!(
            "read subscription pool config from {}",
            config_path.display()
        )
    })?;
    let config: rayline_subscriptions::SubscriptionPoolsConfig = serde_json::from_slice(&bytes)
        .with_context(|| {
            format!(
                "parse subscription pool config from {}",
                config_path.display()
            )
        })?;
    config
        .validate()
        .context("validate subscription pool config")?;
    let pool_id = pool_id.unwrap_or("default");
    let pool = config
        .pools
        .get(pool_id)
        .cloned()
        .ok_or_else(|| anyhow!("subscription pool {pool_id:?} does not exist"))?;
    let control_config_dir = expand_home_path(&pool.control_config_dir)?
        .canonicalize()
        .with_context(|| {
            format!(
                "resolve subscription control config directory {}",
                pool.control_config_dir.display()
            )
        })?;
    if !control_config_dir.is_dir() {
        return Err(anyhow!(
            "subscription control config directory {} is not a directory",
            control_config_dir.display()
        ));
    }
    let options = rayline_subscriptions::SubscriptionRuntimeOptions {
        anthropic_base_url: anthropic_base_url.to_owned(),
        ..Default::default()
    };
    let runtime =
        rayline_subscriptions::SubscriptionPoolRuntime::start(pool_id, pool, options).await?;
    let _ = runtime.spawn_monitor();
    info!(
        "subscription pool={} ready accounts={}",
        runtime.pool_id(),
        runtime.account_count()
    );
    Ok(Some(LoadedSubscriptionPool {
        runtime,
        control_config_dir,
    }))
}

struct LoadedSubscriptionPool {
    runtime: Arc<rayline_subscriptions::SubscriptionPoolRuntime>,
    control_config_dir: PathBuf,
}

fn expand_home_path(path: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().ok_or_else(|| anyhow!("home directory not found"));
    }
    if let Some(suffix) = raw.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(suffix))
            .ok_or_else(|| anyhow!("home directory not found"));
    }
    Ok(path.to_owned())
}

/// How a proxy-mode launch wires up metrics. A serve daemon, when present, owns
/// the metrics server and the proxy forwards to it; otherwise the proxy stands
/// up its own server so monitoring works for any proxy session.
#[derive(Debug, Eq, PartialEq)]
enum ProxyMetricsPlan {
    Forward(String),
    SelfHost(u16),
}

fn proxy_metrics_plan(metrics_url: Option<&str>, metrics_port: u16) -> ProxyMetricsPlan {
    match metrics_url {
        Some(url) => ProxyMetricsPlan::Forward(url.to_owned()),
        None => ProxyMetricsPlan::SelfHost(metrics_port),
    }
}

struct HttpMetricsSink {
    updates: mpsc::Sender<MetricsUpdate>,
}

impl HttpMetricsSink {
    fn new(base_url: &str) -> Self {
        let client = reqwest::Client::new();
        let url = format!("{}/v1/router/top/update", base_url.trim_end_matches('/'));
        let (updates, mut rx) = mpsc::channel::<MetricsUpdate>(1024);
        tokio::spawn(async move {
            while let Some(update) = rx.recv().await {
                if let Err(error) = client.post(&url).json(&update).send().await {
                    warn!("failed to forward metrics update: {error}");
                }
            }
        });
        Self { updates }
    }
}

impl MetricsSink for HttpMetricsSink {
    fn record(&self, update: MetricsUpdate) {
        match self.updates.try_send(update) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("dropping metrics update because forwarding queue is full");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

async fn bind_metrics_control(port: u16) -> Result<TcpListener> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind metrics control on 127.0.0.1:{port}"))?;
    info!("metrics control listening on 127.0.0.1:{port}");
    Ok(listener)
}

fn spawn_metrics_control(metrics: Arc<RouterMetrics>, listener: TcpListener) {
    tokio::spawn(async move {
        if let Err(error) = serve_metrics_control(metrics, listener).await {
            warn!("metrics control server exited: {error}");
        }
    });
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response<BoxBody> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(body))
        .unwrap()
}

async fn serve_metrics_control(metrics: Arc<RouterMetrics>, listener: TcpListener) -> Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let metrics = metrics.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let metrics = metrics.clone();
                async move { Ok::<_, Infallible>(handle_metrics_control(metrics, req).await) }
            });
            if let Err(error) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                warn!("metrics control connection error: {error}");
            }
        });
    }
}

async fn handle_metrics_control(
    metrics: Arc<RouterMetrics>,
    req: Request<Incoming>,
) -> Response<BoxBody> {
    match (req.method().clone(), req.uri().path()) {
        (Method::GET, "/healthz") => json_response(
            StatusCode::OK,
            // The pid says which process owns this port, so a launcher can tell
            // its own serve from another process that bound it first (#82).
            serde_json::json!({
                "ok": true,
                "runtime": "rayline-router-metrics",
                "pid": std::process::id(),
            }),
        ),
        (Method::GET, "/v1/router/top/snapshot") => {
            json_response(StatusCode::OK, serde_json::json!(metrics.snapshot()))
        }
        (Method::POST, "/v1/router/top/update") => {
            let body = match req.into_body().collect().await {
                Ok(body) => body.to_bytes(),
                Err(error) => {
                    return json_response(
                        StatusCode::BAD_REQUEST,
                        serde_json::json!({"ok": false, "error": error.to_string()}),
                    );
                }
            };
            match serde_json::from_slice::<MetricsUpdate>(&body) {
                Ok(update) => {
                    metrics.record(update);
                    json_response(StatusCode::OK, serde_json::json!({"ok": true}))
                }
                Err(error) => json_response(
                    StatusCode::BAD_REQUEST,
                    serde_json::json!({"ok": false, "error": error.to_string()}),
                ),
            }
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(full_body("not found"))
            .unwrap(),
    }
}

/// Normalize a custom upstream endpoint root for the adapter `target`. The
/// adapter appends `/v1/messages`, so a trailing `/` would yield a double
/// slash; trim it.
/// Normalize a custom upstream URL to the server root: strip a trailing `/` and
/// a trailing `/v1` (common for OpenAI-compatible servers). The adapter appends
/// `/v1/messages` itself, so `http://host/v1` must become `http://host` to avoid
/// `/v1/v1/messages`. Mirrors `rl local`'s `local_model::normalize_base_url`.
fn normalize_upstream_target(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    let stripped = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    stripped.trim_end_matches('/').to_string()
}

fn selective_subagent_ids(config_path: Option<&Path>) -> Vec<String> {
    let Some(path) = config_path else {
        return Vec::new();
    };
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return Vec::new();
    };
    let Some(subagents) = value
        .get("routes")
        .and_then(|routes| routes.get("subagents"))
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let mut ids = subagents
        .keys()
        .map(|key| key.trim())
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn resolve_route_status_path(explicit: Option<PathBuf>) -> PathBuf {
    explicit.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(RAYLINE_DOT_CONFIG_DIR)
            .join(RAYLINE_DAEMON_BIN_NAME)
            .join("route-status.json")
    })
}

fn router_api_key() -> Result<String> {
    std::env::var(router_api_key_env_var())
        .or_else(|_| std::env::var("RAYLINE_ROUTER_API_KEY"))
        .map_err(|_| {
            anyhow!(
                "{} is required for `{} proxy`",
                router_api_key_env_var(),
                RAYLINE_DAEMON_BIN_NAME
            )
        })
}

fn router_api_key_env_var() -> &'static str {
    "RAYLINE_ROUTER_API_KEY"
}

/// Resolve the proxy CA cert/key paths, defaulting to
/// `dirs::config_dir()/<brand>/{proxy-ca,proxy-ca-key}.pem`.
///
/// This is the single source of truth for the CA the proxy signs MITM leaves
/// with. The launcher mirrors it so the CA Claude
/// Code trusts via `NODE_EXTRA_CA_CERTS` matches the CA the proxy signs with.
/// If these two diverge, launcher proxy routing fails TLS with
/// CERT_SIGNATURE_FAILURE. Keep the two resolvers in lockstep — see PR #5939.
fn resolve_proxy_ca_paths(
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
) -> Result<(PathBuf, PathBuf)> {
    let config_dir = dirs::config_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
        .join(RAYLINE_CONFIG_DIR_NAME);
    Ok((
        cert_path.unwrap_or_else(|| config_dir.join(DEFAULT_PROXY_CA_CERT)),
        key_path.unwrap_or_else(|| config_dir.join(DEFAULT_PROXY_CA_KEY)),
    ))
}

fn install_shutdown_handler(manager: rayline_llama::LlamaServerManager) -> Result<()> {
    // Launcher stop commands send SIGTERM; ctrl-c at the TTY sends SIGINT.
    // Register before the llama health wait so startup cancellation cleans up.
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = signal(SignalKind::interrupt()).context("install SIGINT handler")?;
        let mut sigterm = signal(SignalKind::terminate()).context("install SIGTERM handler")?;
        tokio::spawn(async move {
            let name = tokio::select! {
                _ = sigint.recv() => "SIGINT",
                _ = sigterm.recv() => "SIGTERM",
            };
            info!("{name} received — stopping llama-server");
            let _ = rayline_llama::stop_server(&manager);
            std::process::exit(0);
        });
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async move {
            if let Err(e) = tokio::signal::ctrl_c().await {
                tracing::warn!("ctrl_c error: {e}");
                return;
            }
            info!("ctrl-c received — stopping llama-server");
            let _ = rayline_llama::stop_server(&manager);
            std::process::exit(0);
        });
    }
    Ok(())
}

async fn resolve_or_download_gguf(
    repo: &str,
    filename: &str,
    revision: Option<&str>,
    expected_sha256: Option<&str>,
) -> Result<PathBuf> {
    if expected_sha256.is_some() && revision.is_none() {
        return Err(anyhow!(
            "--model-sha256 requires --model-revision so the verified download is pinned"
        ));
    }

    // With a pinned revision, only treat the exact snapshot dir as a hit.
    // Without one, this is an explicit unverified/user-supplied path, so any
    // cached copy of repo+filename is acceptable.
    if let Some(rev) = revision {
        if let Some(snapshot) =
            rayline_hf::verified_hf_cache_file(repo, filename, rev, expected_sha256)
                .map_err(|e| anyhow!(e))
                .with_context(|| {
                    format!("cache verification failed for {repo}/{filename} @ {rev}")
                })?
        {
            info!("cache hit: {} / {} @ {}", repo, filename, rev);
            return Ok(snapshot);
        }
    } else {
        for entry in rayline_hf::scan_hf_cache_gguf() {
            if entry.repo == repo && entry.filename == filename {
                info!("cache hit: {} / {}", repo, filename);
                return Ok(entry.path);
            }
        }
    }
    info!("cache miss: downloading {} / {}", repo, filename);
    if expected_sha256.is_none() {
        if revision.is_some() {
            warn!(
                "downloading {} / {} without SHA256 verification; this should only be used for explicitly supplied models",
                repo, filename
            );
        } else {
            warn!(
                "resolving current Hugging Face commit and downloading {} / {} without SHA256 verification; this should only be used for explicitly supplied models",
                repo, filename
            );
        }
    }

    let commit = if let Some(r) = revision {
        r.to_string()
    } else {
        let repo = repo.to_string();
        tokio::task::spawn_blocking(move || rayline_hf::hf_api_get_commit(&repo))
            .await?
            .map_err(|e| anyhow!(e))?
    };
    let repo = repo.to_string();
    let filename = filename.to_string();
    let expected_sha256 = expected_sha256.map(ToOwned::to_owned);
    let path = tokio::task::spawn_blocking(move || {
        let cb = |p: DownloadProgress| emit_progress_event(&p);
        rayline_hf::download_to_hf_cache(
            &repo,
            &filename,
            &commit,
            expected_sha256.as_deref(),
            Some(&cb),
            "model",
            None,
            0,
            0,
            None,
        )
        .map_err(|e| anyhow!(e))
    })
    .await??;
    Ok(path)
}

/// Structured progress payload emitted to stderr so the launcher can
/// render a live progress bar. Keeping this in one place makes it easy to
/// add new event kinds later (e.g. llama-server extract, llama-server load)
/// without re-defining the wire format on each call site.
#[derive(Serialize)]
struct ProgressEvent<'a> {
    event: &'a str,
    stage: &'a str,
    filename: &'a str,
    bytes: u64,
    total: u64,
    percent: f64,
}

fn emit_progress_event(progress: &DownloadProgress) {
    let payload = ProgressEvent {
        event: "download_progress",
        stage: &progress.stage,
        filename: &progress.filename,
        bytes: progress.bytes_downloaded,
        total: progress.total_bytes,
        percent: progress.percent,
    };
    // stdout/stderr are redirected to a log file by the launcher, which tails
    // the file and parses these lines. Use the marker prefix so the wrapper can
    // distinguish structured events from free-form tracing output.
    if let Ok(json) = serde_json::to_string(&payload) {
        eprintln!("{PROGRESS_MARKER} {json}");
    }
}

async fn ensure_llama_binary(manager: &rayline_llama::LlamaServerManager, tag: &str) -> Result<()> {
    if manager.runtime_installed() && manager.runtime_version() == tag {
        info!(
            "llama-server binary present at {}",
            manager.runtime_binary_path().display()
        );
        return Ok(());
    }
    let hw = rayline_llama::detect_hardware();
    let url = rayline_llama::resolve_download_url(tag, &hw.os, &hw.arch, &hw.gpu_type)
        .map_err(|e| anyhow!(e))?;
    let archive_name = rayline_llama::resolve_archive_filename(tag, &hw.os, &hw.arch, &hw.gpu_type)
        .map_err(|e| anyhow!(e))?;
    let bin_dir = manager.bin_dir();
    std::fs::create_dir_all(&bin_dir).context("create bin dir")?;
    let archive_path = bin_dir.join(&archive_name);

    let archive_path_dl = archive_path.clone();
    let url_dl = url.clone();
    tokio::task::spawn_blocking(move || {
        rayline_llama::download_file(&url_dl, &archive_path_dl).map_err(|e| anyhow!(e))
    })
    .await??;

    let bin_dir_ext = bin_dir.clone();
    let archive_path_ext = archive_path.clone();
    let extracted = tokio::task::spawn_blocking(move || {
        rayline_llama::extract_runtime_binary(&archive_path_ext, &bin_dir_ext)
            .map_err(|e| anyhow!(e))
    })
    .await??;
    info!("extracted llama-server binary at {}", extracted.display());

    std::fs::write(manager.version_file_path(), tag).context("write version file")?;
    let variant = match hw.gpu_type.as_str() {
        "apple-silicon" => "metal",
        "nvidia" => "cuda",
        "amd" | "amd-apu" => "vulkan",
        _ => "cpu",
    };
    std::fs::write(manager.variant_file_path(), variant).context("write variant file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_self_hosts_metrics_when_no_forwarding_url() {
        let plan = proxy_metrics_plan(None, 20814);
        assert_eq!(plan, ProxyMetricsPlan::SelfHost(20814));
    }

    #[test]
    fn proxy_forwards_metrics_when_url_present() {
        let plan = proxy_metrics_plan(Some("http://127.0.0.1:20813"), 20814);
        assert_eq!(
            plan,
            ProxyMetricsPlan::Forward("http://127.0.0.1:20813".to_owned())
        );
    }

    /// End-to-end cut-point for the self-host path: a proxy that is not forwarding
    /// stands up its own metrics-control server (the exact `bind_metrics_control` +
    /// `spawn_metrics_control` mechanism `run_proxy`'s `SelfHost` arm uses) and that
    /// server answers `rayline top`'s snapshot probe with a well-formed payload.
    #[tokio::test]
    async fn self_hosted_proxy_metrics_serves_well_formed_snapshot() {
        let metrics = RouterMetrics::new("rayline-proxy");
        let listener = bind_metrics_control(0).await.expect("bind metrics control");
        let port = listener.local_addr().expect("listener addr").port();
        spawn_metrics_control(metrics, listener);

        let client = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/v1/router/top/snapshot");
        let response = client.get(&url).send().await.expect("snapshot request");
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let body: serde_json::Value = response.json().await.expect("json body");
        for key in ["ok", "totals", "active", "recent"] {
            assert!(body.get(key).is_some(), "snapshot missing `{key}`: {body}");
        }
    }

    #[test]
    fn llama_perf_parser_reads_prefill_progress() {
        let update = llama_perf_update_from_log_line(
            "11.00 I slot print_timing: prompt processing, n_tokens = 2050, t = 10.84 s / 189.04 tokens per second",
        )
        .expect("prefill update");
        let MetricsUpdate::LlamaPerf(snapshot) = update else {
            panic!("expected llama perf update");
        };
        assert_eq!(snapshot.prefill_tokens_per_second, Some(189.04));
        assert_eq!(snapshot.generation_tokens_per_second, None);
    }

    #[test]
    fn llama_perf_parser_reads_final_prefill_and_generation() {
        let prefill = llama_perf_update_from_log_line(
            "15.30 I slot print_timing: prompt eval time = 86975.52 ms / 12507 tokens (6.95 ms per token, 143.80 tokens per second)",
        )
        .expect("prefill update");
        let MetricsUpdate::LlamaPerf(prefill) = prefill else {
            panic!("expected llama perf update");
        };
        assert_eq!(prefill.prefill_tokens_per_second, Some(143.80));

        let generation = llama_perf_update_from_log_line(
            "15.30 I slot print_timing:        eval time = 193934.69 ms / 4100 tokens (47.30 ms per token, 21.14 tokens per second)",
        )
        .expect("generation update");
        let MetricsUpdate::LlamaPerf(generation) = generation else {
            panic!("expected llama perf update");
        };
        assert_eq!(generation.generation_tokens_per_second, Some(21.14));
    }

    #[test]
    fn llama_perf_parser_reads_live_generation_speed() {
        let update = llama_perf_update_from_log_line(
            "12.20 I slot print_timing: id 0 | task 2617 | n_decoded = 100, tg = 23.32 t/s",
        )
        .expect("generation update");
        let MetricsUpdate::LlamaPerf(snapshot) = update else {
            panic!("expected llama perf update");
        };
        assert_eq!(snapshot.prefill_tokens_per_second, None);
        assert_eq!(snapshot.generation_tokens_per_second, Some(23.32));
    }

    /// The proxy CA defaults MUST resolve to the Rayline config dir. The launcher
    /// proxy CA defaults mirror this so the CA Claude Code trusts matches the CA
    /// the proxy signs with; drift here resurfaces
    /// the macOS CERT_SIGNATURE_FAILURE fixed in PR #5939.
    #[test]
    fn proxy_ca_defaults_live_under_brand_config_dir() {
        let (cert, key) = resolve_proxy_ca_paths(None, None).unwrap();
        let expected_dir = dirs::config_dir()
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| PathBuf::from(".")))
            .join(RAYLINE_CONFIG_DIR_NAME);
        assert_eq!(cert, expected_dir.join("proxy-ca.pem"));
        assert_eq!(key, expected_dir.join("proxy-ca-key.pem"));
    }

    /// An explicit `--ca-cert-path` / `--ca-key-path` overrides the default and
    /// is passed through verbatim.
    #[test]
    fn proxy_ca_explicit_paths_take_precedence() {
        let cert = PathBuf::from("/custom/c.pem");
        let key = PathBuf::from("/custom/k.pem");
        let (resolved_cert, resolved_key) =
            resolve_proxy_ca_paths(Some(cert.clone()), Some(key.clone())).unwrap();
        assert_eq!(resolved_cert, cert);
        assert_eq!(resolved_key, key);
    }

    fn parse_serve(argv: &[&str]) -> ServeArgs {
        match Cli::try_parse_from(argv).unwrap().cmd {
            Cmd::Serve(args) => args,
            _ => panic!("expected serve subcommand"),
        }
    }

    #[test]
    fn serve_data_ports_include_every_routing_port() {
        // #82: metrics must not bind a port serve needs for routing.
        let bin = RAYLINE_DAEMON_BIN_NAME;
        let args = parse_serve(&[
            bin,
            "serve",
            "--model-repo",
            "r",
            "--model-file",
            "f.gguf",
            "--adapter-port",
            "21001",
            "--injector-port",
            "21002",
            "--local-router-port",
            "21003",
            "--proxy-port",
            "21004",
        ]);
        let ports = serve_data_ports(&args);
        for port in [21001, 21002, 21003, 21004] {
            assert!(ports.contains(&port), "{port} is missing from {ports:?}");
        }
    }

    fn parse_proxy(argv: &[&str]) -> ProxyArgs {
        match Cli::try_parse_from(argv).unwrap().cmd {
            Cmd::Proxy(args) => args,
            _ => panic!("expected proxy subcommand"),
        }
    }

    #[test]
    fn daemon_defaults_are_rayline_only() {
        assert_eq!(RAYLINE_DAEMON_BIN_NAME, "rld");
        assert_eq!(RAYLINE_DOT_CONFIG_DIR, ".rayline");
        assert_eq!(RAYLINE_CONFIG_DIR_NAME, "rayline");
        assert_eq!(RAYLINE_ROUTER_DEFAULT_URL, "https://api.rayline.ai");
    }

    #[test]
    fn proxy_parses_local_advertisement_flags() {
        let bin = RAYLINE_DAEMON_BIN_NAME;
        let config_path = PathBuf::from("/tmp/rayline-router.json");
        let args = parse_proxy(&[
            bin,
            "proxy",
            "--router-config-path",
            config_path.to_str().unwrap(),
            "--local-available",
            "--local-model-id",
            "local-model",
            "--local-adapter-port",
            "31008",
            "--local-custom",
        ]);

        assert!(args.local_available);
        assert_eq!(args.local_model_id.as_deref(), Some("local-model"));
        assert_eq!(args.local_adapter_port, Some(31008));
        assert!(args.local_custom);
        assert_eq!(args.router_config_path, Some(config_path));
    }

    #[test]
    fn selective_subagent_ids_reads_router_config_subagent_keys() {
        let path = std::env::temp_dir().join(format!(
            "rld-router-config-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(
            &path,
            r#"{"routes":{"subagents":{"Review":{"endpoint":"local","model":"m"},"Explore":{"endpoint":"local","model":"m"}," ":{"endpoint":"local","model":"m"}}}}"#,
        )
        .unwrap();

        assert_eq!(
            selective_subagent_ids(Some(&path)),
            vec!["Explore".to_owned(), "Review".to_owned()]
        );
        let _ = std::fs::remove_file(path);
    }

    /// `--upstream-url` parses into `ServeArgs.upstream_url`; absent it stays
    /// `None` (bundled-llama default).
    #[test]
    fn serve_parses_upstream_url_flag() {
        let bin = RAYLINE_DAEMON_BIN_NAME;
        let with_url = parse_serve(&[
            bin,
            "serve",
            "--model-repo",
            "r",
            "--model-file",
            "f.gguf",
            "--upstream-url",
            "http://127.0.0.1:1234",
        ]);
        assert_eq!(
            with_url.upstream_url.as_deref(),
            Some("http://127.0.0.1:1234")
        );

        let without = parse_serve(&[bin, "serve", "--model-repo", "r", "--model-file", "f.gguf"]);
        assert_eq!(without.upstream_url, None);
    }

    #[test]
    fn serve_parses_model_revision_and_sha256_flags() {
        let bin = RAYLINE_DAEMON_BIN_NAME;
        let args = parse_serve(&[
            bin,
            "serve",
            "--model-repo",
            "r",
            "--model-file",
            "f.gguf",
            "--model-revision",
            "abc123",
            "--model-sha256",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        ]);

        assert_eq!(args.model_revision.as_deref(), Some("abc123"));
        assert_eq!(
            args.model_sha256.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
    }

    /// Custom upstream mode needs no GGUF coordinates: `serve` parses with only
    /// `--upstream-url` (+ `--upstream-model`/`--local-model-id`), since the
    /// bundled-llama download is skipped entirely.
    #[test]
    fn serve_parses_custom_mode_without_model_repo_or_file() {
        let bin = RAYLINE_DAEMON_BIN_NAME;
        let args = parse_serve(&[
            bin,
            "serve",
            "--upstream-url",
            "http://127.0.0.1:1234",
            "--upstream-model",
            "google/gemma-4-e4b",
            "--local-model-id",
            "google/gemma-4-e4b",
        ]);
        assert_eq!(args.model_repo, None);
        assert_eq!(args.model_file, None);
        assert_eq!(args.upstream_url.as_deref(), Some("http://127.0.0.1:1234"));
        assert_eq!(args.upstream_model.as_deref(), Some("google/gemma-4-e4b"));
        assert_eq!(args.local_model_id, "google/gemma-4-e4b");
    }

    /// The custom upstream target has any trailing slash trimmed so the adapter's
    /// appended `/v1/messages` doesn't produce a double slash.
    #[test]
    fn normalize_upstream_target_trims_trailing_slash_and_v1() {
        assert_eq!(
            normalize_upstream_target("http://127.0.0.1:1234/"),
            "http://127.0.0.1:1234"
        );
        assert_eq!(
            normalize_upstream_target("http://127.0.0.1:1234"),
            "http://127.0.0.1:1234"
        );
        // OpenAI-compatible servers are commonly given as `…/v1`; the adapter
        // appends `/v1/messages`, so the suffix must be stripped.
        assert_eq!(
            normalize_upstream_target("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            normalize_upstream_target("  http://localhost:1234/v1/  "),
            "http://localhost:1234"
        );
    }
}
