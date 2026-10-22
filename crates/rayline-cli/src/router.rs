use std::collections::BTreeMap;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttribute, SetForegroundColor};
use crossterm::terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{execute, queue};
use serde_json::Value;
use sha2::{Digest, Sha256};

const DEFAULT_INJECTOR_PORT: u16 = 20809;
const DEFAULT_ADAPTER_PORT: u16 = 20808;
const DEFAULT_PROXY_PORT: u16 = 20810;
pub const DEFAULT_LOCAL_ROUTER_PORT: u16 = 20811;
/// Metrics-control port the isolated proxy self-hosts on, distinct from the
/// shared `rayline_metrics::DEFAULT_METRICS_PORT` (20813) so an isolated and a
/// non-isolated cloud-only session can both expose metrics at once.
const DEFAULT_ISOLATED_METRICS_PORT: u16 = 20814;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(1);
const HEALTH_TIMEOUT_SECONDS: u64 = 240;
const HEALTH_TIMEOUT_DOWNLOAD_SECONDS: u64 = 3600;
const HEALTH_POLL: Duration = Duration::from_secs(1);
pub const PROXY_ROUTING_MODE_ALL: &str = "all";
const TOP_REFRESH_INTERVAL: Duration = Duration::from_millis(500);
const TOP_TRACE_REFRESH_INTERVAL: Duration = Duration::from_secs(2);
const TOP_TRACE_LOOKBACK: Duration = Duration::from_secs(12 * 60 * 60);
const TOP_TRACE_MATCH_WINDOW_MS: u64 = 120_000;
const PROXIED_TRAFFIC_POLICY: &str = "selective_passthrough_path";
pub const PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS: &str = "selective-subagents";
pub const DECISION_PLANE_HOSTED: &str = "hosted";
pub const DECISION_PLANE_LOCAL: &str = "local";
pub const ROUTER_API_MODE_ANTHROPIC: &str = "anthropic";
pub const ROUTER_API_MODE_CODEX: &str = "codex";

fn daemon_name() -> &'static str {
    crate::DAEMON_BIN
}

fn cli_name() -> &'static str {
    crate::CLI_BIN
}

fn daemon_bin_env_var() -> &'static str {
    "RLD_BIN"
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterStatusRequest {
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterStartRequest {
    pub env_name: Option<String>,
    pub model_repo: String,
    pub model_file: String,
    pub model_revision: Option<String>,
    pub model_sha256: Option<String>,
    pub router_url: String,
    pub router_url_explicit: bool,
    pub decision_plane: String,
    pub local_router_port: u16,
    pub router_config_path: Option<PathBuf>,
    pub local_model_id: String,
    /// Custom upstream endpoint root (LM Studio / Ollama / llama.cpp). When set,
    /// `spawn_router` passes `--upstream-url` and skips `--model-repo`/
    /// `--model-file` so the daemon never downloads a GGUF (custom mode).
    pub upstream_url: Option<String>,
    /// Model name the custom endpoint advertises/serves (`--upstream-model`).
    pub upstream_model: Option<String>,
    pub adapter_port: u16,
    pub injector_port: u16,
    pub enable_proxy: bool,
    pub proxy_port: u16,
    pub proxy_routing_mode: String,
    /// Config-only: start the daemon without a bundled local model (no GGUF /
    /// upstream). Used when the static config routes only to named endpoints.
    pub no_local_model: bool,
    /// Override for the router API key the daemon endpoints read from
    /// `RAYLINE_ROUTER_API_KEY` (e.g. the `rayline auth login` key for a hosted
    /// cloud-router endpoint). When `None`, the normal resolution applies.
    pub router_api_key_override: Option<String>,
    /// Codex may-local (Rcl-Rcl/Rcl-K): the local router advertises
    /// `x-rayline-local-*` on its native RCR forward and follows the RCR's 307 to
    /// the on-device adapter. Claude does this in the proxy; codex is proxy-less,
    /// so the local router does it on the native OpenAI Responses forward.
    pub may_local_advertise: bool,
    /// Codex may-local: the hosted RCR URL the on-device adapter must post its
    /// `/v1/usage/update` callback to. The RCR issues the `usage_doc_id` in its
    /// 307, so the placeholder row it owns can only be closed against the RCR —
    /// not the local router (whose decision-plane `router_url` is loopback and
    /// whose `/v1/usage/update` is a stub OK). `None` outside may-local.
    pub usage_callback_url: Option<String>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalProxyStartRequest {
    pub router_url: String,
    pub proxy_port: u16,
    pub proxy_routing_mode: String,
    pub router_config_path: Option<PathBuf>,
    pub local_model_id: String,
    pub adapter_port: u16,
    pub custom_mode: bool,
    pub force_restart: bool,
    pub diagnose: bool,
    pub upstream_ca_path: Option<PathBuf>,
    pub isolated: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProxyLocalConfig {
    local_model_id: String,
    adapter_port: u16,
    custom_mode: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterLogsRequest {
    pub lines: i64,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterTopRequest {
    pub json: bool,
    pub show_all: bool,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterStopRequest {
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouterStartCliRequest {
    /// Inbound API surface to expose for user-facing clients. `anthropic`
    /// preserves the transparent Claude proxy startup; `codex` starts the same
    /// local router without the proxy and expects clients to use /v1/responses.
    pub api_mode: String,
    /// Proxy routing mode: `PROXY_ROUTING_MODE_ALL` or
    /// `PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS`. Ignored when `config_path` is set
    /// (the scope is then derived from the config's `routes.main`).
    pub proxy_routing_mode: String,
    /// v2: a `RouterConfig` file (`--config`) driving BOTH main + subagents for
    /// headless/agent use. When set, the daemon runs config-driven (decision plane
    /// local, scope from `routes.main`).
    pub config_path: Option<PathBuf>,
    /// Codex provider auth behavior. `Auto` mirrors `rayline codex`: subscription
    /// passthrough when no config is supplied, no client auth for explicit configs.
    pub codex_auth_mode: crate::codex::CodexAuthMode,
    pub root_env_explicit: bool,
    /// Pre-resolved hosted-RCR router key (`rlk-`) to inject as
    /// `RAYLINE_ROUTER_API_KEY` for the spawned daemon, so a config whose route
    /// targets a `rayline-cloud` endpoint authenticates without a manual env var.
    /// The Codex run/app paths populate this via
    /// [`crate::codex::resolve_cloud_router_key`]; `None` falls back to the env.
    pub router_api_key_override: Option<String>,
}

impl RouterStatusRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl RouterStartRequest {
    /// Base request with no model: there is deliberately no bundled default —
    /// the model always comes from the `local_model` config, layered on via
    /// `from_local_model` (a request must never reach `spawn_router` with
    /// these fields still empty).
    pub fn defaults(root_env_explicit: bool) -> Self {
        Self {
            env_name: None,
            model_repo: String::new(),
            model_file: String::new(),
            model_revision: None,
            model_sha256: None,
            router_url: crate::ROUTER_PROD_URL.to_owned(),
            router_url_explicit: false,
            decision_plane: DECISION_PLANE_HOSTED.to_owned(),
            local_router_port: DEFAULT_LOCAL_ROUTER_PORT,
            router_config_path: None,
            local_model_id: String::new(),
            upstream_url: None,
            upstream_model: None,
            adapter_port: DEFAULT_ADAPTER_PORT,
            injector_port: DEFAULT_INJECTOR_PORT,
            enable_proxy: false,
            proxy_port: DEFAULT_PROXY_PORT,
            proxy_routing_mode: PROXY_ROUTING_MODE_ALL.to_owned(),
            no_local_model: false,
            router_api_key_override: None,
            may_local_advertise: false,
            usage_callback_url: None,
            root_env_explicit,
        }
    }

    /// Local-router launches need a one-command bundled llama.cpp path, while
    /// ordinary local router starts remain config-driven through `local_model`.
    pub fn local_router_defaults(root_env_explicit: bool) -> Self {
        let mut request = Self::defaults(root_env_explicit);
        request.decision_plane = DECISION_PLANE_LOCAL.to_owned();
        request.local_router_port = DEFAULT_LOCAL_ROUTER_PORT;
        request.router_url = format!("http://127.0.0.1:{DEFAULT_LOCAL_ROUTER_PORT}");
        request.router_url_explicit = true;
        request
    }

    /// Layer a local-model config onto a model-less `base` request.
    ///
    /// `Recommended` mode fills the GGUF coordinates from the user's curated
    /// pick (repo/file plus revision/SHA, advertising the catalog `model_id`
    /// to the classifier). `Custom` mode points the daemon at the user's
    /// endpoint: it sets `upstream_url`/`upstream_model` (so `spawn_router`
    /// emits `--upstream-url`/`--upstream-model` and skips the GGUF args) and
    /// overrides `local_model_id` with the user's model so the injector
    /// advertises the real model name to the classifier. Incomplete configs
    /// (no pick, or custom missing URL/model) return `base` unchanged — the
    /// engagement gates (`resolve_engageable_local_config` in claude,
    /// `resolve_start_model` in router start) never pass one in, and a
    /// model-less request fails visibly at the daemon rather than silently
    /// serving something the user didn't choose.
    pub fn from_local_model(cfg: &crate::local_model::LocalModelConfig, base: Self) -> Self {
        match cfg.mode {
            crate::local_model::LocalModelMode::Recommended => {
                let (Some(model_repo), Some(model_file), Some(model_revision), Some(model_sha256)) = (
                    cfg.model_repo.clone(),
                    cfg.model_file.clone(),
                    cfg.model_revision.clone(),
                    cfg.model_sha256.clone(),
                ) else {
                    return base;
                };
                Self {
                    model_repo,
                    model_file,
                    model_revision: Some(model_revision),
                    model_sha256: Some(model_sha256),
                    local_model_id: cfg.model_id.clone().unwrap_or(base.local_model_id),
                    ..base
                }
            }
            crate::local_model::LocalModelMode::Custom => {
                let (Some(upstream_url), Some(model)) = (cfg.base_url.clone(), cfg.model.clone())
                else {
                    return base;
                };
                Self {
                    upstream_url: Some(upstream_url),
                    upstream_model: Some(model.clone()),
                    model_revision: None,
                    model_sha256: None,
                    local_model_id: model,
                    ..base
                }
            }
        }
    }

    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl RouterLogsRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl RouterTopRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        false
    }
}

impl RouterStartCliRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl RouterStopRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

pub async fn render_status(_request: &RouterStatusRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    render_status_from_home(&home).await
}

pub fn render_logs(request: &RouterLogsRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(tail_log(&RouterPaths::new(&home).log_file, request.lines))
}

pub async fn render_top(request: &RouterTopRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    render_top_from_home(&home, request).await
}

async fn render_top_from_home(home: &Path, request: &RouterTopRequest) -> io::Result<String> {
    let serve_meta = read_meta(&RouterPaths::new(home).meta_file);
    let proxy_meta = read_meta(&RouterPaths::new(home).proxy_meta_file);
    let isolated_proxy_meta = read_meta(&RouterPaths::new_isolated(home).proxy_meta_file);
    let candidates = metrics_port_candidates(&serve_meta, &proxy_meta, &isolated_proxy_meta);
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(io::Error::other)?;
    let metrics_port = first_reachable_metrics_port(&client, &candidates).await;
    let url = format!("http://127.0.0.1:{metrics_port}/v1/router/top/snapshot");
    let mut trace_cache = ClaudeTraceCache::new(home);

    if request.json {
        let mut snapshot = fetch_top_snapshot(&client, &url).await?;
        trace_cache.enrich_snapshot(&mut snapshot);
        filter_top_snapshot(&mut snapshot, request.show_all);
        return serde_json::to_string_pretty(&snapshot)
            .map(|mut json| {
                json.push('\n');
                json
            })
            .map_err(io::Error::other);
    }

    if !io::stdout().is_terminal() {
        let mut snapshot = fetch_top_snapshot(&client, &url).await?;
        trace_cache.enrich_snapshot(&mut snapshot);
        return Ok(format_top_snapshot(&snapshot, request.show_all));
    }

    run_top_tui(&client, &url, trace_cache, request.show_all).await?;
    Ok(String::new())
}

async fn fetch_top_snapshot(client: &reqwest::Client, url: &str) -> io::Result<Value> {
    client
        .get(url)
        .send()
        .await
        .map_err(|error| {
            io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("router metrics endpoint is not available: {error}"),
            )
        })?
        .error_for_status()
        .map_err(io::Error::other)?
        .json::<Value>()
        .await
        .map_err(io::Error::other)
}

struct ClaudeTraceCache {
    home: PathBuf,
    cwd: Option<PathBuf>,
    last_refresh: Option<Instant>,
    usages: Vec<TraceUsage>,
}

impl ClaudeTraceCache {
    fn new(home: &Path) -> Self {
        Self {
            home: home.to_path_buf(),
            cwd: std::env::current_dir().ok(),
            last_refresh: None,
            usages: Vec::new(),
        }
    }

    fn enrich_snapshot(&mut self, snapshot: &mut Value) {
        self.refresh_if_stale();
        enrich_top_snapshot_with_trace_usages(snapshot, &self.usages);
    }

    fn refresh_if_stale(&mut self) {
        if self
            .last_refresh
            .is_some_and(|last| last.elapsed() < TOP_TRACE_REFRESH_INTERVAL)
        {
            return;
        }
        self.usages = load_claude_trace_usages(&self.home, self.cwd.as_deref());
        self.last_refresh = Some(Instant::now());
    }
}

#[derive(Clone, Debug)]
struct TraceUsage {
    timestamp_ms: u64,
    request_id: Option<String>,
    message_id: Option<String>,
    agent_type: Option<String>,
    model: Option<String>,
    input_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    output_tokens: u64,
}

impl TraceUsage {
    fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    fn cache_ratio(&self) -> Option<f64> {
        let total = self.total_input_tokens();
        (total > 0).then(|| self.cache_read_input_tokens as f64 / total as f64)
    }
}

fn enrich_top_snapshot_with_trace_usages(snapshot: &mut Value, usages: &[TraceUsage]) {
    if usages.is_empty() {
        return;
    }
    let mut total_input_delta = 0i128;
    let mut total_output_delta = 0i128;
    let Some(rows) = snapshot.get_mut("recent").and_then(Value::as_array_mut) else {
        return;
    };
    for row in rows {
        if value_str(row, "state") != "completed" {
            continue;
        }
        let Some((input_delta, output_delta)) = enrich_top_row_with_trace_usage(row, usages) else {
            continue;
        };
        total_input_delta += input_delta;
        total_output_delta += output_delta;
    }
    if let Some(totals) = snapshot.get_mut("totals").and_then(Value::as_object_mut) {
        adjust_object_u64(totals, "input_tokens", total_input_delta);
        adjust_object_u64(totals, "output_tokens", total_output_delta);
    }
}

fn enrich_top_row_with_trace_usage(row: &mut Value, usages: &[TraceUsage]) -> Option<(i128, i128)> {
    let usage = find_matching_trace_usage(row, usages)?;
    let old_input = row_u64(row, "input_tokens").unwrap_or(0);
    let old_output = row_u64(row, "output_tokens").unwrap_or(0);
    let total_input = usage.total_input_tokens();

    set_row_u64(row, "input_tokens", total_input);
    set_row_u64(row, "prompt_tokens", total_input);
    set_row_u64(row, "prompt_cache_tokens", usage.cache_read_input_tokens);
    set_row_u64(row, "output_tokens", usage.output_tokens);
    if let Some(ratio) = usage.cache_ratio() {
        set_row_f64(row, "cache_hit_ratio", ratio);
    }
    if let Some(output_tps) = trace_output_tps(row, usage.output_tokens) {
        set_row_f64(row, "output_tps", output_tps);
    }
    if let Some(request_id) = usage.request_id.as_deref() {
        set_row_str(row, "provider_request_id", request_id);
    }
    set_row_str(row, "metrics_overlay", "claude_trace");

    Some((
        total_input as i128 - old_input as i128,
        usage.output_tokens as i128 - old_output as i128,
    ))
}

fn find_matching_trace_usage<'a>(row: &Value, usages: &'a [TraceUsage]) -> Option<&'a TraceUsage> {
    let row_target = value_str(row, "target");
    let row_model = match value_str(row, "selected_model") {
        "-" => value_str(row, "requested_model"),
        model => model,
    };
    let row_agent = value_str(row, "agent_type");
    let row_output = row_u64(row, "output_tokens");
    let row_completed_at = row_u64(row, "completed_at_unix_ms")?;
    usages
        .iter()
        .filter(|usage| {
            let remote_row = row_target != "local";
            if remote_row && usage.request_id.is_none() {
                return false;
            }
            if !remote_row && usage.request_id.is_some() {
                return false;
            }
            if !models_match(row_model, usage.model.as_deref()) {
                return false;
            }
            if row_agent != "-"
                && usage
                    .agent_type
                    .as_deref()
                    .is_some_and(|agent| !agent.eq_ignore_ascii_case(row_agent))
            {
                return false;
            }
            if row_target == "local"
                && row_output.is_some_and(|output| output != usage.output_tokens)
            {
                return false;
            }
            timestamp_diff_ms(row_completed_at, usage.timestamp_ms) <= TOP_TRACE_MATCH_WINDOW_MS
        })
        .min_by_key(|usage| timestamp_diff_ms(row_completed_at, usage.timestamp_ms))
}

fn trace_output_tps(row: &Value, output_tokens: u64) -> Option<f64> {
    if output_tokens == 0 {
        return None;
    }
    let completed_or_duration = row_completed_at_ms(row)
        .zip(row_u64(row, "started_at_unix_ms"))
        .map(|(completed, started)| completed.saturating_sub(started))
        .or_else(|| row_u64(row, "duration_ms"))?;
    let generation_ms = row_u64(row, "first_token_at_unix_ms")
        .zip(row_completed_at_ms(row))
        .map(|(first, completed)| completed.saturating_sub(first))
        .unwrap_or(completed_or_duration);
    (generation_ms > 0).then(|| output_tokens as f64 / (generation_ms as f64 / 1000.0))
}

fn row_completed_at_ms(row: &Value) -> Option<u64> {
    row_u64(row, "completed_at_unix_ms").or_else(|| {
        row_u64(row, "started_at_unix_ms")
            .zip(row_u64(row, "duration_ms"))
            .map(|(started, duration)| started.saturating_add(duration))
    })
}

fn timestamp_diff_ms(left: u64, right: u64) -> u64 {
    left.abs_diff(right)
}

fn models_match(row_model: &str, trace_model: Option<&str>) -> bool {
    if row_model == "-" {
        return true;
    }
    let Some(trace_model) = trace_model.filter(|model| !model.is_empty()) else {
        return false;
    };
    row_model == trace_model
        || row_model.contains(trace_model)
        || trace_model.contains(row_model)
        || short_model(row_model) == short_model(trace_model)
}

fn short_model(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn load_claude_trace_usages(home: &Path, cwd: Option<&Path>) -> Vec<TraceUsage> {
    let mut trace_files = Vec::new();
    for project_dir in claude_trace_project_dirs(home, cwd) {
        collect_recent_trace_files(&project_dir, &mut trace_files);
    }
    let mut deduped = BTreeMap::new();
    for file in trace_files {
        let Ok(contents) = fs::read_to_string(&file) else {
            continue;
        };
        for (line_idx, line) in contents.lines().enumerate() {
            let Some(usage) = trace_usage_from_line(line) else {
                continue;
            };
            let key = usage
                .request_id
                .clone()
                .or_else(|| usage.message_id.clone())
                .unwrap_or_else(|| format!("{}:{line_idx}", file.display()));
            deduped.insert(key, usage);
        }
    }
    deduped.into_values().collect()
}

fn claude_trace_project_dirs(home: &Path, cwd: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(config_dir) = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        roots.push(config_dir.join("projects"));
    }
    roots.push(home.join(".claude").join("projects"));

    let mut dirs = Vec::new();
    if let Some(cwd) = cwd {
        let slug = claude_project_slug(cwd);
        for root in &roots {
            let dir = root.join(&slug);
            if dir.is_dir() {
                dirs.push(dir);
            }
        }
    }
    dirs
}

fn collect_recent_trace_files(project_dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(project_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            if trace_file_is_recent(&path) {
                files.push(path);
            }
            continue;
        }
        if path.is_dir() {
            collect_recent_subagent_trace_files(&path.join("subagents"), files);
        }
    }
}

fn collect_recent_subagent_trace_files(subagents_dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(subagents_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
            && trace_file_is_recent(&path)
        {
            files.push(path);
        }
    }
}

fn trace_file_is_recent(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age <= TOP_TRACE_LOOKBACK)
}

fn trace_usage_from_line(line: &str) -> Option<TraceUsage> {
    let value = serde_json::from_str::<Value>(line).ok()?;
    let usage = value.pointer("/message/usage")?;
    let timestamp_ms = value
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_trace_timestamp_ms)?;
    let input_tokens = usage
        .get("input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_creation_input_tokens = usage
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let cache_read_input_tokens = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if input_tokens == 0
        && cache_creation_input_tokens == 0
        && cache_read_input_tokens == 0
        && output_tokens == 0
    {
        return None;
    }
    Some(TraceUsage {
        timestamp_ms,
        request_id: value
            .get("requestId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        message_id: value
            .pointer("/message/id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        agent_type: value
            .get("attributionAgent")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        model: value
            .pointer("/message/model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        input_tokens,
        cache_creation_input_tokens,
        cache_read_input_tokens,
        output_tokens,
    })
}

fn parse_trace_timestamp_ms(value: &str) -> Option<u64> {
    let value = value.strip_suffix('Z')?;
    let (date, time) = value.split_once('T')?;
    let mut date_parts = date.split('-');
    let year = date_parts.next()?.parse::<i32>().ok()?;
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    let mut time_parts = time.split(':');
    let hour = time_parts.next()?.parse::<u32>().ok()?;
    let minute = time_parts.next()?.parse::<u32>().ok()?;
    let second_part = time_parts.next()?;
    let (second, millis) = match second_part.split_once('.') {
        Some((second, fraction)) => {
            let millis = fraction
                .chars()
                .take(3)
                .collect::<String>()
                .parse::<u64>()
                .ok()
                .unwrap_or(0);
            (second.parse::<u32>().ok()?, millis)
        }
        None => (second_part.parse::<u32>().ok()?, 0),
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days
        .checked_mul(86_400)?
        .checked_add(hour as i64 * 3_600 + minute as i64 * 60 + second as i64)?;
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1000)?
        .checked_add(millis)
}

fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = year - if month <= 2 { 1 } else { 0 };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month_prime = month as i32 + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day as i32 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era as i64 * 146_097 + day_of_era as i64 - 719_468
}

fn claude_project_slug(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect()
}

fn row_u64(row: &Value, key: &str) -> Option<u64> {
    row.get(key).and_then(Value::as_u64)
}

fn set_row_u64(row: &mut Value, key: &str, value: u64) {
    if let Some(obj) = row.as_object_mut() {
        obj.insert(key.to_owned(), Value::from(value));
    }
}

fn set_row_f64(row: &mut Value, key: &str, value: f64) {
    if let Some(obj) = row.as_object_mut() {
        if let Some(number) = serde_json::Number::from_f64(value) {
            obj.insert(key.to_owned(), Value::Number(number));
        }
    }
}

fn set_row_str(row: &mut Value, key: &str, value: &str) {
    if let Some(obj) = row.as_object_mut() {
        obj.insert(key.to_owned(), Value::String(value.to_owned()));
    }
}

fn adjust_object_u64(obj: &mut serde_json::Map<String, Value>, key: &str, delta: i128) {
    if delta == 0 {
        return;
    }
    let current = obj.get(key).and_then(Value::as_u64).unwrap_or(0);
    let next = if delta > 0 {
        current.saturating_add(delta.min(u64::MAX as i128) as u64)
    } else {
        current.saturating_sub((-delta).min(u64::MAX as i128) as u64)
    };
    obj.insert(key.to_owned(), Value::from(next));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TopSort {
    Started,
    Ttft,
    Throughput,
}

impl TopSort {
    fn next(self) -> Self {
        match self {
            Self::Started => Self::Ttft,
            Self::Ttft => Self::Throughput,
            Self::Throughput => Self::Started,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Ttft => "ttft",
            Self::Throughput => "tg",
        }
    }
}

struct TopTerminalGuard;

impl TopTerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> io::Result<Self> {
        terminal::enable_raw_mode().map_err(io::Error::other)?;
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(error);
        }
        Ok(Self)
    }
}

impl Drop for TopTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

async fn run_top_tui(
    client: &reqwest::Client,
    url: &str,
    mut trace_cache: ClaudeTraceCache,
    show_all: bool,
) -> io::Result<()> {
    let mut stdout = io::stdout();
    let _guard = TopTerminalGuard::enter(&mut stdout)?;
    let mut snapshot = Value::Null;
    let mut last_error: Option<String> = None;
    let mut last_fetch: Option<Instant> = None;
    let mut paused = false;
    let mut force_refresh = true;
    let mut needs_draw = true;
    let mut sort = TopSort::Started;
    let mut show_all = show_all;

    loop {
        let should_refresh = force_refresh
            || (!paused
                && last_fetch
                    .map(|last| last.elapsed() >= TOP_REFRESH_INTERVAL)
                    .unwrap_or(true));
        if should_refresh {
            match fetch_top_snapshot(client, url).await {
                Ok(mut next_snapshot) => {
                    trace_cache.enrich_snapshot(&mut next_snapshot);
                    snapshot = next_snapshot;
                    last_error = None;
                }
                Err(error) => {
                    last_error = Some(error.to_string());
                }
            }
            last_fetch = Some(Instant::now());
            force_refresh = false;
            needs_draw = true;
        }

        if needs_draw {
            draw_top(
                &mut stdout,
                &snapshot,
                last_error.as_deref(),
                paused,
                sort,
                show_all,
            )?;
            needs_draw = false;
        }

        if event::poll(Duration::from_millis(100)).map_err(io::Error::other)? {
            match event::read().map_err(io::Error::other)? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => break,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                    KeyCode::Char('p') => {
                        paused = !paused;
                        needs_draw = true;
                    }
                    KeyCode::Char('r') => force_refresh = true,
                    KeyCode::Char('s') => {
                        sort = sort.next();
                        needs_draw = true;
                    }
                    KeyCode::Char('a') => {
                        show_all = !show_all;
                        needs_draw = true;
                    }
                    _ => {}
                },
                Event::Resize(_, _) => needs_draw = true,
                _ => {}
            }
        }
    }

    Ok(())
}

fn draw_top(
    stdout: &mut io::Stdout,
    snapshot: &Value,
    last_error: Option<&str>,
    paused: bool,
    sort: TopSort,
    show_all: bool,
) -> io::Result<()> {
    let (width, height) = terminal::size().map_err(io::Error::other)?;
    queue!(stdout, MoveTo(0, 0), Clear(ClearType::All))?;

    let mut y = 0;
    y = draw_title(stdout, y, width, "Rayline Local Router")?;
    y = draw_top_summary(stdout, y, width, snapshot, paused, sort, show_all)?;
    if let Some(error) = last_error {
        y = draw_colored_line(
            stdout,
            y,
            width,
            Color::Red,
            &format!("last fetch error: {error}"),
        )?;
    }
    y = draw_line(stdout, y, width, "")?;

    let active = sorted_top_rows(snapshot, "active", sort, show_all);
    let recent = sorted_top_rows(snapshot, "recent", sort, show_all);
    let remaining = height.saturating_sub(y).saturating_sub(1);
    let active_budget = if remaining > 12 {
        remaining / 2
    } else {
        remaining
    };
    y = draw_table_section(
        stdout,
        y,
        width,
        y.saturating_add(active_budget),
        "Active",
        &active,
    )?;
    if y < height.saturating_sub(1) {
        y = draw_line(stdout, y, width, "")?;
    }
    let footer_y = height.saturating_sub(1);
    y = draw_table_section(stdout, y, width, footer_y, "Recent", &recent)?;

    let _ = y;
    let footer = "q/Esc quit  p pause  r refresh  s sort  a all/llm  | daemon-memory metrics  | --json for scripts";
    queue!(
        stdout,
        MoveTo(0, footer_y),
        Clear(ClearType::CurrentLine),
        SetForegroundColor(Color::DarkGrey),
        Print(fit_width(footer, width as usize)),
        ResetColor
    )?;
    stdout.flush()
}

fn draw_top_summary(
    stdout: &mut io::Stdout,
    mut y: u16,
    width: u16,
    snapshot: &Value,
    paused: bool,
    sort: TopSort,
    show_all: bool,
) -> io::Result<u16> {
    let totals = snapshot.get("totals").unwrap_or(&Value::Null);
    let active = visible_row_count(snapshot, "active", show_all);
    let recent = visible_row_count(snapshot, "recent", show_all);
    let hidden = hidden_proxied_row_count(snapshot);
    y = draw_line(
        stdout,
        y,
        width,
        &format!(
            "current active={active}  recent={recent}  traffic={}{}  mode={}  sort={}  refresh={}ms",
            if show_all { "all" } else { "llm" },
            if !show_all && hidden > 0 {
                format!(" hidden-proxied={hidden}")
            } else {
                String::new()
            },
            if paused { "paused" } else { "live" },
            sort.label(),
            TOP_REFRESH_INTERVAL.as_millis(),
        ),
    )?;
    draw_line(stdout, y, width, &{
        let uncertain = totals_u64(totals, "routing_uncertain");
        let uncertain_label = if uncertain > 0 {
            format!("  routing_uncertain={uncertain}")
        } else {
            String::new()
        };
        format!(
            "overall completed={}  errors={}  local={}  remote={}  input={}  output={}{}  {}",
            totals_u64(totals, "completed_requests"),
            totals_u64(totals, "errored_requests"),
            totals_u64(totals, "local_requests"),
            totals_u64(totals, "remote_requests"),
            totals_u64(totals, "input_tokens"),
            totals_u64(totals, "output_tokens"),
            uncertain_label,
            llama_perf_summary(snapshot),
        )
    })
}

fn llama_perf_summary(snapshot: &Value) -> String {
    let Some(perf) = snapshot.get("llama_perf").filter(|value| !value.is_null()) else {
        return "llama.cpp pf=- tg=-".to_owned();
    };
    format!(
        "llama.cpp pf={} tg={} updated={}",
        rate_cell(perf, "prefill_tokens_per_second"),
        rate_cell(perf, "generation_tokens_per_second"),
        perf.get("updated_at_unix_ms")
            .and_then(Value::as_u64)
            .map(format_unix_ms_age)
            .unwrap_or_else(|| "-".to_owned()),
    )
}

fn draw_table_section(
    stdout: &mut io::Stdout,
    mut y: u16,
    width: u16,
    bottom_exclusive: u16,
    title: &str,
    rows: &[Value],
) -> io::Result<u16> {
    if y >= bottom_exclusive {
        return Ok(y);
    }

    y = draw_heading(
        stdout,
        y,
        width,
        &format!("{title} requests ({})", rows.len()),
    )?;
    if y >= bottom_exclusive {
        return Ok(y);
    }

    y = draw_colored_line(stdout, y, width, Color::DarkGrey, &top_table_header(width))?;
    if rows.is_empty() {
        if y < bottom_exclusive {
            y = draw_line(stdout, y, width, "no requests")?;
        }
        return Ok(y);
    }

    let available = bottom_exclusive.saturating_sub(y) as usize;
    let rendered_rows = rows.len().min(available);
    for row in rows.iter().take(rendered_rows) {
        let rendered = top_table_row(row, width);
        y = match top_row_color(row) {
            Some(color) => draw_colored_line(stdout, y, width, color, &rendered)?,
            None => draw_line(stdout, y, width, &rendered)?,
        };
    }
    if rows.len() > rendered_rows && y < bottom_exclusive {
        y = draw_colored_line(
            stdout,
            y,
            width,
            Color::DarkGrey,
            &format!("... {} more", rows.len() - rendered_rows),
        )?;
    }
    Ok(y)
}

fn draw_title(stdout: &mut io::Stdout, y: u16, width: u16, text: &str) -> io::Result<u16> {
    queue!(
        stdout,
        MoveTo(0, y),
        SetAttribute(Attribute::Bold),
        SetForegroundColor(Color::Cyan),
        Print(fit_width(text, width as usize)),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(y.saturating_add(1))
}

fn draw_heading(stdout: &mut io::Stdout, y: u16, width: u16, text: &str) -> io::Result<u16> {
    queue!(
        stdout,
        MoveTo(0, y),
        SetAttribute(Attribute::Bold),
        Print(fit_width(text, width as usize)),
        SetAttribute(Attribute::Reset)
    )?;
    Ok(y.saturating_add(1))
}

fn draw_colored_line(
    stdout: &mut io::Stdout,
    y: u16,
    width: u16,
    color: Color,
    text: &str,
) -> io::Result<u16> {
    queue!(
        stdout,
        MoveTo(0, y),
        SetForegroundColor(color),
        Print(fit_width(text, width as usize)),
        ResetColor
    )?;
    Ok(y.saturating_add(1))
}

fn draw_line(stdout: &mut io::Stdout, y: u16, width: u16, text: &str) -> io::Result<u16> {
    queue!(stdout, MoveTo(0, y), Print(fit_width(text, width as usize)))?;
    Ok(y.saturating_add(1))
}

fn top_row_color(row: &Value) -> Option<Color> {
    (value_str(row, "target") == "local").then_some(Color::DarkCyan)
}

fn sorted_top_rows(snapshot: &Value, key: &str, sort: TopSort, show_all: bool) -> Vec<Value> {
    let mut rows = snapshot
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !show_all {
        rows.retain(|row| !is_proxied_traffic(row));
    }
    rows.sort_by(|left, right| {
        top_sort_value(right, sort)
            .partial_cmp(&top_sort_value(left, sort))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows
}

fn top_sort_value(row: &Value, sort: TopSort) -> f64 {
    match sort {
        TopSort::Started => row
            .get("started_at_unix_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0) as f64,
        TopSort::Ttft => row.get("ttft_ms").and_then(Value::as_u64).unwrap_or(0) as f64,
        TopSort::Throughput => row.get("output_tps").and_then(Value::as_f64).unwrap_or(0.0),
    }
}

fn top_table_header(width: u16) -> String {
    if width < 120 {
        format!(
            "{:<14} {:<10} {:<7} {:<10} {:>7} {:>7} {:>7} {:>6} {:>6} MODEL",
            "REQ", "AGENT", "TARGET", "STATE", "OUT", "CACHE %", "TTFT", "PF", "TG"
        )
    } else {
        format!(
            "{:<18} {:<12} {:<8} {:<10} {:>7} {:>7} {:>7} {:>8} {:>8} {:>6} {:>6} MODEL / POLICY",
            "REQ", "AGENT", "TARGET", "STATE", "IN", "OUT", "CACHE %", "AGE", "TTFT", "PF", "TG"
        )
    }
}

fn top_table_row(row: &Value, width: u16) -> String {
    let request_id = truncate_cell(
        value_str(row, "request_id"),
        if width < 120 { 14 } else { 18 },
    );
    let agent = truncate_cell(
        value_str(row, "agent_type"),
        if width < 120 { 10 } else { 12 },
    );
    let target_value = top_target_cell(row);
    let target = truncate_cell(&target_value, 8);
    let state = truncate_cell(value_str(row, "state"), 10);
    let output_tokens = count_cell(row, "output_tokens");
    let ttft = ms_cell(row, "ttft_ms");
    let prefill_tps = rate_cell(row, "prefill_tps");
    let generation_tps = rate_cell(row, "output_tps");
    let cache_hit = percent_cell(row, "cache_hit_ratio");
    let model = top_model_cell(row);

    if width < 120 {
        return format!(
            "{request_id:<14} {agent:<10} {target:<7} {state:<10} {output_tokens:>7} {cache_hit:>7} {ttft:>7} {prefill_tps:>6} {generation_tps:>6} {model}"
        );
    }

    let input_tokens = count_cell(row, "input_tokens");
    let age = ms_cell(row, "duration_ms");
    format!(
        "{request_id:<18} {agent:<12} {target:<8} {state:<10} {input_tokens:>7} {output_tokens:>7} {cache_hit:>7} {age:>8} {ttft:>8} {prefill_tps:>6} {generation_tps:>6} {model}"
    )
}

fn format_top_snapshot(snapshot: &Value, show_all: bool) -> String {
    let active = visible_row_count(snapshot, "active", show_all);
    let recent = visible_row_count(snapshot, "recent", show_all);
    let hidden = hidden_proxied_row_count(snapshot);
    let totals = snapshot.get("totals").unwrap_or(&Value::Null);
    let completed = totals
        .get("completed_requests")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let errored = totals
        .get("errored_requests")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let mut output = format!(
        "Rayline router top snapshot\nactive: {active}  recent: {recent}  completed: {completed}  errors: {errored}  traffic: {}{}\n",
        if show_all { "all" } else { "llm" },
        if !show_all && hidden > 0 {
            format!("  hidden-proxied: {hidden}")
        } else {
            String::new()
        }
    );
    if let Some(rows) = snapshot.get("active").and_then(Value::as_array) {
        for row in rows {
            if !show_all && is_proxied_traffic(row) {
                continue;
            }
            output.push_str(&format_top_row(row));
        }
    }
    if active == 0 {
        output.push_str("no active requests\n");
    }
    output
}

fn format_top_row(row: &Value) -> String {
    let request_id = value_str(row, "request_id");
    let agent = value_str(row, "agent_type");
    let target = top_target_cell(row);
    let model = value_str(row, "selected_model");
    let state = value_str(row, "state");
    let ttft = ms_cell(row, "ttft_ms");
    let output_tokens = count_cell(row, "output_tokens");
    let prefill_tps = rate_cell(row, "prefill_tps");
    let generation_tps = rate_cell(row, "output_tps");
    let cache_hit = percent_cell(row, "cache_hit_ratio");
    format!(
        "{request_id:<22} {agent:<12} {target:<10} {state:<10} out={output_tokens:<7} cache%={cache_hit:<7} ttft={ttft:<8} pf={prefill_tps:<6} tg={generation_tps:<6} {model}\n"
    )
}

fn value_str<'a>(row: &'a Value, key: &str) -> &'a str {
    row.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("-")
}

fn totals_u64(totals: &Value, key: &str) -> u64 {
    totals.get(key).and_then(Value::as_u64).unwrap_or(0)
}

fn count_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_u64)
        .map(|value| format_compact_number(value as f64, 0))
        .unwrap_or_else(|| "-".to_owned())
}

fn ms_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_u64)
        .map(format_ms)
        .unwrap_or_else(|| "-".to_owned())
}

fn format_ms(value: u64) -> String {
    if value < 1000 {
        format!("{value}ms")
    } else {
        format!("{}s", format_compact_number(value as f64 / 1000.0, 1))
    }
}

fn format_unix_ms_age(value: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(value);
    format!("{} ago", format_ms(now.saturating_sub(value)))
}

fn rate_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_f64)
        .map(|value| format_compact_number(value, 1))
        .unwrap_or_else(|| "-".to_owned())
}

fn percent_cell(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_f64)
        .map(|value| format!("{}%", format_compact_number(value * 100.0, 0)))
        .unwrap_or_else(|| "-".to_owned())
}

fn filter_top_snapshot(snapshot: &mut Value, show_all: bool) {
    if show_all {
        return;
    }
    for key in ["active", "recent"] {
        if let Some(rows) = snapshot.get_mut(key).and_then(Value::as_array_mut) {
            rows.retain(|row| !is_proxied_traffic(row));
        }
    }
    let visible_active = visible_row_count(snapshot, "active", true);
    if let Some(totals) = snapshot.get_mut("totals").and_then(Value::as_object_mut) {
        totals.insert("active_requests".to_owned(), Value::from(visible_active));
    }
}

fn visible_row_count(snapshot: &Value, key: &str, show_all: bool) -> usize {
    snapshot
        .get(key)
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .filter(|row| show_all || !is_proxied_traffic(row))
                .count()
        })
        .unwrap_or(0)
}

fn hidden_proxied_row_count(snapshot: &Value) -> usize {
    ["active", "recent"]
        .into_iter()
        .map(|key| {
            snapshot
                .get(key)
                .and_then(Value::as_array)
                .map(|rows| rows.iter().filter(|row| is_proxied_traffic(row)).count())
                .unwrap_or(0)
        })
        .sum()
}

fn is_proxied_traffic(row: &Value) -> bool {
    row.get("policy").and_then(Value::as_str) == Some(PROXIED_TRAFFIC_POLICY)
}

fn top_target_cell(row: &Value) -> String {
    if is_proxied_traffic(row) {
        "proxied".to_owned()
    } else {
        value_str(row, "target").to_owned()
    }
}

fn top_policy_cell(row: &Value) -> String {
    match value_str(row, "policy") {
        PROXIED_TRAFFIC_POLICY => "proxied traffic",
        "selective_main_passthrough" => "main passthrough",
        "selective_subagent_passthrough" => "subagent passthrough",
        "selective_subagent_header" => "subagent routed",
        "selective_virtual_model" => "virtual model",
        "selective_virtual_model_lookup" => "virtual model lookup",
        "selective_provider_model_lookup" => "provider model lookup",
        "selective_model_list" => "model list",
        "router_routed_path" => "router routed",
        "anthropic_passthrough" => "passthrough",
        policy => policy,
    }
    .to_owned()
}

fn format_compact_number(value: f64, plain_decimals: usize) -> String {
    if !value.is_finite() {
        return "-".to_owned();
    }

    let units = [
        (1.0, ""),
        (1_000.0, "k"),
        (1_000_000.0, "M"),
        (1_000_000_000.0, "B"),
        (1_000_000_000_000.0, "T"),
    ];
    let abs = value.abs();
    let mut unit_index = 0;
    while unit_index + 1 < units.len() && abs >= units[unit_index + 1].0 {
        unit_index += 1;
    }
    while unit_index + 1 < units.len() && abs / units[unit_index].0 >= 999.95 {
        unit_index += 1;
    }

    let (scale, suffix) = units[unit_index];
    if unit_index == 0 {
        format!("{value:.plain_decimals$}")
    } else {
        format!("{:.1}{suffix}", value / scale)
    }
}

fn top_model_cell(row: &Value) -> String {
    let model = if is_proxied_traffic(row) {
        value_str(row, "endpoint_id")
    } else {
        match value_str(row, "selected_model") {
            "-" => value_str(row, "requested_model"),
            selected => selected,
        }
    };
    let model = match model {
        "-" => value_str(row, "requested_model"),
        selected => selected,
    };
    let policy = top_policy_cell(row);
    if policy == "-" {
        model.to_owned()
    } else {
        format!("{model} / {policy}")
    }
}

fn truncate_cell(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        fit_width(value, width)
    }
}

fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let mut chars = text.chars();
    let mut output = String::new();
    for _ in 0..width {
        let Some(ch) = chars.next() else {
            return output;
        };
        output.push(ch);
    }
    if chars.next().is_none() {
        return output;
    }
    output.pop();
    output.push('~');
    output
}

/// Filesystem path of the on-device router's combined log. Exposed so a
/// `claude --local-router` launch can point users at live progress during the
/// otherwise-silent wait while the local model loads.
pub fn local_router_log_path(home: &Path) -> PathBuf {
    RouterPaths::new(home).log_file
}

/// Filesystem path of the proxy log, for `--diagnose`. `isolated` selects the
/// `--isolated` proxy's state dir (the `cc/` subdir) so diagnostics point users
/// at the right log for isolated failures.
pub fn proxy_log_path(home: &Path, isolated: bool) -> PathBuf {
    RouterPaths::for_isolation(home, isolated).proxy_log_file
}

pub async fn start(request: &RouterStartRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    // Resolve the daemon binary before the model config so a broken install
    // surfaces as the first error.
    let bin_path = resolve_rld_bin(&home)?;
    let request = resolve_start_model(&home, request.clone()).await?;
    start_from_home_with_rld_bin(&home, &request, &bin_path).await
}

/// Fill the start request's model from the stored `local_model` config — the
/// only model source for `router start`. A Recommended config without a pick
/// adopts the best already-downloaded curated model and persists it as the pick. Errors (with
/// remediation) when nothing is configured, the custom endpoint is
/// incomplete, or the picked model is not downloaded — `router start` never
/// downloads a model. (`claude` resolves its own config and calls
/// `start_from_home` directly, bypassing this.)
async fn resolve_start_model(
    home: &Path,
    request: RouterStartRequest,
) -> io::Result<RouterStartRequest> {
    let cli = cli_name();
    let Some(cfg) = crate::local_model::read_from_home(home) else {
        return Err(io::Error::other(format!(
            "No local model configured. Pick one with `{cli} local use <model-id>` (see `{cli} local models`)."
        )));
    };
    match cfg.mode {
        crate::local_model::LocalModelMode::Custom => {
            if !cfg.is_engageable() {
                return Err(io::Error::other(format!(
                    "Custom local endpoint is incomplete. Set it with `{cli} local custom --url <URL> --model <NAME>`."
                )));
            }
            start_request_from_local_model(home, &cfg, request)
        }
        crate::local_model::LocalModelMode::Recommended => {
            let cfg = if cfg.has_recommended_pick() {
                cfg
            } else {
                let env_name = crate::status::resolve_env(request.env_name.as_deref(), Some(home));
                match crate::catalog::auto_select_downloaded(&env_name).await {
                    Some(model) => {
                        eprintln!(
                            "No local model selected — using downloaded `{id}` (saved as your selection; change with `{cli} local use <model-id>`).",
                            id = model.id,
                        );
                        crate::local_model::set_recommended_in_home(home, &model)?
                    }
                    // Nothing downloaded — a sole saved custom endpoint is the
                    // only added model, so select and serve it (custom mode:
                    // no GGUF check applies).
                    None => match cfg.custom_endpoints.as_slice() {
                        [endpoint] => {
                            eprintln!(
                                "No local model selected — using your saved custom endpoint `{model}` ({url}).",
                                model = endpoint.model,
                                url = endpoint.base_url,
                            );
                            let cfg = crate::local_model::activate_custom_endpoint_in_home(
                                home, endpoint,
                            )?;
                            return start_request_from_local_model(home, &cfg, request);
                        }
                        _ => {
                            return Err(io::Error::other(format!(
                                "No local model selected and none downloaded. Pick one with `{cli} local use <model-id>` (see `{cli} local models`)."
                            )));
                        }
                    },
                }
            };
            if !hf_cache_has_verified_config_gguf(home, &cfg) {
                let model_id = cfg
                    .model_id
                    .as_deref()
                    .or(cfg.model_file.as_deref())
                    .unwrap_or("selected model");
                return Err(io::Error::other(format!(
                    "Local model `{model_id}` is not downloaded. Run `{cli} local download {model_id}`."
                )));
            }
            start_request_from_local_model(home, &cfg, request)
        }
    }
}

fn start_request_from_local_model(
    home: &Path,
    cfg: &crate::local_model::LocalModelConfig,
    request: RouterStartRequest,
) -> io::Result<RouterStartRequest> {
    let mut request = RouterStartRequest::from_local_model(cfg, request);
    if let Some(path) = crate::providers::write_provider_routes_for_local_config(
        home,
        cfg,
        request.router_config_path.as_deref(),
    )
    .map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("failed to write provider routes: {error}"),
        )
    })? {
        request.router_config_path = Some(path);
    }
    Ok(request)
}

pub async fn start_from_home(home: &Path, request: &RouterStartRequest) -> io::Result<String> {
    let bin_path = resolve_rld_bin(home)?;
    start_from_home_with_rld_bin(home, request, &bin_path).await
}

/// Start the local router daemon (router + embedded proxy) without launching
/// Claude Code, then return a status string. This mirrors the local startup that
/// `rayline claude --local` performs, but leaves the daemon running so other
/// clients (e.g. the Anthropic SDK pointed at the proxy on 127.0.0.1:20810) can
/// use it. The model comes from the stored `local_model` config, exactly like
/// `start`.
pub async fn start_from_cli(request: &RouterStartCliRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    let bin_path = resolve_rld_bin(&home)?;
    let mut start_request = RouterStartRequest::local_router_defaults(request.root_env_explicit);
    // A pre-resolved hosted-RCR key (Codex run/app path) is injected as
    // `RAYLINE_ROUTER_API_KEY` for the daemon; `resolve_router_api_key` honors it
    // before the `enable_proxy` gate, so it reaches the cloud leg even in Codex
    // mode (`enable_proxy = false`).
    start_request.router_api_key_override = request.router_api_key_override.clone();
    let codex_mode = request.api_mode == ROUTER_API_MODE_CODEX;
    let codex_subscription_auth = codex_mode
        && request
            .codex_auth_mode
            .effective_for_run(request.config_path.as_ref())
            == crate::codex::EffectiveCodexAuthMode::Subscription;
    start_request.enable_proxy = !codex_mode;

    // v2: `--config <file>` drives BOTH main + subagents for headless/agent use.
    // The local router reads the config's `endpoints` + `routes` directly; scope is
    // derived from `routes.main` (passthrough sentinel → selective-subagents, else
    // route-all). A `"local"` route needs the bundled model; named endpoints
    // (ollama, direct-anthropic, rayline-cloud) are served by the local router
    // directly. Cloud-router endpoints read `RAYLINE_ROUTER_API_KEY` from the env.
    if let Some(path) = request.config_path.as_deref() {
        // Rcl-Rcl (may-local): a cloud-only config whose `rayline-cloud` route declares
        // `local_models`. Route on the hosted cloud router (RCR) but front the
        // config's local endpoint with a custom adapter and advertise it, so the RCR
        // may 307-redirect to it. Cloud decision plane + custom upstream; the daemon
        // reads `RAYLINE_ROUTER_API_KEY` from the env for the RCR.
        if let Some(may_local) = crate::router_config::config_may_local(path) {
            // Claude keeps the hosted decision plane and lets the MITM proxy
            // advertise local + follow the RCR's 307. Codex runs proxy-less and
            // forwards natively, so it uses the local decision plane: the local
            // router itself advertises `x-rayline-local-*` on the native RCR
            // forward and follows the RCR's 307 to the on-device adapter.
            let mut start_request = if codex_mode {
                let mut req = RouterStartRequest::local_router_defaults(request.root_env_explicit);
                // Materialize the codex-native config (rayline-cloud →
                // openai_responses + `x-rayline-client: codex`) so the main
                // forwards natively to the RCR; the local router advertises
                // may-local on that forward. When the config's main is the
                // subscription sentinel (main on ChatGPT, subagents may-local),
                // use the subscription materializer instead — it wires the
                // `codex-subscription` main (so Codex exposes `spawn_agent` and
                // can spawn subagents) while still forwarding the `rayline-cloud`
                // subagent route natively for may-local advertise/307.
                req.router_config_path = Some(if codex_subscription_auth {
                    crate::router_config::materialize_codex_subscription_for_local_router(
                        path, &home,
                    )?
                } else {
                    crate::router_config::materialize_codex_config_for_local_router(path, &home)?
                });
                req.may_local_advertise = true;
                req.router_api_key_override = request.router_api_key_override.clone();
                // The RCR owns the usage_doc_id it issues in the 307, so the
                // adapter's /v1/usage/update must go to the hosted RCR — not the
                // local router (loopback decision plane, stub OK handler). Take the
                // RCR URL straight from the config's hosted-cloud endpoint so it
                // matches exactly what the may-local turn forwards to (prod or dev).
                req.usage_callback_url = crate::router_config::config_hosted_rcr_base_url(path);
                if req.usage_callback_url.is_none() {
                    // may-local is enabled (a route declares `local_models`), but no
                    // hosted-RCR endpoint base_url resolved — so the adapter would
                    // fall back to posting /v1/usage/update at the loopback router,
                    // whose stub can't close the RCR's placeholder row. Surface it
                    // rather than silently losing may-local usage accounting.
                    eprintln!(
                        "warning: may-local is enabled but no hosted-RCR endpoint \
                         (api.rayline.ai / api-dev.rayline.ai) was found in the config; \
                         on-device usage will not be reported to the router."
                    );
                }
                req
            } else {
                RouterStartRequest::defaults(request.root_env_explicit)
            };
            start_request.enable_proxy = !codex_mode;
            start_request.proxy_routing_mode =
                if crate::router_config::config_main_is_passthrough(path) {
                    PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS
                } else {
                    PROXY_ROUTING_MODE_ALL
                }
                .to_owned();
            start_request.upstream_url = Some(may_local.upstream_url);
            start_request.upstream_model = Some(may_local.model.clone());
            start_request.local_model_id = may_local.model;
            return finish_start_from_cli(&home, &start_request, &bin_path, codex_mode).await;
        }
        start_request.router_config_path = Some(if codex_subscription_auth {
            crate::router_config::materialize_codex_subscription_for_local_router(path, &home)?
        } else if codex_mode {
            crate::router_config::materialize_codex_config_for_local_router(path, &home)?
        } else {
            crate::router_config::materialize_for_local_router(path, &home)?
        });
        start_request.proxy_routing_mode =
            if crate::router_config::config_main_is_passthrough(path) {
                PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS
            } else {
                PROXY_ROUTING_MODE_ALL
            }
            .to_owned();
        if crate::router_config::config_uses_local_endpoint(path) {
            let start_request = resolve_start_model(&home, start_request).await?;
            return finish_start_from_cli(&home, &start_request, &bin_path, codex_mode).await;
        }
        start_request.no_local_model = true;
        return finish_start_from_cli(&home, &start_request, &bin_path, codex_mode).await;
    }

    if codex_subscription_auth {
        // Default `rayline codex` (no `--config`): mirror `rayline claude`'s
        // graceful hybrid — main on the ChatGPT subscription, subagents on the
        // on-device model when one is usable. Codex surfaces every subagent as a
        // single generic `collab_spawn`, so this is a `routes.subagent → local`
        // default (not a per-type allowlist like Claude's). When no usable local
        // model exists — or resolving/downloading it fails — fall back to the
        // zero-setup all-subscription behavior instead of blocking.
        //
        // Attempt the hybrid whenever a bundled/recommended local model config
        // exists, and let `resolve_start_model` be the authority on whether it can
        // actually serve — it also handles the "no explicit pick but a curated
        // model is already downloaded" case via `auto_select_downloaded`, which a
        // stricter `is_engageable()` prefilter would wrongly skip. If it can't
        // resolve (nothing downloaded, GGUF missing, etc.) it returns `Err` and we
        // fall back to all-subscription below.
        //
        // Scoped to bundled/recommended models: for a provider-backed local
        // (ollama/lmstudio), `resolve_start_model` would rewrite the router config
        // into a provider-centric one and clobber the subscription `main` route.
        // That advanced shape is better expressed with an explicit `--config`;
        // here we keep `main → subscription` intact.
        let local_configured = crate::local_model::read_from_home(&home)
            .filter(|cfg| crate::providers::provider_from_local_config(cfg).is_none())
            .is_some();
        if local_configured {
            let hybrid = {
                let mut req = start_request.clone();
                req.router_config_path =
                    Some(crate::codex::write_subscription_router_config(&home, true)?);
                resolve_start_model(&home, req).await
            };
            match hybrid {
                Ok(hybrid) => {
                    return finish_start_from_cli(&home, &hybrid, &bin_path, codex_mode).await;
                }
                Err(error) => {
                    eprintln!(
                        "rayline codex: on-device subagent routing unavailable ({error}); \
                         running subagents on your subscription. Set up a local model with \
                         `{} local onboard` to offload them on-device.",
                        cli_name()
                    );
                }
            }
        }
        start_request.router_config_path = Some(crate::codex::write_subscription_router_config(
            &home, false,
        )?);
        start_request.no_local_model = true;
        return finish_start_from_cli(&home, &start_request, &bin_path, codex_mode).await;
    }

    start_request.proxy_routing_mode = request.proxy_routing_mode.clone();
    // Selective-subagents materializes the conservative read-only subagent
    // allowlist as the proxy's static config; all-route leaves it unset so the
    // daemon's built-in model routes apply (requesting `rayline-local` then
    // lands on the on-device model).
    if start_request.proxy_routing_mode == PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS {
        start_request.router_config_path = Some(
            crate::onboarding::write_default_local_routes(&home).map_err(|error| {
                io::Error::other(format!("failed to write default local routes: {error}"))
            })?,
        );
    }
    let start_request = resolve_start_model(&home, start_request).await?;
    finish_start_from_cli(&home, &start_request, &bin_path, codex_mode).await
}

async fn finish_start_from_cli(
    home: &Path,
    start_request: &RouterStartRequest,
    bin_path: &Path,
    codex_mode: bool,
) -> io::Result<String> {
    let mut output = start_from_home_with_rld_bin(home, start_request, bin_path).await?;
    if codex_mode {
        output.push_str(&format!(
            "Codex base URL: http://127.0.0.1:{}/v1\n",
            start_request.local_router_port
        ));
    }
    Ok(output)
}

pub async fn stop(_request: &RouterStopRequest) -> io::Result<String> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    stop_from_home(&home).await
}

pub async fn render_status_from_home(home: &Path) -> io::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|error| io::Error::other(format!("health client setup failed: {error}")))?;
    render_status_from_home_with_client(home, &client).await
}

pub async fn start_from_home_with_rld_bin(
    home: &Path,
    request: &RouterStartRequest,
    bin_path: &Path,
) -> io::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|error| io::Error::other(format!("health client setup failed: {error}")))?;
    start_from_home_with_client(home, request, bin_path, &client).await
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
pub async fn start_proxy_from_home(
    home: &Path,
    router_url: &str,
    router_api_key: &str,
    proxy_port: u16,
    proxy_routing_mode: &str,
    force_restart: bool,
    diagnose: bool,
    upstream_ca_path: Option<&Path>,
    isolated: bool,
    // v2 `--config` (cloud plane): the proxy reads this for its subagent allowlist
    // (`routes.subagents`), so a config can route specific subagent types to the
    // hosted router and pass the rest through. `None` keeps today's behavior.
    router_config_path: Option<&Path>,
) -> io::Result<String> {
    if router_api_key.is_empty() {
        return Err(io::Error::other(
            "RAYLINE_ROUTER_API_KEY is required for proxy mode.",
        ));
    }
    let bin_path = resolve_rld_bin(home)?;
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|error| io::Error::other(format!("health client setup failed: {error}")))?;
    start_proxy_from_home_with_client(
        home,
        router_url,
        router_api_key,
        proxy_port,
        proxy_routing_mode,
        &bin_path,
        force_restart,
        diagnose,
        upstream_ca_path,
        isolated,
        router_config_path,
        None,
        &client,
    )
    .await
}

pub async fn start_local_proxy_from_home(
    home: &Path,
    request: &LocalProxyStartRequest,
) -> io::Result<String> {
    let bin_path = resolve_rld_bin(home)?;
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|error| io::Error::other(format!("health client setup failed: {error}")))?;
    let local = ProxyLocalConfig {
        local_model_id: request.local_model_id.clone(),
        adapter_port: request.adapter_port,
        custom_mode: request.custom_mode,
    };
    start_proxy_from_home_with_client(
        home,
        &request.router_url,
        "",
        request.proxy_port,
        &request.proxy_routing_mode,
        &bin_path,
        request.force_restart,
        request.diagnose,
        request.upstream_ca_path.as_deref(),
        request.isolated,
        request.router_config_path.as_deref(),
        Some(&local),
        &client,
    )
    .await
}

/// Stop a lingering shared serve daemon (`rld serve` + embedded proxy) ONLY
/// when this cloud launch is about to bind the same proxy port the daemon's
/// embedded proxy holds — i.e. when the launch will replace it in place.
///
/// Used by `claude` on a non-isolated proxy launch where the account toggle is
/// confirmed off: such a launch starts a cloud-only proxy on `proxy_port`, but
/// a serve daemon left over from a prior local session still binds that port,
/// so `start_proxy_from_home` would fail with a port conflict and the model
/// would keep occupying RAM/GPU. Stopping the daemon first frees the port for
/// the replacement.
///
/// The stop is gated tightly to avoid taking down a daemon this launch is NOT
/// replacing: only when the serve meta says it has an embedded proxy on exactly
/// `proxy_port` (a daemon on a different port, or with no embedded proxy, does
/// not conflict and may still be serving another session). The caller is
/// responsible for preflighting the replacement binary before calling this, so
/// a launch that cannot start its own proxy never performs the destructive
/// stop.
///
/// Returns `true` when a live serve daemon was actually stopped.
pub async fn stop_serve_daemon_from_home(home: &Path, proxy_port: u16) -> io::Result<bool> {
    let paths = RouterPaths::new(home);
    // Nothing recorded → nothing to stop (the common case); avoid building a
    // client and probing.
    if read_pid(&paths.pid_file).is_none() {
        return Ok(false);
    }
    // Only stop a daemon whose embedded proxy occupies the exact port this
    // launch will rebind — anything else, this launch is not the replacement.
    let meta = read_meta(&paths.meta_file);
    let owns_proxy_port = meta.get("proxy_enabled").map(String::as_str) == Some("true")
        && parse_optional_port(meta.get("proxy_port")) == Some(proxy_port);
    if !owns_proxy_port {
        return Ok(false);
    }
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|error| io::Error::other(format!("health client setup failed: {error}")))?;
    let was_running = is_serve_daemon_running(&paths, &client).await;
    let mut output = String::new();
    stop_router(&paths, &client, &mut output).await?;
    Ok(was_running)
}

/// Whether the serve pidfile points at a live `rld serve` supervisor (vs. a
/// stale pidfile, which `stop_router` cleans up but isn't a "was running").
async fn is_serve_daemon_running(paths: &RouterPaths, client: &reqwest::Client) -> bool {
    let Some(pid) = read_pid(&paths.pid_file) else {
        return false;
    };
    process_exists(pid)
        && is_rld_process(pid, &read_meta(&paths.meta_file), RldMode::Serve, client).await
}

pub async fn stop_from_home(home: &Path) -> io::Result<String> {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .build()
        .map_err(|error| io::Error::other(format!("health client setup failed: {error}")))?;
    let paths = RouterPaths::new(home);
    let mut output = String::new();
    stop_router(&paths, &client, &mut output).await?;
    stop_proxy(&paths, &client, &mut output, false).await?;
    // Also clean up the `--isolated` proxy so it does not orphan on its own port
    // in the `cc/` state dir. Only act (and print) when one is actually present,
    // so the common no-isolated case keeps its existing output.
    let isolated = RouterPaths::new_isolated(home);
    if read_pid(&isolated.proxy_pid_file).is_some() {
        output.push_str("--isolated:\n");
        stop_proxy(&isolated, &client, &mut output, false).await?;
    }
    Ok(output)
}

async fn start_from_home_with_client(
    home: &Path,
    request: &RouterStartRequest,
    bin_path: &Path,
    client: &reqwest::Client,
) -> io::Result<String> {
    let request = effective_start_request(home, request)?;
    let router_api_key = resolve_router_api_key(home, &request)?;
    let paths = RouterPaths::new(home);
    std::fs::create_dir_all(paths.data_dir())?;
    let mut output = String::new();

    // ---------- locked window: read-pid → decide → spawn → atomic meta write ----------
    // Hold the advisory lock across the entire check-then-spawn sequence so
    // concurrent launchers serialize and only one daemon is ever started.
    // The lock is released (by drop) before the long readiness wait below.
    let started = {
        let _lock = acquire_router_lock(&paths.lock_file)?;
        let requested_meta = router_meta(home, &request, None, router_api_key.as_deref());

        if let Some(existing) = read_pid(&paths.pid_file) {
            if process_exists(existing) {
                let meta = read_meta(&paths.meta_file);
                let health_port =
                    parse_optional_port(meta.get("injector_port")).unwrap_or(request.injector_port);
                let health = healthz(client, health_port).await;
                let is_rld = is_rld_process(existing, &meta, RldMode::Serve, client).await;
                let metadata_matches = metadata_matches_config(&meta, &requested_meta);
                if health
                    .as_ref()
                    .is_some_and(|health| router_health_matches_meta(health, &requested_meta))
                    && is_rld
                    && metadata_matches
                {
                    output.push_str(&format!(
                        "{} already running (pid {existing}, injector :{health_port}).\n",
                        daemon_name()
                    ));
                    return Ok(output); // lock released by drop
                }
                if !is_rld {
                    let requested_health = if health_port == request.injector_port {
                        health
                    } else {
                        healthz(client, request.injector_port).await
                    };
                    if requested_health.is_some() {
                        return Err(io::Error::other(format!(
                            "injector :{} is responding, but pid {} from {} is not this {} supervisor. Run `{} router stop` to clean up before starting.",
                            request.injector_port,
                            existing,
                            paths.pid_file.display(),
                            daemon_name(),
                            cli_name()
                        )));
                    }
                    output.push_str(&format!(
                        "{} pidfile points at {existing}, but that process is not this {} supervisor. Cleaning up.\n",
                        daemon_name(),
                        daemon_name()
                    ));
                    clear_router_state(&paths);
                } else if !metadata_matches || health.is_some() {
                    output.push_str(&format!(
                        "{} running with different config (pid {existing}); restarting.\n",
                        daemon_name()
                    ));
                    stop_router(&paths, client, &mut output).await?;
                } else {
                    return Err(io::Error::other(format!(
                        "pid {existing} from {} is alive but injector :{health_port} is not responding. Run `{} router stop` first.",
                        paths.pid_file.display(),
                        cli_name()
                    )));
                }
            } else {
                output.push_str(&format!(
                    "{} pidfile points at {existing} but the process is gone. Cleaning up.\n",
                    daemon_name()
                ));
                clear_router_state(&paths);
            }
        }

        // A combined local-router serve also binds the requested proxy port.
        // Cloud-only Rayline clients run a standalone proxy there, so take the port
        // over: stop that proxy first, otherwise the
        // serve's embedded proxy dies with "Address already in use" and tears down
        // llama-server. Only do this when the incumbent proxy's meta *explicitly*
        // records the same port we are about to bind. With a custom `RAYLINE_PROXY_PORT`
        // there is no conflict, and a missing/unparsable meta port is not proof of a
        // conflict, so leave an unrelated proxy (e.g. one on another port feeding
        // other sessions) alone rather than tearing it down on a guess. A genuine
        // same-port collision we miss this way still surfaces as a clear bind error.
        if request.enable_proxy && read_pid(&paths.proxy_pid_file).is_some() {
            let incumbent_port =
                parse_optional_port(read_meta(&paths.proxy_meta_file).get("proxy_port"));
            if incumbent_port == Some(request.proxy_port) {
                stop_proxy(&paths, client, &mut output, false).await?;
            }
        }

        spawn_router(home, &request, bin_path, router_api_key.as_deref())?
        // _lock dropped here → advisory lock released before long readiness wait
    };
    // ---------- end of locked window ----------

    output.push_str(&started.output);

    wait_for_router_ready(
        RouterReadyWait {
            paths: &paths,
            client,
            pid: started.pid,
            injector_port: request.injector_port,
            requested_meta: &started.meta,
            proxy_port: if request.enable_proxy {
                Some(request.proxy_port)
            } else {
                None
            },
            timeout: if started.cache_hit {
                Duration::from_secs(HEALTH_TIMEOUT_SECONDS)
            } else {
                Duration::from_secs(HEALTH_TIMEOUT_DOWNLOAD_SECONDS)
            },
        },
        &mut output,
    )
    .await?;
    let serve_metrics_port = parse_optional_port(started.meta.get("metrics_port"));
    reconcile_serve_metrics_meta(&paths, started.pid, serve_metrics_port, client, &mut output)
        .await;
    Ok(output)
}

/// Once serve is ready, drop its advertised `metrics_port` unless serve itself
/// answers there: the daemon binds metrics best-effort, and another process
/// can take the chosen port between the CLI's probe and the daemon's bind.
/// Without the entry a proxy self-hosts its metrics instead of forwarding them
/// to a port serve does not own (#82).
///
/// Ownership is the pid the metrics server reports, not merely an answer:
/// another metrics server on the port also answers. The meta is edited under
/// the router lock, and only while the pid file still names the serve this
/// launcher started, because the lock is released before readiness and
/// another launcher may have replaced serve meanwhile.
async fn reconcile_serve_metrics_meta(
    paths: &RouterPaths,
    serve_pid: i32,
    metrics_port: Option<u16>,
    client: &reqwest::Client,
    output: &mut String,
) {
    let Some(port) = metrics_port else {
        return;
    };
    if metrics_port_owner_pid(client, port).await == Some(serve_pid) {
        return;
    }
    let Ok(_lock) = acquire_router_lock(&paths.lock_file) else {
        return;
    };
    if read_pid(&paths.pid_file) != Some(serve_pid) {
        return;
    }
    let mut meta = read_meta(&paths.meta_file);
    if parse_optional_port(meta.get("metrics_port")) != Some(port) {
        return;
    }
    if meta.remove("metrics_port").is_some() {
        let _ = atomic_write(&paths.meta_file, format_meta(&meta).as_bytes());
        output.push_str(&format!(
            "warning: {} could not serve metrics on :{port}; `{} top` metrics are disabled for this session.\n",
            daemon_name(),
            cli_name(),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
async fn start_proxy_from_home_with_client(
    home: &Path,
    router_url: &str,
    router_api_key: &str,
    proxy_port: u16,
    proxy_routing_mode: &str,
    bin_path: &Path,
    force_restart: bool,
    diagnose: bool,
    upstream_ca_path: Option<&Path>,
    isolated: bool,
    router_config_path: Option<&Path>,
    local_config: Option<&ProxyLocalConfig>,
    client: &reqwest::Client,
) -> io::Result<String> {
    let paths = RouterPaths::for_isolation(home, isolated);
    std::fs::create_dir_all(paths.data_dir())?;
    let mut output = String::new();
    if force_restart {
        stop_proxy(&paths, client, &mut output, true).await?;
    }

    let metrics_url = serve_metrics_forward_url(home, client).await;
    let self_hosted_metrics_port = metrics_url
        .is_none()
        .then(|| resolve_metrics_port(isolated));
    let requested_meta = proxy_meta(
        home,
        router_url,
        router_api_key,
        proxy_port,
        proxy_routing_mode,
        None,
        upstream_ca_path,
        router_config_path,
        local_config,
        metrics_url.as_deref(),
        self_hosted_metrics_port,
    );
    // ---------- locked window: read-pid → decide → spawn → atomic meta write ----------
    // Hold the proxy-specific advisory lock across the check-then-spawn sequence
    // so concurrent `router proxy start` launchers serialize and only one proxy
    // daemon is ever started.  A SEPARATE lock file from the serve daemon means
    // a proxy launch never blocks on a serve launch.  Released (by drop) before
    // the long readiness wait below.
    let started = {
        let _lock = acquire_router_lock(&paths.proxy_lock_file)?;

        if let Some(existing) = read_pid(&paths.proxy_pid_file) {
            if process_exists(existing) {
                let meta = read_meta(&paths.proxy_meta_file);
                let health_port = parse_optional_port(meta.get("proxy_port")).unwrap_or(proxy_port);
                let health = healthz(client, health_port).await;
                let is_rld = is_rld_process(existing, &meta, RldMode::Proxy, client).await;
                let metadata_matches = metadata_matches_config(&meta, &requested_meta);
                if health
                    .as_ref()
                    .is_some_and(|health| proxy_health_matches_meta(health, &requested_meta))
                    && is_rld
                    && metadata_matches
                {
                    output.push_str(&format!(
                        "{} proxy already running (pid {existing}, proxy :{health_port}).\n",
                        daemon_name()
                    ));
                    return Ok(output); // lock released by drop
                }
                if !is_rld {
                    let requested_health = if health_port == proxy_port {
                        health
                    } else {
                        healthz(client, proxy_port).await
                    };
                    if requested_health.is_some() {
                        return Err(io::Error::other(format!(
                            "proxy :{proxy_port} is responding, but pid {existing} from {} is not this {} proxy. Run `{} router stop` to clean up before starting.",
                            paths.proxy_pid_file.display(),
                            daemon_name(),
                            cli_name()
                        )));
                    }
                    output.push_str(&format!(
                        "{} proxy pidfile points at {existing}, but that process is not this {} proxy. Cleaning up.\n",
                        daemon_name(),
                        daemon_name()
                    ));
                    clear_proxy_state(&paths);
                } else if !metadata_matches || health.is_some() {
                    output.push_str(&format!(
                        "{} proxy running with different config (pid {existing}); restarting.\n",
                        daemon_name()
                    ));
                    stop_proxy(&paths, client, &mut output, true).await?;
                } else {
                    return Err(io::Error::other(format!(
                        "pid {existing} from {} is alive but proxy :{health_port} is not responding. Run `{} router stop` first.",
                        paths.proxy_pid_file.display(),
                        cli_name()
                    )));
                }
            } else {
                output.push_str(&format!(
                    "{} proxy pidfile points at {existing} but the process is gone. Cleaning up.\n",
                    daemon_name()
                ));
                clear_proxy_state(&paths);
            }
        }

        if let Some(health) = healthz(client, proxy_port).await {
            if !proxy_health_matches_meta(&health, &requested_meta) {
                return Err(io::Error::other(format!(
                    "proxy :{proxy_port} is already responding, but it does not match the requested {} proxy config. Stop the process using that port or set RAYLINE_PROXY_PORT/RAYLINE_ISOLATED_PROXY_PORT to a free port.",
                    daemon_name()
                )));
            }
        }

        spawn_proxy(
            home,
            router_url,
            router_api_key,
            proxy_port,
            proxy_routing_mode,
            bin_path,
            diagnose,
            upstream_ca_path,
            isolated,
            router_config_path,
            local_config,
            metrics_url.as_deref(),
        )?
        // _lock dropped here → advisory lock released before long readiness wait
    };
    // ---------- end of locked window ----------

    output.push_str(&started.output);
    wait_for_proxy_ready(
        &paths,
        client,
        started.pid,
        proxy_port,
        &started.meta,
        &mut output,
    )
    .await?;
    reconcile_self_hosted_metrics_meta(&paths, self_hosted_metrics_port, client, &mut output).await;
    Ok(output)
}

struct StartedRouter {
    pid: i32,
    cache_hit: bool,
    meta: BTreeMap<String, String>,
    output: String,
}

struct StartedProxy {
    pid: i32,
    meta: BTreeMap<String, String>,
    output: String,
}

/// Record the metrics port serve is told to bind, or none at all: `router_meta`
/// may already carry a default entry, which must not stay advertised when serve
/// runs without metrics (#82).
fn advertise_serve_metrics_port(meta: &mut BTreeMap<String, String>, port: Option<u16>) {
    match port {
        Some(port) => {
            meta.insert("metrics_port".to_owned(), port.to_string());
        }
        None => {
            meta.remove("metrics_port");
        }
    }
}

fn spawn_router(
    home: &Path,
    request: &RouterStartRequest,
    bin_path: &Path,
    router_api_key: Option<&str>,
) -> io::Result<StartedRouter> {
    let paths = RouterPaths::new(home);
    std::fs::create_dir_all(paths.data_dir())?;
    let mut data_ports = vec![
        request.adapter_port,
        request.injector_port,
        request.local_router_port,
    ];
    if request.enable_proxy {
        data_ports.push(request.proxy_port);
    }
    let chosen_metrics_port = choose_serve_metrics_port(&data_ports);
    let mut requested_meta = router_meta(home, request, Some(bin_path), router_api_key);
    // Record the port this daemon is told to bind, so `rayline top` and a
    // forwarding proxy find it when it is not the default. With no usable
    // port, serve is given its preferred one, which is a data port; serve
    // refuses to bind metrics there and runs without them, and no port is
    // advertised.
    advertise_serve_metrics_port(&mut requested_meta, chosen_metrics_port);
    let metrics_port = chosen_metrics_port.unwrap_or_else(|| resolve_metrics_port(false));
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.log_file)?;
    let adapter_port = request.adapter_port.to_string();
    let injector_port = request.injector_port.to_string();
    let local_router_port = request.local_router_port.to_string();
    let metrics_port = metrics_port.to_string();
    let mut command = Command::new(bin_path);
    command.args(["serve"]);
    // Config-only: no bundled model at all (the static config routes only to named
    // endpoints). Custom mode: forward to the user's endpoint and skip the GGUF
    // args. Auto mode: the bundled GGUF path.
    if request.no_local_model {
        command.arg("--no-local-model");
    } else if let Some(upstream_url) = request.upstream_url.as_deref() {
        command.args(["--upstream-url", upstream_url]);
        if let Some(upstream_model) = request.upstream_model.as_deref() {
            command.args(["--upstream-model", upstream_model]);
        }
    } else {
        command.args([
            "--model-repo",
            &request.model_repo,
            "--model-file",
            &request.model_file,
        ]);
        if let Some(revision) = request.model_revision.as_deref() {
            command.args(["--model-revision", revision]);
        }
        if let Some(sha256) = request.model_sha256.as_deref() {
            command.args(["--model-sha256", sha256]);
        }
    }
    command.args([
        "--decision-plane",
        &request.decision_plane,
        "--local-router-port",
        &local_router_port,
        "--router-url",
        &request.router_url,
        "--local-model-id",
        &request.local_model_id,
        "--adapter-port",
        &adapter_port,
        "--injector-port",
        &injector_port,
        "--metrics-port",
        &metrics_port,
    ]);
    if let Some(path) = request.router_config_path.as_deref() {
        command.arg("--router-config-path").arg(path);
    }
    if request.may_local_advertise {
        command.arg("--may-local-advertise");
    }
    if let Some(url) = request.usage_callback_url.as_deref() {
        command.args(["--usage-callback-url", url]);
    }
    let ca_cert_path = proxy_ca_cert_path(home);
    let ca_key_path = proxy_ca_key_path(home);
    if request.enable_proxy {
        let proxy_port = request.proxy_port.to_string();
        let ca_cert_path = ca_cert_path.display().to_string();
        let ca_key_path = ca_key_path.display().to_string();
        command.args([
            "--proxy-port",
            &proxy_port,
            "--ca-cert-path",
            &ca_cert_path,
            "--ca-key-path",
            &ca_key_path,
            "--proxy-routing-mode",
            &request.proxy_routing_mode,
        ]);
        set_proxy_child_env(
            &mut command,
            router_api_key.unwrap_or_default(),
            request.proxy_port,
        );
    } else if let Some(key) = router_api_key.filter(|key| !key.is_empty()) {
        // Proxy disabled (e.g. the isolated local-plane path uses a separate proxy
        // sidecar), but the local router daemon itself still needs
        // RAYLINE_ROUTER_API_KEY when a config route targets the hosted
        // `rayline-cloud` endpoint (e.g. Rc-K/Rl-K). Export it directly rather than
        // via `set_proxy_child_env`, which also scrubs proxy env not relevant here.
        command.env("RAYLINE_ROUTER_API_KEY", key);
    }
    command.env("RUST_LOG", "info");
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file));

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn()?;
    let pid = child.id() as i32;
    write_pid_meta_atomic(&paths.pid_file, &paths.meta_file, pid, &requested_meta)?;

    let mut output = format!(
        "{} starting (pid {pid}, log {}).",
        daemon_name(),
        paths.log_file.display()
    );
    // Custom mode and config-only mode download no GGUF, so there is never a
    // first-run download wait to warn about — treat them like a warm cache.
    let cache_hit = request.no_local_model
        || request.upstream_url.is_some()
        || hf_cache_has_verified_gguf(
            home,
            &request.model_repo,
            &request.model_file,
            request.model_revision.as_deref(),
            request.model_sha256.as_deref(),
        );
    if cache_hit {
        output.push_str(" Waiting for healthz\u{2026}\n");
    } else {
        output.push('\n');
        output.push_str(&format!(
            "First-time setup: downloading {} from {}. This can take 10+ minutes on a slow connection \u{2014} progress below. If you Ctrl-C, the download continues in the background; re-run the same command to resume.\n",
            request.model_file, request.model_repo
        ));
    }

    Ok(StartedRouter {
        pid,
        cache_hit,
        meta: requested_meta,
        output,
    })
}

#[allow(clippy::too_many_arguments)]
fn spawn_proxy(
    home: &Path,
    router_url: &str,
    router_api_key: &str,
    proxy_port: u16,
    proxy_routing_mode: &str,
    bin_path: &Path,
    diagnose: bool,
    upstream_ca_path: Option<&Path>,
    isolated: bool,
    router_config_path: Option<&Path>,
    local_config: Option<&ProxyLocalConfig>,
    metrics_url: Option<&str>,
) -> io::Result<StartedProxy> {
    let paths = RouterPaths::for_isolation(home, isolated);
    std::fs::create_dir_all(paths.data_dir())?;
    let metrics_port = resolve_metrics_port(isolated);
    let self_hosted_metrics_port = metrics_url.is_none().then_some(metrics_port);
    let requested_meta = proxy_meta(
        home,
        router_url,
        router_api_key,
        proxy_port,
        proxy_routing_mode,
        Some(bin_path),
        upstream_ca_path,
        router_config_path,
        local_config,
        metrics_url,
        self_hosted_metrics_port,
    );
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&paths.proxy_log_file)?;
    let proxy_port_arg = proxy_port.to_string();
    let metrics_port_arg = metrics_port.to_string();
    let ca_cert_path = proxy_ca_cert_path(home);
    let ca_key_path = proxy_ca_key_path(home);
    let ca_cert_arg = ca_cert_path.display().to_string();
    let ca_key_arg = ca_key_path.display().to_string();
    let mut command = Command::new(bin_path);
    command.args([
        "proxy",
        "--proxy-port",
        &proxy_port_arg,
        "--metrics-port",
        &metrics_port_arg,
        "--router-url",
        router_url,
        "--ca-cert-path",
        &ca_cert_arg,
        "--ca-key-path",
        &ca_key_arg,
        "--proxy-routing-mode",
        proxy_routing_mode,
    ]);
    if let Some(upstream_ca_path) = upstream_ca_path {
        command
            .arg("--upstream-ca-path")
            .arg(upstream_ca_path.as_os_str());
    }
    if let Some(router_config_path) = router_config_path {
        command
            .arg("--router-config-path")
            .arg(router_config_path.as_os_str());
    }
    if let Some(local_config) = local_config {
        let adapter_port = local_config.adapter_port.to_string();
        command.args([
            "--local-available",
            "--local-model-id",
            &local_config.local_model_id,
            "--local-adapter-port",
            &adapter_port,
        ]);
        if local_config.custom_mode {
            command.arg("--local-custom");
        }
        command.arg("--local-router-owns-metrics");
    }
    if let Some(metrics_url) = metrics_url {
        command.args(["--metrics-url", metrics_url]);
    }
    set_proxy_child_env(&mut command, router_api_key, proxy_port);
    command.env("RUST_LOG", if diagnose { "debug" } else { "info" });
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file.try_clone()?))
        .stderr(Stdio::from(log_file));

    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = command.spawn()?;
    let pid = child.id() as i32;
    write_pid_meta_atomic(
        &paths.proxy_pid_file,
        &paths.proxy_meta_file,
        pid,
        &requested_meta,
    )?;
    Ok(StartedProxy {
        pid,
        meta: requested_meta,
        output: format!(
            "{} proxy starting (pid {pid}, log {}). Waiting for healthz\u{2026}\n",
            daemon_name(),
            paths.proxy_log_file.display()
        ),
    })
}

struct RouterReadyWait<'a> {
    paths: &'a RouterPaths,
    client: &'a reqwest::Client,
    pid: i32,
    injector_port: u16,
    requested_meta: &'a BTreeMap<String, String>,
    proxy_port: Option<u16>,
    timeout: Duration,
}

async fn wait_for_router_ready(state: RouterReadyWait<'_>, output: &mut String) -> io::Result<()> {
    let deadline = std::time::Instant::now() + state.timeout;
    while std::time::Instant::now() < deadline {
        if !process_exists(state.pid) {
            return Err(io::Error::other(format!(
                "{} exited early. Last log lines:\n{}",
                daemon_name(),
                tail_log(&state.paths.log_file, 40)
            )));
        }
        let injector_ready = healthz(state.client, state.injector_port)
            .await
            .as_ref()
            .is_some_and(|health| router_health_matches_meta(health, state.requested_meta));
        if injector_ready {
            if let Some(proxy_port) = state.proxy_port {
                if !healthz(state.client, proxy_port)
                    .await
                    .as_ref()
                    .is_some_and(|health| proxy_health_matches_meta(health, state.requested_meta))
                {
                    tokio::time::sleep(HEALTH_POLL).await;
                    continue;
                }
                output.push_str(&format!(
                    "{} ready \u{2014} set HTTPS_PROXY=http://127.0.0.1:{proxy_port}\n",
                    daemon_name()
                ));
                return Ok(());
            }
            output.push_str(&format!(
                "{} ready \u{2014} point Claude Code at ANTHROPIC_BASE_URL=http://127.0.0.1:{}\n",
                daemon_name(),
                state.injector_port
            ));
            return Ok(());
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }

    let _ = terminate_process(state.pid);
    clear_router_state(state.paths);
    Err(io::Error::other(format!(
        "{} did not become healthy in {}s. See {}.",
        daemon_name(),
        state.timeout.as_secs(),
        state.paths.log_file.display()
    )))
}

async fn wait_for_proxy_ready(
    paths: &RouterPaths,
    client: &reqwest::Client,
    pid: i32,
    proxy_port: u16,
    requested_meta: &BTreeMap<String, String>,
    output: &mut String,
) -> io::Result<()> {
    let deadline = std::time::Instant::now() + Duration::from_secs(HEALTH_TIMEOUT_SECONDS);
    while std::time::Instant::now() < deadline {
        if !process_exists(pid) {
            return Err(io::Error::other(format!(
                "{} proxy exited early. Last log lines:\n{}",
                daemon_name(),
                tail_log(&paths.proxy_log_file, 40)
            )));
        }
        if healthz(client, proxy_port)
            .await
            .as_ref()
            .is_some_and(|health| proxy_health_matches_meta(health, requested_meta))
        {
            output.push_str(&format!(
                "{} proxy ready \u{2014} set HTTPS_PROXY=http://127.0.0.1:{proxy_port}\n",
                daemon_name()
            ));
            return Ok(());
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }

    let _ = terminate_process(pid);
    clear_proxy_state(paths);
    Err(io::Error::other(format!(
        "{} proxy did not become healthy in {}s. See {}.",
        daemon_name(),
        HEALTH_TIMEOUT_SECONDS,
        paths.proxy_log_file.display()
    )))
}

fn router_meta(
    home: &Path,
    request: &RouterStartRequest,
    bin_path: Option<&Path>,
    router_api_key: Option<&str>,
) -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    meta.insert("model_repo".to_owned(), request.model_repo.clone());
    meta.insert("model_file".to_owned(), request.model_file.clone());
    meta.insert(
        "model_revision".to_owned(),
        request.model_revision.clone().unwrap_or_default(),
    );
    meta.insert(
        "model_sha256".to_owned(),
        request.model_sha256.clone().unwrap_or_default(),
    );
    // Record the custom upstream so switching endpoints (or auto↔custom) is seen
    // as a config change and restarts the daemon. Empty in auto mode.
    meta.insert(
        "upstream_url".to_owned(),
        request.upstream_url.clone().unwrap_or_default(),
    );
    // Also record the upstream model so changing only `--upstream-model` (same
    // url + local-model-id) is seen as a config change and restarts the daemon —
    // otherwise the adapter keeps rewriting requests to the previous model.
    meta.insert(
        "upstream_model".to_owned(),
        request.upstream_model.clone().unwrap_or_default(),
    );
    meta.insert("router_url".to_owned(), request.router_url.clone());
    meta.insert("decision_plane".to_owned(), request.decision_plane.clone());
    meta.insert(
        "local_router_port".to_owned(),
        request.local_router_port.to_string(),
    );
    meta.insert(
        "router_config_path".to_owned(),
        request
            .router_config_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    );
    if let Some(fingerprint) = router_config_fingerprint(request.router_config_path.as_deref()) {
        meta.insert("router_config_sha256".to_owned(), fingerprint);
    }
    meta.insert("local_model_id".to_owned(), request.local_model_id.clone());
    meta.insert("adapter_port".to_owned(), request.adapter_port.to_string());
    meta.insert(
        "injector_port".to_owned(),
        request.injector_port.to_string(),
    );
    meta.insert(
        "metrics_port".to_owned(),
        rayline_metrics::DEFAULT_METRICS_PORT.to_string(),
    );
    if let Some(bin_path) = bin_path {
        meta.insert("bin_path".to_owned(), bin_path.display().to_string());
    }
    // Track the router key in BOTH proxy and non-proxy modes. The non-proxy
    // local-plane path (isolated mixed configs like Rc-K/Rl-K) exports
    // RAYLINE_ROUTER_API_KEY to the daemon for a `rayline-cloud` endpoint, so a
    // key acquired (first login) or rotated after the daemon is already running
    // counts as a config change and restarts the daemon — otherwise the reuse
    // check keeps a stale/missing key and 502s cloud routes. This mirrors
    // `proxy_meta`'s conditional recording (both share the "removed key isn't
    // detected" limitation of `metadata_matches_config`, which is pre-existing
    // and out of scope here).
    if let Some(fingerprint) = router_key_fingerprint(router_api_key) {
        meta.insert("router_key_sha256".to_owned(), fingerprint);
    }
    if request.enable_proxy {
        meta.insert("proxy_enabled".to_owned(), "true".to_owned());
        meta.insert("proxy_port".to_owned(), request.proxy_port.to_string());
        meta.insert(
            "proxy_routing_mode".to_owned(),
            request.proxy_routing_mode.clone(),
        );
        meta.insert(
            "ca_cert_path".to_owned(),
            proxy_ca_cert_path(home).display().to_string(),
        );
        meta.insert(
            "ca_key_path".to_owned(),
            proxy_ca_key_path(home).display().to_string(),
        );
        meta.insert("upstream_ca_path".to_owned(), String::new());
    }
    meta
}

/// The serve daemon's metrics-control URL the proxy should forward to, or
/// `None` when the proxy should self-host its own metrics instead.
///
/// Forwarding is chosen only when a serve daemon is actually live. Stale serve
/// meta left behind by a crashed daemon must not pin a cloud-only proxy to a
/// dead endpoint — that would leave `rayline top` with no metrics. The empty
/// fast-path skips the liveness probe entirely for the common cloud-only launch
/// where no serve daemon has ever published meta.
async fn serve_metrics_forward_url(home: &Path, client: &reqwest::Client) -> Option<String> {
    let paths = RouterPaths::new(home);
    let serve_meta = read_meta(&paths.meta_file);
    if serve_meta.is_empty() {
        return None;
    }
    let serve_running = is_serve_daemon_running(&paths, client).await;
    let url = serve_metrics_url(serve_running, &serve_meta)?;
    // The meta can still name a port serve lost to another process, until
    // serve's launcher reconciles it (#82): forward only to a port serve owns.
    let port = parse_optional_port(serve_meta.get("metrics_port"))?;
    if !metrics_port_owned_by_serve(&paths, client, port).await {
        return None;
    }
    Some(url)
}

/// Whether the metrics server on `port` is the serve daemon the pid file
/// records, as its `/healthz` pid says.
async fn metrics_port_owned_by_serve(
    paths: &RouterPaths,
    client: &reqwest::Client,
    port: u16,
) -> bool {
    match read_pid(&paths.pid_file) {
        Some(serve_pid) => metrics_port_owner_pid(client, port).await == Some(serve_pid),
        None => false,
    }
}

/// Pure decision: the serve metrics-control URL when a serve daemon is live and
/// has published meta, else `None` so the proxy self-hosts.
fn serve_metrics_url(serve_running: bool, serve_meta: &BTreeMap<String, String>) -> Option<String> {
    if !serve_running || serve_meta.is_empty() {
        return None;
    }
    // No advertised port means serve runs without metrics (its bind failed), so
    // the proxy self-hosts rather than forwarding to a port serve does not own.
    let port = parse_optional_port(serve_meta.get("metrics_port"))?;
    Some(format!("http://127.0.0.1:{port}"))
}

/// Once the proxy is healthy, make the advertised `metrics_port` reflect what the
/// daemon actually bound. The daemon self-hosts metrics best-effort: on a port
/// collision it runs without metrics rather than failing the proxy. Leaving
/// `metrics_port` in the proxy meta then points `rayline top` (and future
/// launches' port discovery) at an endpoint the proxy never owned, so drop it
/// when the self-hosted server is not answering. Metrics stays best-effort —
/// this only edits meta and never restarts the proxy.
async fn reconcile_self_hosted_metrics_meta(
    paths: &RouterPaths,
    self_hosted_metrics_port: Option<u16>,
    client: &reqwest::Client,
    output: &mut String,
) {
    let Some(port) = self_hosted_metrics_port else {
        return;
    };
    if metrics_port_is_serving(client, port).await {
        return;
    }
    let mut meta = read_meta(&paths.proxy_meta_file);
    if meta.remove("metrics_port").is_some() {
        let _ = std::fs::write(&paths.proxy_meta_file, format_meta(&meta));
        output.push_str(&format!(
            "warning: {} proxy could not self-host metrics on :{port}; `{} top` metrics are disabled for this session.\n",
            daemon_name(),
            cli_name(),
        ));
    }
}

#[allow(clippy::too_many_arguments)]
fn proxy_meta(
    home: &Path,
    router_url: &str,
    router_api_key: &str,
    proxy_port: u16,
    proxy_routing_mode: &str,
    bin_path: Option<&Path>,
    upstream_ca_path: Option<&Path>,
    router_config_path: Option<&Path>,
    local_config: Option<&ProxyLocalConfig>,
    metrics_url: Option<&str>,
    self_hosted_metrics_port: Option<u16>,
) -> BTreeMap<String, String> {
    let mut meta = BTreeMap::new();
    meta.insert("router_url".to_owned(), router_url.to_owned());
    meta.insert("proxy_port".to_owned(), proxy_port.to_string());
    meta.insert(
        "proxy_routing_mode".to_owned(),
        proxy_routing_mode.to_owned(),
    );
    if let Some(fingerprint) = router_key_fingerprint(Some(router_api_key)) {
        meta.insert("router_key_sha256".to_owned(), fingerprint);
    }
    if let Some(local_config) = local_config {
        meta.insert("decision_plane".to_owned(), DECISION_PLANE_LOCAL.to_owned());
        meta.insert("local_available".to_owned(), "true".to_owned());
        meta.insert(
            "local_model_id".to_owned(),
            local_config.local_model_id.clone(),
        );
        meta.insert(
            "local_adapter_port".to_owned(),
            local_config.adapter_port.to_string(),
        );
        meta.insert(
            "local_custom".to_owned(),
            local_config.custom_mode.to_string(),
        );
    }
    if let Some(metrics_url) = metrics_url {
        meta.insert("metrics_url".to_owned(), metrics_url.to_owned());
    }
    // Only recorded when the proxy self-hosts metrics (i.e. it is not forwarding
    // to a serve daemon), so `rayline top` only ever discovers a port the proxy
    // actually owns.
    if let Some(metrics_port) = self_hosted_metrics_port {
        meta.insert("metrics_port".to_owned(), metrics_port.to_string());
    }
    meta.insert(
        "ca_cert_path".to_owned(),
        proxy_ca_cert_path(home).display().to_string(),
    );
    meta.insert(
        "ca_key_path".to_owned(),
        proxy_ca_key_path(home).display().to_string(),
    );
    meta.insert(
        "upstream_ca_path".to_owned(),
        upstream_ca_path
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    );
    meta.insert(
        "router_config_path".to_owned(),
        router_config_path
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
    );
    if let Some(fingerprint) = router_config_fingerprint(router_config_path) {
        meta.insert("router_config_sha256".to_owned(), fingerprint);
    }
    if let Some(bin_path) = bin_path {
        meta.insert("bin_path".to_owned(), bin_path.display().to_string());
    }
    meta
}

fn router_config_fingerprint(router_config_path: Option<&Path>) -> Option<String> {
    let bytes = fs::read(router_config_path?).ok()?;
    Some(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn metadata_matches_config(
    meta: &BTreeMap<String, String>,
    expected: &BTreeMap<String, String>,
) -> bool {
    expected.iter().all(|(key, expected_value)| {
        // `bin_path` may legitimately differ (PATH vs absolute launcher).
        // `metrics_port` is a best-effort runtime fact reconciled after the
        // proxy is ready (see reconcile_self_hosted_metrics_meta), not a config
        // input — a difference must never churn-restart an otherwise healthy
        // proxy just because metrics could or could not self-host.
        if key == "bin_path" || key == "metrics_port" {
            return true;
        }
        let Some(actual) = meta.get(key) else {
            return false;
        };
        if key == "router_url" {
            actual.trim_end_matches('/') == expected_value.trim_end_matches('/')
        } else {
            actual == expected_value
        }
    })
}

fn format_meta(meta: &BTreeMap<String, String>) -> String {
    let mut output = String::new();
    for key in [
        "model_repo",
        "model_file",
        "model_revision",
        "model_sha256",
        "upstream_url",
        "upstream_model",
        "router_url",
        "decision_plane",
        "local_available",
        "local_router_port",
        "router_config_path",
        "router_config_sha256",
        "local_model_id",
        "local_adapter_port",
        "local_custom",
        "adapter_port",
        "injector_port",
        "metrics_port",
        "metrics_url",
        "bin_path",
        "proxy_enabled",
        "proxy_port",
        "proxy_routing_mode",
        "router_key_sha256",
        "ca_cert_path",
        "ca_key_path",
        "upstream_ca_path",
    ] {
        if let Some(value) = meta.get(key) {
            output.push_str(&format!("{key}={value}\n"));
        }
    }
    output
}

fn effective_start_request(
    home: &Path,
    request: &RouterStartRequest,
) -> io::Result<RouterStartRequest> {
    let mut effective = request.clone();
    if effective.decision_plane == DECISION_PLANE_LOCAL {
        effective.router_url = format!("http://127.0.0.1:{}", effective.local_router_port);
        effective.router_url_explicit = true;
        return Ok(effective);
    }
    if effective.enable_proxy && !effective.router_url_explicit {
        let env_name = crate::status::resolve_env(effective.env_name.as_deref(), Some(home));
        let hosted = crate::status::resolve_hosted_environment(&env_name, Some(home))
            .map_err(|error| io::Error::other(error.to_string()))?;
        effective.router_url = hosted.router_url;
    }
    Ok(effective)
}

fn resolve_router_api_key(home: &Path, request: &RouterStartRequest) -> io::Result<Option<String>> {
    // An explicit override (e.g. the `rayline auth login` session key injected
    // for a config whose `rayline-cloud` endpoint reads `RAYLINE_ROUTER_API_KEY`)
    // is honored regardless of `enable_proxy`. The isolated local-plane path
    // starts the router with `enable_proxy = false`, so gating on it here would
    // drop the key and 502 the cloud leg with `requires $RAYLINE_ROUTER_API_KEY`
    // for mixed configs (e.g. Rc-K: rayline-cloud main + anthropic subagent).
    if let Some(override_key) = request.router_api_key_override.as_deref() {
        return Ok(Some(override_key.to_owned()));
    }
    if request.decision_plane == DECISION_PLANE_LOCAL {
        // The local router on :20811 needs no auth, but a config endpoint may read
        // RAYLINE_ROUTER_API_KEY (e.g. a hosted cloud-router route). Preserve a
        // user-set value rather than clobbering it with empty. Checked before the
        // `!enable_proxy` short-circuit below so a proxy-less Codex start
        // (`enable_proxy = false`, including `router start --mode codex`) still
        // forwards an explicit env var to the daemon and its reuse metadata.
        return Ok(Some(
            std::env::var("RAYLINE_ROUTER_API_KEY").unwrap_or_default(),
        ));
    }
    if !request.enable_proxy {
        return Ok(None);
    }
    let env_name = crate::status::resolve_env(request.env_name.as_deref(), Some(home));
    crate::status::resolve_hosted_environment(&env_name, Some(home))
        .map_err(|error| io::Error::other(error.to_string()))?;
    crate::status::load_claude_key_from_home(&env_name, home)
        .map(Some)
        .ok_or_else(|| {
            io::Error::other(format!(
                "No {} router key stored for {env_name}. Run: {} auth login",
                crate::DISPLAY_NAME,
                cli_name()
            ))
        })
}

fn router_key_fingerprint(router_api_key: Option<&str>) -> Option<String> {
    let router_api_key = router_api_key.filter(|key| !key.is_empty())?;
    Some(format!(
        "sha256:{:x}",
        Sha256::digest(router_api_key.as_bytes())
    ))
}

fn proxy_ca_cert_path(home: &Path) -> PathBuf {
    proxy_ca_dir(home).join("proxy-ca.pem")
}

pub fn default_proxy_ca_cert_path(home: &Path) -> PathBuf {
    proxy_ca_cert_path(home)
}

fn proxy_ca_key_path(home: &Path) -> PathBuf {
    proxy_ca_dir(home).join("proxy-ca-key.pem")
}

fn proxy_ca_dir(home: &Path) -> PathBuf {
    platform_config_dir(home).join(crate::CONFIG_DIR)
}

fn platform_config_dir(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library").join("Application Support")
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("AppData").join("Roaming"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"))
    }
}

fn set_proxy_child_env(command: &mut Command, router_api_key: &str, proxy_port: u16) {
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        if std::env::var(name).is_ok_and(|value| is_self_proxy_url(&value, proxy_port)) {
            command.env_remove(name);
        }
    }
    // The forward-vs-self-host decision is carried solely by the `--metrics-url`
    // flag, passed only when forwarding to a serve daemon. Clear any inherited
    // RAYLINE_METRICS_URL so a value in the launcher's environment cannot
    // override that decision via clap's env fallback and force a self-hosting
    // proxy to forward to a stale endpoint instead of binding its own server.
    command.env_remove("RAYLINE_METRICS_URL");
    command.env("RAYLINE_ROUTER_API_KEY", router_api_key);
}

fn is_self_proxy_url(value: &str, proxy_port: u16) -> bool {
    let raw = value.trim();
    if raw.is_empty() {
        return false;
    }
    let candidate = if raw.contains("://") {
        raw.to_owned()
    } else {
        format!("http://{raw}")
    };
    let Ok(parsed) = reqwest::Url::parse(&candidate) else {
        return false;
    };
    matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1"))
        && parsed.port() == Some(proxy_port)
}

pub(crate) fn resolve_rld_bin(home: &Path) -> io::Result<PathBuf> {
    let env_var = daemon_bin_env_var();
    if let Some(explicit) = std::env::var_os(env_var) {
        let path = PathBuf::from(explicit);
        if path.is_file() {
            return Ok(path);
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("${env_var} points to a missing file: {}", path.display()),
        ));
    }
    let bundled = home
        .join(crate::DOT_CONFIG_DIR)
        .join("bin")
        .join(daemon_name());
    if bundled.is_file() {
        return Ok(bundled);
    }
    if let Some(found) = find_on_path(daemon_name()) {
        return Ok(found);
    }
    let dev_hint = format!(
        "`cargo build --release -p rayline-daemon` and place target/release/{} on PATH or set ${env_var}",
        daemon_name()
    );
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "{} binary not found. Reinstall with `curl -fsSL {} | bash` (drops it at {}), or for dev: {dev_hint}.",
            daemon_name(),
            crate::INSTALLER_URL,
            bundled.display(),
        ),
    ))
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary_name);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe_candidate = dir.join(format!("{binary_name}.exe"));
            if exe_candidate.is_file() {
                return Some(exe_candidate);
            }
        }
    }
    None
}

pub(crate) fn hf_cache_has_verified_config_gguf(
    home: &Path,
    cfg: &crate::local_model::LocalModelConfig,
) -> bool {
    hf_cache_has_verified_gguf(
        home,
        cfg.model_repo.as_deref().unwrap_or_default(),
        cfg.model_file.as_deref().unwrap_or_default(),
        cfg.model_revision.as_deref(),
        cfg.model_sha256.as_deref(),
    )
}

fn hf_cache_has_verified_gguf(
    home: &Path,
    model_repo: &str,
    model_file: &str,
    model_revision: Option<&str>,
    model_sha256: Option<&str>,
) -> bool {
    hf_cache_has_verified_gguf_at(
        &hf_hub_dir(home),
        model_repo,
        model_file,
        model_revision,
        model_sha256,
    )
}

fn hf_cache_has_verified_gguf_at(
    hub_dir: &Path,
    model_repo: &str,
    model_file: &str,
    model_revision: Option<&str>,
    model_sha256: Option<&str>,
) -> bool {
    let (Some(revision), Some(sha256)) = (model_revision, model_sha256) else {
        return false;
    };
    let snapshot = hf_cache_verified_snapshot_path(hub_dir, model_repo, model_file, revision);
    hf_cache_snapshot_is_verified(&snapshot, sha256)
}

fn hf_cache_snapshot_is_verified(snapshot: &Path, sha256: &str) -> bool {
    snapshot.is_file() && rayline_hf::verify_file_sha256(snapshot, sha256).is_ok()
}

fn hf_cache_verified_snapshot_path(
    hub_dir: &Path,
    model_repo: &str,
    model_file: &str,
    revision: &str,
) -> PathBuf {
    hub_dir
        .join(rayline_hf::repo_to_folder_name(model_repo))
        .join("snapshots")
        .join(revision)
        .join(model_file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_metrics_port_keeps_the_preferred_port_when_free() {
        let port = pick_metrics_port(20813, true, &[], || panic!("no ephemeral port is needed"));
        assert_eq!(port, Some(20813));
    }

    #[test]
    fn serve_metrics_port_moves_off_a_held_port() {
        // #82: a running `rayline claude` proxy holds the default port.
        let held = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("bind a test port");
        let held_port = held.local_addr().expect("local addr").port();
        assert!(!local_port_is_free(held_port));
        let port = pick_metrics_port(
            held_port,
            local_port_is_free(held_port),
            &[],
            ephemeral_local_port,
        );
        let port = port.expect("an ephemeral port");
        assert_ne!(port, held_port);
        assert!(local_port_is_free(port), "the chosen port must be bindable");
    }

    #[test]
    fn serve_meta_advertises_no_metrics_port_when_none_is_usable() {
        let mut meta = BTreeMap::new();
        meta.insert("metrics_port".to_owned(), "20813".to_owned());
        advertise_serve_metrics_port(&mut meta, None);
        assert!(!meta.contains_key("metrics_port"));

        advertise_serve_metrics_port(&mut meta, Some(50123));
        assert_eq!(meta.get("metrics_port").map(String::as_str), Some("50123"));
    }

    #[test]
    fn serve_metrics_port_is_none_rather_than_a_data_port() {
        // #82: the preferred port is a data port and every ephemeral offer is
        // one too; returning the preferred port would let metrics take it.
        let port = pick_metrics_port(20811, true, &[20811, 20812], || Some(20812));
        assert_eq!(port, None);
        assert_eq!(pick_metrics_port(20811, false, &[20811], || None), None);
    }

    #[test]
    fn serve_metrics_port_falls_back_to_preferred_without_an_ephemeral_port() {
        assert_eq!(pick_metrics_port(20813, false, &[], || None), Some(20813));
    }

    #[test]
    fn serve_metrics_port_never_takes_a_data_port() {
        // #82: with RAYLINE_METRICS_PORT set to the local-router port, serve
        // would bind metrics there first and then fail to bind its router.
        let mut offers = vec![20811, 50123].into_iter();
        let port = pick_metrics_port(20811, true, &[20811, 20812], || offers.next());
        assert_eq!(
            port,
            Some(50123),
            "an ephemeral port equal to a data port is skipped"
        );
    }
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[tokio::test]
    async fn first_reachable_metrics_port_skips_unreachable_and_picks_live_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // A port that is bound then released: probing it gets connection-refused.
        let released = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = released.local_addr().unwrap().port();
        drop(released);

        // A live responder that answers the snapshot probe with 200.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = b"{\"ok\":true}";
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.flush().await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();

        let port = first_reachable_metrics_port(&client, &[closed_port, live_port]).await;
        assert_eq!(port, live_port);
    }

    #[tokio::test]
    async fn first_reachable_metrics_port_falls_back_to_first_when_none_reachable() {
        let released = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = released.local_addr().unwrap().port();
        drop(released);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();

        let port = first_reachable_metrics_port(&client, &[closed_port]).await;
        assert_eq!(port, closed_port);
    }

    #[test]
    fn resolve_metrics_port_defaults_by_isolation() {
        // Env-driven overrides are exercised elsewhere; these assert the
        // default ports the proxy self-hosts on per isolation state.
        unsafe {
            std::env::remove_var("RAYLINE_METRICS_PORT");
            std::env::remove_var("RAYLINE_ISOLATED_METRICS_PORT");
        }
        assert_eq!(
            resolve_metrics_port(false),
            rayline_metrics::DEFAULT_METRICS_PORT
        );
        assert_eq!(resolve_metrics_port(true), DEFAULT_ISOLATED_METRICS_PORT);
        assert_ne!(
            DEFAULT_ISOLATED_METRICS_PORT,
            rayline_metrics::DEFAULT_METRICS_PORT
        );
    }

    #[test]
    fn proxy_meta_records_metrics_port_only_when_self_hosting() {
        let home = Path::new("/tmp/rayline-test-home");

        let self_hosted = proxy_meta(
            home,
            "https://r",
            "key",
            20812,
            "selective-subagents",
            None,
            None,
            None,
            None,
            None,
            Some(20814),
        );
        assert_eq!(
            self_hosted.get("metrics_port").map(String::as_str),
            Some("20814")
        );

        let forwarding = proxy_meta(
            home,
            "https://r",
            "key",
            20810,
            "selective-subagents",
            None,
            None,
            None,
            None,
            Some("http://127.0.0.1:20813"),
            None,
        );
        assert_eq!(forwarding.get("metrics_port"), None);
    }

    fn meta_with_metrics_port(port: u16) -> BTreeMap<String, String> {
        let mut meta = BTreeMap::new();
        meta.insert("metrics_port".to_owned(), port.to_string());
        meta
    }

    #[test]
    fn metrics_port_candidates_order_serve_then_proxy_then_isolated_then_default() {
        let serve = meta_with_metrics_port(20900);
        let proxy = meta_with_metrics_port(20901);
        let isolated = meta_with_metrics_port(20902);

        let ports = metrics_port_candidates(&serve, &proxy, &isolated);

        assert_eq!(
            ports,
            vec![20900, 20901, 20902, rayline_metrics::DEFAULT_METRICS_PORT]
        );
    }

    #[test]
    fn metrics_port_candidates_fall_back_to_default_when_all_meta_empty() {
        let empty = BTreeMap::new();

        let ports = metrics_port_candidates(&empty, &empty, &empty);

        assert_eq!(ports, vec![rayline_metrics::DEFAULT_METRICS_PORT]);
    }

    #[test]
    fn serve_metrics_url_none_when_serve_not_running() {
        // Regression: a crashed serve daemon can leave stale meta behind. The
        // proxy must not forward metrics to that dead endpoint — it should
        // self-host instead so `rayline top` still works.
        let stale = meta_with_metrics_port(20813);

        assert_eq!(serve_metrics_url(false, &stale), None);
    }

    #[test]
    fn serve_metrics_url_forwards_when_serve_running() {
        let serve = meta_with_metrics_port(20990);

        assert_eq!(
            serve_metrics_url(true, &serve),
            Some("http://127.0.0.1:20990".to_owned())
        );
    }

    #[test]
    fn serve_metrics_url_none_when_meta_empty() {
        let empty = BTreeMap::new();

        assert_eq!(serve_metrics_url(true, &empty), None);
    }

    #[test]
    fn serve_metrics_url_none_when_running_without_port() {
        // Serve meta always records the port it was told to bind; the entry is
        // removed only when nothing answered there (#82). The proxy then
        // self-hosts instead of forwarding to a port another rld may own.
        let mut serve = BTreeMap::new();
        serve.insert("router_url".to_owned(), "https://api.rayline.ai".to_owned());

        assert_eq!(serve_metrics_url(true, &serve), None);
    }

    #[test]
    fn resolve_metrics_port_reads_zero_as_unset() {
        // Parsed without touching the process environment, which other tests
        // read concurrently.
        let default = rayline_metrics::DEFAULT_METRICS_PORT;
        assert_eq!(metrics_port_from_env(Some("0"), default), default);
        assert_eq!(metrics_port_from_env(Some(""), default), default);
        assert_eq!(metrics_port_from_env(Some("nope"), default), default);
        assert_eq!(metrics_port_from_env(None, default), default);
        assert_eq!(metrics_port_from_env(Some("20900"), default), 20900);
    }

    #[test]
    fn proxy_child_env_drops_inherited_metrics_url() {
        use std::ffi::OsStr;

        // The proxy's forward-vs-self-host decision is carried solely by the
        // `--metrics-url` flag (present only when forwarding). An inherited
        // RAYLINE_METRICS_URL in the launcher env must not override it, or a
        // self-hosting proxy would forward to a stale endpoint and never bind.
        let mut command = Command::new("true");
        set_proxy_child_env(&mut command, "router-key", 20810);

        let removed = command
            .get_envs()
            .any(|(key, value)| key == OsStr::new("RAYLINE_METRICS_URL") && value.is_none());

        assert!(
            removed,
            "RAYLINE_METRICS_URL must be cleared for the spawned proxy"
        );
    }

    #[test]
    fn metadata_matches_config_ignores_metrics_port() {
        // metrics_port is a best-effort runtime fact reconciled after readiness,
        // not a config input — a difference must not churn-restart a healthy
        // proxy.
        let mut on_disk = BTreeMap::new();
        on_disk.insert("router_url".to_owned(), "https://api.rayline.ai".to_owned());
        let mut requested = on_disk.clone();
        requested.insert("metrics_port".to_owned(), "20814".to_owned());

        assert!(metadata_matches_config(&on_disk, &requested));
    }

    #[test]
    fn metadata_detects_router_config_content_changes_at_same_path() {
        let home = unique_test_dir("router-config-meta");
        std::fs::create_dir_all(&home).unwrap();
        let config_path = home.join("routes.json");
        std::fs::write(
            &config_path,
            r#"{"endpoints":{},"routes":{"default":{"endpoint":"local"}}}"#,
        )
        .unwrap();

        let mut request = RouterStartRequest::local_router_defaults(false);
        request.router_config_path = Some(config_path.clone());
        request.local_model_id = "local-model".to_owned();
        let on_disk = router_meta(&home, &request, None, None);

        std::fs::write(
            &config_path,
            r#"{"endpoints":{},"routes":{"default":{"endpoint":"ollama","model":"llama3"}}}"#,
        )
        .unwrap();
        let requested = router_meta(&home, &request, None, None);

        assert_ne!(
            on_disk.get("router_config_sha256"),
            requested.get("router_config_sha256")
        );
        assert!(!metadata_matches_config(&on_disk, &requested));

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn metadata_detects_router_key_acquisition_in_non_proxy_mode() {
        // Regression: a non-proxy local-plane router (isolated mixed configs like
        // Rc-K) exports RAYLINE_ROUTER_API_KEY for a rayline-cloud endpoint. A key
        // acquired after the daemon is already running (first launch pre-login →
        // relaunch post-login) must be tracked in router_meta so the reuse check
        // restarts the daemon instead of keeping a keyless env.
        let home = unique_test_dir("router-key-meta");
        std::fs::create_dir_all(&home).unwrap();

        let mut request = RouterStartRequest::local_router_defaults(false);
        request.local_model_id = "local-model".to_owned();
        assert!(
            !request.enable_proxy,
            "isolated local-plane runs proxy-disabled"
        );

        // Daemon first started without a key (pre-login) records no fingerprint.
        let keyless = router_meta(&home, &request, None, None);
        assert!(!keyless.contains_key("router_key_sha256"));

        // Relaunch after ensure_router_key succeeds — now carries the session key.
        let keyed = router_meta(&home, &request, None, Some("rlk-session-key"));
        assert!(keyed.contains_key("router_key_sha256"));

        // Acquisition (keyless → keyed) is seen as a config change → restart.
        assert!(!metadata_matches_config(&keyless, &keyed));

        // Rotation (keyed → different key) is a change.
        let rotated = router_meta(&home, &request, None, Some("rlk-rotated-key"));
        assert_ne!(
            keyed.get("router_key_sha256"),
            rotated.get("router_key_sha256")
        );
        assert!(!metadata_matches_config(&keyed, &rotated));

        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn resolve_start_model_writes_provider_routes_for_saved_provider() {
        let home = unique_test_dir("router-provider-routes");
        std::fs::create_dir_all(&home).unwrap();
        crate::local_model::set_provider_endpoint_in_home(
            &home,
            "ollama",
            "http://localhost:11434",
            "qwen3-coder:30b",
            "openai_chat",
        )
        .unwrap();
        let explicit_config_path = home.join("explicit-routes.json");
        std::fs::write(
            &explicit_config_path,
            r#"{"endpoints":[],"routes":{"subagents":{"Explore":{"endpoint":"local"}}}}"#,
        )
        .unwrap();

        let mut request = RouterStartRequest::local_router_defaults(false);
        request.router_config_path = Some(explicit_config_path.clone());
        let resolved = resolve_start_model(&home, request).await.unwrap();

        assert_eq!(
            resolved.upstream_url.as_deref(),
            Some("http://localhost:11434")
        );
        assert_eq!(resolved.upstream_model.as_deref(), Some("qwen3-coder:30b"));
        let provider_routes_path = resolved.router_config_path.as_ref().unwrap();
        assert_ne!(provider_routes_path, &explicit_config_path);
        let routes: Value =
            serde_json::from_str(&std::fs::read_to_string(provider_routes_path).unwrap()).unwrap();
        assert_eq!(routes["endpoints"][0]["id"], "ollama");
        assert_eq!(routes["endpoints"][0]["protocol"], "openai_chat");
        assert_eq!(
            routes["endpoints"][0]["base_url"],
            "http://localhost:11434/v1"
        );
        assert_eq!(
            routes["routes"]["model_routes"]["rayline-local"]["endpoint"],
            "ollama"
        );
        assert_eq!(
            routes["routes"]["subagents"]["Explore"]["endpoint"],
            "ollama"
        );
        assert_eq!(
            routes["routes"]["subagents"]["Explore"]["model"],
            "qwen3-coder:30b"
        );

        let _ = std::fs::remove_dir_all(home);
    }

    #[tokio::test]
    async fn reconcile_drops_metrics_port_when_self_host_failed() {
        let home = unique_test_dir("reconcile-metrics-drop");
        let paths = RouterPaths::new(&home);
        std::fs::create_dir_all(paths.data_dir()).unwrap();
        let mut meta = BTreeMap::new();
        meta.insert("proxy_port".to_owned(), "20810".to_owned());
        meta.insert("metrics_port".to_owned(), "20814".to_owned());
        std::fs::write(&paths.proxy_meta_file, format_meta(&meta)).unwrap();

        // A port bound then released: probing it gets connection-refused.
        let released = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = released.local_addr().unwrap().port();
        drop(released);

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let mut output = String::new();
        reconcile_self_hosted_metrics_meta(&paths, Some(dead_port), &client, &mut output).await;

        let after = read_meta(&paths.proxy_meta_file);
        assert!(
            !after.contains_key("metrics_port"),
            "a metrics port the proxy never bound must not stay advertised"
        );
        assert!(output.contains("disabled"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn reconcile_drops_serve_metrics_port_nothing_answers_on() {
        // #82: serve binds metrics best-effort, so a port it lost to another
        // process must not stay advertised for proxies to forward to.
        let home = unique_test_dir("reconcile-serve-metrics-drop");
        let paths = RouterPaths::new(&home);
        std::fs::create_dir_all(paths.data_dir()).unwrap();
        let released = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_port = released.local_addr().unwrap().port();
        drop(released);
        let mut meta = BTreeMap::new();
        meta.insert("router_url".to_owned(), "https://r".to_owned());
        meta.insert("metrics_port".to_owned(), dead_port.to_string());
        std::fs::write(&paths.meta_file, format_meta(&meta)).unwrap();
        std::fs::write(&paths.pid_file, "4242").unwrap();

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let mut output = String::new();
        reconcile_serve_metrics_meta(&paths, 4242, Some(dead_port), &client, &mut output).await;

        let after = read_meta(&paths.meta_file);
        assert!(!after.contains_key("metrics_port"));
        assert_eq!(
            after.get("router_url").map(String::as_str),
            Some("https://r")
        );
        assert!(output.contains("disabled"));
        assert_eq!(serve_metrics_url(true, &after), None);
        std::fs::remove_dir_all(&home).ok();
    }

    /// A metrics-control stub answering `/healthz` as the daemon does, naming
    /// `pid` as its owner, for as many requests as arrive.
    async fn healthz_stub(pid: u32) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body =
                    format!("{{\"ok\":true,\"runtime\":\"rayline-router-metrics\",\"pid\":{pid}}}");
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        port
    }

    fn serve_meta_with_metrics_port(paths: &RouterPaths, port: u16, pid: i32) {
        std::fs::create_dir_all(paths.data_dir()).unwrap();
        let mut meta = BTreeMap::new();
        meta.insert("router_url".to_owned(), "https://r".to_owned());
        meta.insert("metrics_port".to_owned(), port.to_string());
        std::fs::write(&paths.meta_file, format_meta(&meta)).unwrap();
        std::fs::write(&paths.pid_file, pid.to_string()).unwrap();
    }

    fn quick_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn reconcile_drops_serve_metrics_port_another_process_won() {
        // #82: another metrics server bound the port between the probe and
        // serve's own bind. It answers, but it is not serve, so proxies must
        // not forward to it.
        let home = unique_test_dir("reconcile-serve-metrics-foreign");
        let paths = RouterPaths::new(&home);
        let port = healthz_stub(999_999).await;
        serve_meta_with_metrics_port(&paths, port, 4242);

        let mut output = String::new();
        reconcile_serve_metrics_meta(&paths, 4242, Some(port), &quick_client(), &mut output).await;

        let after = read_meta(&paths.meta_file);
        assert!(
            !after.contains_key("metrics_port"),
            "a metrics port another process owns must not stay advertised"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn reconcile_keeps_serve_metrics_port_serve_owns() {
        let home = unique_test_dir("reconcile-serve-metrics-own");
        let paths = RouterPaths::new(&home);
        let port = healthz_stub(4242).await;
        serve_meta_with_metrics_port(&paths, port, 4242);

        let mut output = String::new();
        reconcile_serve_metrics_meta(&paths, 4242, Some(port), &quick_client(), &mut output).await;

        let after = read_meta(&paths.meta_file);
        assert_eq!(
            after.get("metrics_port").map(String::as_str),
            Some(port.to_string().as_str())
        );
        assert!(output.is_empty());
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn reconcile_leaves_a_replacement_serves_meta_alone() {
        // #82: the lock is released before readiness, so another launcher can
        // restart serve meanwhile. The started serve's port is closed, but the
        // meta now belongs to the replacement and must not be edited.
        let home = unique_test_dir("reconcile-serve-metrics-replaced");
        let paths = RouterPaths::new(&home);
        // The replacement reuses the same metrics port, so only the pid-file
        // check, not the port comparison, keeps its meta intact.
        let replacement_port = healthz_stub(5151).await;
        serve_meta_with_metrics_port(&paths, replacement_port, 5151);

        let mut output = String::new();
        reconcile_serve_metrics_meta(
            &paths,
            4242,
            Some(replacement_port),
            &quick_client(),
            &mut output,
        )
        .await;

        let after = read_meta(&paths.meta_file);
        assert_eq!(
            after.get("metrics_port").map(String::as_str),
            Some(replacement_port.to_string().as_str()),
            "the replacement serve's metrics port was removed"
        );
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn proxy_forwards_metrics_only_to_a_port_serve_owns() {
        // #82: before reconciliation removes a lost port, a proxy launcher can
        // read it from serve's meta; it must not forward to another process.
        let home = unique_test_dir("forward-metrics-owner");
        let paths = RouterPaths::new(&home);
        let foreign = healthz_stub(999_999).await;
        serve_meta_with_metrics_port(&paths, foreign, 4242);
        assert!(!metrics_port_owned_by_serve(&paths, &quick_client(), foreign).await);

        let owned = healthz_stub(4242).await;
        serve_meta_with_metrics_port(&paths, owned, 4242);
        assert!(metrics_port_owned_by_serve(&paths, &quick_client(), owned).await);
        std::fs::remove_dir_all(&home).ok();
    }

    #[tokio::test]
    async fn reconcile_keeps_metrics_port_when_self_host_serving() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let home = unique_test_dir("reconcile-metrics-keep");
        let paths = RouterPaths::new(&home);
        std::fs::create_dir_all(paths.data_dir()).unwrap();
        let mut meta = BTreeMap::new();
        meta.insert("metrics_port".to_owned(), "20814".to_owned());
        std::fs::write(&paths.proxy_meta_file, format_meta(&meta)).unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let live_port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf).await;
                let body = b"{\"ok\":true}";
                let head = format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n", body.len());
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body).await;
                let _ = stream.flush().await;
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(1))
            .build()
            .unwrap();
        let mut output = String::new();
        reconcile_self_hosted_metrics_meta(&paths, Some(live_port), &client, &mut output).await;

        let after = read_meta(&paths.proxy_meta_file);
        assert_eq!(after.get("metrics_port").map(String::as_str), Some("20814"));
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn metrics_port_candidates_find_isolated_proxy_when_serve_and_proxy_absent() {
        let empty = BTreeMap::new();
        let isolated = meta_with_metrics_port(20814);

        let ports = metrics_port_candidates(&empty, &empty, &isolated);

        assert_eq!(ports, vec![20814, rayline_metrics::DEFAULT_METRICS_PORT]);
    }

    #[test]
    fn metrics_port_candidates_dedupe_repeated_and_default_ports() {
        let serve = meta_with_metrics_port(rayline_metrics::DEFAULT_METRICS_PORT);
        let proxy = meta_with_metrics_port(rayline_metrics::DEFAULT_METRICS_PORT);
        let empty = BTreeMap::new();

        let ports = metrics_port_candidates(&serve, &proxy, &empty);

        assert_eq!(ports, vec![rayline_metrics::DEFAULT_METRICS_PORT]);
    }

    #[test]
    fn top_numeric_cells_use_compact_suffixes() {
        let row = serde_json::json!({
            "input_tokens": 10_200,
            "output_tokens": 1_200_000,
            "duration_ms": 10_200,
            "ttft_ms": 999,
            "prefill_tps": 12_000.2,
            "output_tps": 1_200_000.0,
            "cache_hit_ratio": 0.876,
        });

        assert_eq!(count_cell(&row, "input_tokens"), "10.2k");
        assert_eq!(count_cell(&row, "output_tokens"), "1.2M");
        assert_eq!(ms_cell(&row, "duration_ms"), "10.2s");
        assert_eq!(ms_cell(&row, "ttft_ms"), "999ms");
        assert_eq!(rate_cell(&row, "prefill_tps"), "12.0k");
        assert_eq!(rate_cell(&row, "output_tps"), "1.2M");
        assert_eq!(percent_cell(&row, "cache_hit_ratio"), "88%");
    }

    #[test]
    fn top_table_places_cache_percent_after_token_columns() {
        let header = top_table_header(140);
        let input = header.find("IN").expect("header includes input column");
        let output = header.find("OUT").expect("header includes output column");
        let cache = header
            .find("CACHE %")
            .expect("header includes cache percent column");
        let age = header.rfind("AGE").expect("header includes age column");
        assert!(input < output);
        assert!(output < cache);
        assert!(cache < age);

        let row = serde_json::json!({
            "request_id": "req",
            "agent_type": "main",
            "target": "local",
            "state": "done",
            "selected_model": "model",
            "input_tokens": 10_200,
            "output_tokens": 1_200_000,
            "duration_ms": 10_200,
            "ttft_ms": 999,
            "prefill_tps": 12_000.2,
            "output_tps": 1_200_000.0,
            "cache_hit_ratio": 0.876,
        });

        let cells = top_table_row(&row, 140)
            .split_whitespace()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        assert_eq!(
            &cells[4..11],
            ["10.2k", "1.2M", "88%", "10.2s", "999ms", "12.0k", "1.2M"]
        );
    }

    #[test]
    fn top_filters_proxied_sideband_rows_by_default() {
        let mut snapshot = serde_json::json!({
            "active": [
                {
                    "request_id": "llm",
                    "agent_type": "main",
                    "target": "anthropic",
                    "state": "streaming",
                    "selected_model": "claude-sonnet-4-5",
                    "policy": "selective_main_passthrough"
                },
                {
                    "request_id": "sideband",
                    "agent_type": "main",
                    "target": "anthropic",
                    "state": "done",
                    "selected_model": "-",
                    "policy": PROXIED_TRAFFIC_POLICY
                }
            ],
            "recent": [
                {
                    "request_id": "recent-sideband",
                    "target": "anthropic",
                    "state": "done",
                    "policy": PROXIED_TRAFFIC_POLICY
                }
            ],
            "totals": {
                "active_requests": 2,
                "completed_requests": 3,
                "errored_requests": 0
            }
        });

        assert_eq!(visible_row_count(&snapshot, "active", false), 1);
        assert_eq!(visible_row_count(&snapshot, "recent", false), 0);
        assert_eq!(hidden_proxied_row_count(&snapshot), 2);

        let text = format_top_snapshot(&snapshot, false);
        assert!(text.contains("active: 1"));
        assert!(text.contains("hidden-proxied: 2"));
        assert!(text.contains("llm"));
        assert!(!text.contains("sideband"));

        filter_top_snapshot(&mut snapshot, false);
        assert_eq!(
            snapshot["active"].as_array().expect("active array").len(),
            1
        );
        assert_eq!(
            snapshot["recent"].as_array().expect("recent array").len(),
            0
        );
        assert_eq!(snapshot["totals"]["active_requests"], 1);
    }

    #[test]
    fn top_all_mode_labels_proxied_sideband_rows() {
        let row = serde_json::json!({
            "request_id": "sideband",
            "agent_type": "main",
            "target": "anthropic",
            "endpoint_id": "/api/oauth/token",
            "state": "done",
            "selected_model": "-",
            "policy": PROXIED_TRAFFIC_POLICY
        });

        assert_eq!(top_target_cell(&row), "proxied");
        assert_eq!(top_policy_cell(&row), "proxied traffic");
        assert_eq!(top_model_cell(&row), "/api/oauth/token / proxied traffic");

        let rendered = top_table_row(&row, 140);
        assert!(rendered.contains("proxied"));
        assert!(rendered.contains("proxied traffic"));
        assert!(rendered.contains("/api/oauth/token"));
    }

    #[test]
    fn top_policy_display_simplifies_proxy_reasons() {
        for (raw, display) in [
            ("selective_main_passthrough", "main passthrough"),
            ("selective_subagent_passthrough", "subagent passthrough"),
            ("selective_subagent_header", "subagent routed"),
            ("router_routed_path", "router routed"),
            ("anthropic_passthrough", "passthrough"),
        ] {
            let row = serde_json::json!({ "policy": raw });
            assert_eq!(top_policy_cell(&row), display);
        }
    }

    #[test]
    fn top_row_color_highlights_local_requests() {
        let local = serde_json::json!({ "target": "local" });
        let remote = serde_json::json!({ "target": "anthropic" });

        assert_eq!(top_row_color(&local), Some(Color::DarkCyan));
        assert_eq!(top_row_color(&remote), None);
    }

    #[test]
    fn verified_cache_requires_exact_revision_and_sha256() {
        let test_dir = unique_test_dir("verified-router-cache");
        let hub = test_dir.join("hub");
        let repo = "org/repo";
        let filename = "model.gguf";
        let revision = "ffffffffffffffffffffffffffffffffffffffff";
        let old_revision = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
        let body = b"verified model bytes";
        let sha256 = format!("{:x}", Sha256::digest(body));
        let wrong_sha = format!("{:x}", Sha256::digest(b"wrong bytes"));

        let old_snapshot = hf_cache_verified_snapshot_path(&hub, repo, filename, old_revision);
        fs::create_dir_all(old_snapshot.parent().unwrap()).unwrap();
        fs::write(&old_snapshot, body).unwrap();

        assert!(!hf_cache_has_verified_gguf_at(
            &hub,
            repo,
            filename,
            Some(revision),
            Some(&sha256),
        ));

        let snapshot = hf_cache_verified_snapshot_path(&hub, repo, filename, revision);
        fs::create_dir_all(snapshot.parent().unwrap()).unwrap();
        fs::write(&snapshot, body).unwrap();

        assert!(hf_cache_has_verified_gguf_at(
            &hub,
            repo,
            filename,
            Some(revision),
            Some(&sha256),
        ));
        assert!(!hf_cache_has_verified_gguf_at(
            &hub,
            repo,
            filename,
            Some(revision),
            Some(&wrong_sha),
        ));
        assert!(!hf_cache_has_verified_gguf_at(
            &hub,
            repo,
            filename,
            Some(revision),
            None,
        ));

        let _ = fs::remove_dir_all(test_dir);
    }

    // -----------------------------------------------------------------------
    // flock + atomic meta tests (Task 1)
    // -----------------------------------------------------------------------

    /// Minimal seam: simulate the locked check-then-spawn logic with a fake
    /// spawn that increments a counter file instead of launching a real process.
    /// This exercises `acquire_router_lock` + `write_pid_meta_atomic` without
    /// requiring a real rld binary.
    ///
    /// Protocol:
    ///   - counter file (`{prefix}.spawn_count`) tracks total spawns
    ///   - pid file written by the first spawn (pid = 1)
    ///   - meta file written atomically with a known key
    ///   - second caller detects pid file → reuses (no second spawn)
    #[cfg(unix)]
    async fn start_with_stub(paths: &RouterPaths) -> io::Result<()> {
        start_with_stub_at(
            paths,
            &paths.lock_file,
            &paths.pid_file,
            &paths.meta_file,
            "spawn_count",
        )
        .await
    }

    /// Stub variant for the standalone proxy path: uses the proxy lock/pid/meta
    /// and a separate counter file.
    #[cfg(unix)]
    async fn start_proxy_with_stub(paths: &RouterPaths) -> io::Result<()> {
        start_with_stub_at(
            paths,
            &paths.proxy_lock_file,
            &paths.proxy_pid_file,
            &paths.proxy_meta_file,
            "proxy_spawn_count",
        )
        .await
    }

    /// Shared stub: simulate the locked check-then-spawn logic against the given
    /// lock/pid/meta paths and counter file, with a fake spawn (counter bump)
    /// instead of launching a real process.  Exercises `acquire_router_lock` +
    /// `write_pid_meta_atomic`.
    #[cfg(unix)]
    async fn start_with_stub_at(
        paths: &RouterPaths,
        lock_file: &Path,
        pid_file: &Path,
        meta_file: &Path,
        counter_name: &str,
    ) -> io::Result<()> {
        // Small sleep *before* acquiring the lock to encourage interleaving in
        // concurrent callers — this is what makes the race reproducible in the
        // pre-fix (RED) phase.
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;

        let _lock = acquire_router_lock(lock_file)?;

        // Under the lock: check if already started.
        if read_pid(pid_file).is_some() {
            // Another launcher got here first — reuse its daemon.
            return Ok(());
        }

        // "Spawn": increment counter file and write pid/meta.
        let counter_path = paths.data_dir().join(counter_name);
        let count: u32 = std::fs::read_to_string(&counter_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        std::fs::write(&counter_path, format!("{}\n", count + 1))?;

        let fake_pid: i32 = 1;
        let mut meta = BTreeMap::new();
        meta.insert("injector_port".to_owned(), "20809".to_owned());
        write_pid_meta_atomic(pid_file, meta_file, fake_pid, &meta)?;
        Ok(())
    }

    #[cfg(unix)]
    fn count_spawned_daemons_at(paths: &RouterPaths, counter_name: &str) -> u32 {
        let counter_path = paths.data_dir().join(counter_name);
        std::fs::read_to_string(&counter_path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    #[cfg(unix)]
    fn count_spawned_daemons(paths: &RouterPaths) -> u32 {
        count_spawned_daemons_at(paths, "spawn_count")
    }

    #[cfg(unix)]
    fn meta_parses_at(meta_file: &Path) -> bool {
        let meta = read_meta(meta_file);
        meta.contains_key("injector_port")
    }

    #[cfg(unix)]
    fn meta_parses(paths: &RouterPaths) -> bool {
        meta_parses_at(&paths.meta_file)
    }

    /// Run two `f` invocations concurrently against the same paths via blocking
    /// tasks, returning their results.
    #[cfg(unix)]
    async fn join_two_concurrent_starts<Fut>(
        paths: &std::sync::Arc<RouterPaths>,
        f: fn(std::sync::Arc<RouterPaths>) -> Fut,
    ) -> (io::Result<()>, io::Result<()>)
    where
        Fut: std::future::Future<Output = io::Result<()>> + Send + 'static,
    {
        let p1 = paths.clone();
        let p2 = paths.clone();
        let (a, b) = tokio::join!(
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(f(p1))
            }),
            tokio::task::spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(f(p2))
            })
        );
        (a.expect("task a panicked"), b.expect("task b panicked"))
    }

    /// Two concurrent serve starts must spawn exactly one daemon; the second
    /// reuses the first.  Meta must always parse (no torn writes).
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_starts_spawn_one_daemon() {
        let paths = std::sync::Arc::new(RouterPaths::temp());
        let (a, b) =
            join_two_concurrent_starts(&paths, |p| async move { start_with_stub(&p).await }).await;
        a.expect("start_with_stub a failed");
        b.expect("start_with_stub b failed");

        assert_eq!(
            count_spawned_daemons(&paths),
            1,
            "exactly one daemon must be spawned; second caller must reuse"
        );
        assert!(
            meta_parses(&paths),
            "meta file must always parse (no torn writes)"
        );
        let _ = std::fs::remove_dir_all(paths.data_dir());
    }

    /// Two concurrent standalone proxy starts must spawn exactly one proxy; the
    /// second reuses the first.  Proxy meta must always parse.
    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_proxy_starts_spawn_one_proxy() {
        let paths = std::sync::Arc::new(RouterPaths::temp());
        let (a, b) =
            join_two_concurrent_starts(&paths, |p| async move { start_proxy_with_stub(&p).await })
                .await;
        a.expect("start_proxy_with_stub a failed");
        b.expect("start_proxy_with_stub b failed");

        assert_eq!(
            count_spawned_daemons_at(&paths, "proxy_spawn_count"),
            1,
            "exactly one proxy must be spawned; second caller must reuse"
        );
        assert!(
            meta_parses_at(&paths.proxy_meta_file),
            "proxy meta file must always parse (no torn writes)"
        );
        let _ = std::fs::remove_dir_all(paths.data_dir());
    }

    /// Concurrent atomic writes to the same meta path must never produce a
    /// partially-written file visible to readers.
    #[test]
    fn atomic_write_never_tears_meta() {
        let dir = unique_test_dir("atomic-write-test");
        std::fs::create_dir_all(&dir).unwrap();
        let dest = dir.join("rld.meta");

        // Write an initial version so readers always see *something*.
        let mut initial = BTreeMap::new();
        initial.insert("injector_port".to_owned(), "20809".to_owned());
        atomic_write(&dest, format_meta(&initial).as_bytes()).unwrap();

        let dest_clone = dest.clone();
        let writer = std::thread::spawn(move || {
            for i in 0..200u32 {
                let mut meta = BTreeMap::new();
                meta.insert("injector_port".to_owned(), (20809 + i).to_string());
                atomic_write(&dest_clone, format_meta(&meta).as_bytes()).unwrap();
            }
        });

        // Reader: parse every read; none should be empty or malformed.
        for _ in 0..200 {
            let text = std::fs::read_to_string(&dest).unwrap_or_default();
            // Either old or new content is fine, but it must not be empty
            // and every line must be parseable as key=value.
            for line in text.lines() {
                assert!(line.contains('='), "torn meta line (no '='): {line:?}");
            }
        }
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn config_may_local_detects_rcl_configs() {
        use serde_json::json;
        let dir = unique_test_dir("codex-may-local-detect");
        std::fs::create_dir_all(&dir).unwrap();

        // Rcl-Rcl-shape: a `rayline-cloud` route carrying `local_models` → may-local on.
        let rcl_rcl = dir.join("Rcl-Rcl.json");
        std::fs::write(
            &rcl_rcl,
            serde_json::to_vec(&json!({
                "endpoints": [
                    { "id": "rayline-cloud", "protocol": "anthropic_messages",
                      "base_url": "https://api.rayline.ai", "models": ["rayline-router"] },
                    { "id": "ollama", "protocol": "openai_chat",
                      "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
                ],
                "routes": {
                    "main":     { "endpoint": "rayline-cloud", "model": "rayline-router",
                                  "router": "rayline-cloud", "local_models": ["qwen2.5-coder:7b"] },
                    "subagent": { "endpoint": "rayline-cloud", "model": "rayline-router",
                                  "router": "rayline-cloud", "local_models": ["qwen2.5-coder:7b"] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // Rc-Rc-shape: no `local_models` → may-local off.
        let rc_rc = dir.join("Rc-Rc.json");
        std::fs::write(
            &rc_rc,
            serde_json::to_vec(&json!({
                "endpoints": [
                    { "id": "rayline-cloud", "protocol": "anthropic_messages",
                      "base_url": "https://api.rayline.ai", "models": ["rayline-router"] }
                ],
                "routes": {
                    "main":     { "endpoint": "rayline-cloud", "model": "rayline-router" },
                    "subagent": { "endpoint": "rayline-cloud", "model": "rayline-router" }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        // A `rayline-cloud` route carrying `local_models` → may-local on (drives
        // the codex may-local branch in `start_from_cli`); no `local_models` → off.
        assert!(
            crate::router_config::config_may_local(&rcl_rcl).is_some(),
            "Rcl-Rcl config → may-local detected"
        );
        assert!(
            crate::router_config::config_may_local(&rc_rc).is_none(),
            "Rc-Rc config (no local_models) → may-local off"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn router_api_key_override_survives_disabled_proxy() {
        // Regression: the isolated local-plane path starts the router with
        // enable_proxy = false. An explicit `router_api_key_override` (the
        // `rayline auth login` session key injected for a config whose
        // `rayline-cloud` endpoint reads RAYLINE_ROUTER_API_KEY, e.g. Rc-K/Rl-K)
        // must still be resolved — not dropped by the proxy gate — so the cloud
        // main leg authenticates instead of 502-ing.
        let home = unique_test_dir("router-key-override");
        std::fs::create_dir_all(&home).unwrap();

        let mut request = RouterStartRequest::local_router_defaults(false);
        request.enable_proxy = false;
        request.router_api_key_override = Some("rlk-test-session-key".to_owned());

        let resolved = resolve_router_api_key(&home, &request).unwrap();
        assert_eq!(resolved.as_deref(), Some("rlk-test-session-key"));

        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn local_plane_disabled_proxy_forwards_env_key() {
        // A proxy-less local-plane start (the Codex daemon shape, including
        // `router start --mode codex`) reads RAYLINE_ROUTER_API_KEY even without
        // an explicit override, so a config's cloud-router leg authenticates and
        // a changed key reaches the daemon reuse metadata. This is checked before
        // the `!enable_proxy` short-circuit.
        let home = unique_test_dir("router-key-localplane-env");
        std::fs::create_dir_all(&home).unwrap();

        let mut request = RouterStartRequest::local_router_defaults(false);
        request.enable_proxy = false;
        request.router_api_key_override = None;

        let var = "RAYLINE_ROUTER_API_KEY";
        let previous = std::env::var_os(var);
        // SAFETY: single-threaded suite (`--test-threads=1`); we own the var for
        // this test and restore it below.
        unsafe { std::env::set_var(var, "rlk-env-forwarded") };
        assert_eq!(
            resolve_router_api_key(&home, &request).unwrap().as_deref(),
            Some("rlk-env-forwarded"),
            "an explicit env var must be forwarded on the proxy-less local plane"
        );
        // Unset → empty string (preserve the user value, else empty), not an error.
        // SAFETY: see the note above.
        unsafe { std::env::remove_var(var) };
        assert_eq!(
            resolve_router_api_key(&home, &request).unwrap().as_deref(),
            Some("")
        );

        // SAFETY: see the note above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
        let _ = std::fs::remove_dir_all(home);
    }
}

fn hf_hub_dir(home: &Path) -> PathBuf {
    if let Some(hub_cache) = std::env::var_os("HF_HUB_CACHE") {
        return PathBuf::from(hub_cache);
    }
    if let Some(hf_home) = std::env::var_os("HF_HOME") {
        return PathBuf::from(hf_home).join("hub");
    }
    platform_cache_dir(home).join("huggingface/hub")
}

fn platform_cache_dir(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        return home.join("Library/Caches");
    }
    if cfg!(windows) {
        if let Some(local_appdata) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_appdata);
        }
        return home.join("AppData/Local");
    }
    if let Some(xdg_cache_home) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(xdg_cache_home);
    }
    home.join(".cache")
}

async fn render_status_from_home_with_client(
    home: &Path,
    client: &reqwest::Client,
) -> io::Result<String> {
    let paths = RouterPaths::new(home);
    let pid = read_pid(&paths.pid_file);
    let meta = read_meta(&paths.meta_file);
    let injector_port = parse_port(meta.get("injector_port"), DEFAULT_INJECTOR_PORT);
    let adapter_port = parse_port(meta.get("adapter_port"), DEFAULT_ADAPTER_PORT);
    let mut output = String::new();

    match pid {
        None => {
            output.push_str(&format!("{}: not running (no pidfile).\n", daemon_name()));
            append_proxy_sidecar_status(&mut output, &paths, client).await;
            append_isolated_proxy_status(&mut output, home, client).await;
            return Ok(output);
        }
        Some(pid) if !process_exists(pid) => {
            output.push_str(&format!(
                "{}: stale pidfile (pid {pid} gone). Run `{} router stop` to clean up.\n",
                daemon_name(),
                cli_name()
            ));
            append_proxy_sidecar_status(&mut output, &paths, client).await;
            append_isolated_proxy_status(&mut output, home, client).await;
            return Ok(output);
        }
        Some(pid) => {
            output.push_str(&format!("{}: pid {pid}\n", daemon_name()));
        }
    }

    for key in ["model_repo", "model_file", "local_model_id", "router_url"] {
        if let Some(value) = meta.get(key) {
            output.push_str(&format!("  {key}: {value}\n"));
        }
    }

    let injector_health = healthz(client, injector_port).await;
    if let Some(health) = injector_health.as_ref() {
        output.push_str(&format!(
            "  injector :{injector_port} \u{2713} ({})\n",
            value_display_or_empty(health.get("router_url")),
        ));
    } else {
        output.push_str(&format!(
            "  injector :{injector_port} \u{2717} (no healthz)\n"
        ));
    }

    let adapter_health = healthz(client, adapter_port).await;
    if let Some(health) = adapter_health.as_ref() {
        output.push_str(&format!(
            "  adapter  :{adapter_port} \u{2713} (\u{2192} {})\n",
            value_display_or_empty(health.get("target")),
        ));
    } else {
        output.push_str(&format!(
            "  adapter  :{adapter_port} \u{2717} (no healthz)\n"
        ));
    }

    if let Some(proxy_port) = parse_optional_port(meta.get("proxy_port")) {
        append_proxy_health(&mut output, client, proxy_port).await;
    } else {
        append_proxy_sidecar_status(&mut output, &paths, client).await;
    }
    output.push_str(&format!("  log: {}\n", paths.log_file.display()));
    append_isolated_proxy_status(&mut output, home, client).await;

    Ok(output)
}

pub fn tail_log(path: &Path, lines: i64) -> String {
    if !path.is_file() {
        return "(no log)".to_owned();
    };
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => return format!("(could not read log: {error})"),
    };

    let entries = text.lines().collect::<Vec<_>>();
    let start = tail_start(entries.len(), lines);
    entries[start..].join("\n")
}

fn tail_start(len: usize, lines: i64) -> usize {
    if lines == 0 {
        return 0;
    }
    if lines > 0 {
        return len.saturating_sub(lines as usize);
    }
    len.min(lines.unsigned_abs() as usize)
}

async fn stop_router(
    paths: &RouterPaths,
    client: &reqwest::Client,
    output: &mut String,
) -> io::Result<()> {
    let Some(pid) = read_pid(&paths.pid_file) else {
        output.push_str(&format!("{} is not running (no pidfile).\n", daemon_name()));
        return Ok(());
    };
    if !process_exists(pid) {
        output.push_str(&format!(
            "{} pidfile points at {pid} but the process is gone. Cleaning up.\n",
            daemon_name()
        ));
        clear_router_state(paths);
        return Ok(());
    }

    let meta = read_meta(&paths.meta_file);
    if !is_rld_process(pid, &meta, RldMode::Serve, client).await {
        output.push_str(&format!(
            "{} pidfile points at {pid}, but that process is not this {} supervisor. Cleaning up.\n",
            daemon_name(),
            daemon_name()
        ));
        clear_router_state(paths);
        return Ok(());
    }

    output.push_str(&format!("Stopping {} (pid {pid})\u{2026}\n", daemon_name()));
    match terminate_process(pid) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            clear_router_state(paths);
            output.push_str("already gone.\n");
            return Ok(());
        }
        Err(error) => return Err(error),
    }

    for _ in 0..50 {
        if !process_exists(pid) {
            clear_router_state(paths);
            output.push_str(&format!("{} stopped.\n", daemon_name()));
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    output.push_str("SIGTERM timeout \u{2014} forcing router process tree to stop.\n");
    force_stop_process_group(pid)?;
    clear_router_state(paths);
    output.push_str(&format!("{} killed.\n", daemon_name()));
    Ok(())
}

async fn stop_proxy(
    paths: &RouterPaths,
    client: &reqwest::Client,
    output: &mut String,
    quiet: bool,
) -> io::Result<()> {
    let Some(pid) = read_pid(&paths.proxy_pid_file) else {
        if !quiet {
            output.push_str(&format!(
                "{} proxy is not running (no pidfile).\n",
                daemon_name()
            ));
        }
        clear_proxy_state(paths);
        return Ok(());
    };
    if !process_exists(pid) {
        if !quiet {
            output.push_str(&format!(
                "{} proxy pidfile points at {pid} but the process is gone. Cleaning up.\n",
                daemon_name()
            ));
        }
        clear_proxy_state(paths);
        return Ok(());
    }

    let meta = read_meta(&paths.proxy_meta_file);
    if !is_rld_process(pid, &meta, RldMode::Proxy, client).await {
        if !quiet {
            output.push_str(&format!(
                "{} proxy pidfile points at {pid}, but that process is not this {} proxy. Cleaning up.\n",
                daemon_name(),
                daemon_name()
            ));
        }
        clear_proxy_state(paths);
        return Ok(());
    }

    if !quiet {
        output.push_str(&format!(
            "Stopping {} proxy (pid {pid})\u{2026}\n",
            daemon_name()
        ));
    }
    if let Err(error) = terminate_process(pid) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    for _ in 0..50 {
        if !process_exists(pid) {
            clear_proxy_state(paths);
            if !quiet {
                output.push_str(&format!("{} proxy stopped.\n", daemon_name()));
            }
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    if !quiet {
        output.push_str("SIGTERM timeout \u{2014} forcing proxy process tree to stop.\n");
    }
    force_stop_process_group(pid)?;
    clear_proxy_state(paths);
    if !quiet {
        output.push_str(&format!("{} proxy killed.\n", daemon_name()));
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum RldMode {
    Serve,
    Proxy,
}

impl RldMode {
    fn arg(self) -> &'static str {
        match self {
            Self::Serve => "serve",
            Self::Proxy => "proxy",
        }
    }
}

async fn is_rld_process(
    pid: i32,
    meta: &BTreeMap<String, String>,
    mode: RldMode,
    client: &reqwest::Client,
) -> bool {
    let Some(argv) = process_argv(pid) else {
        return running_matches_meta(meta, mode, client).await;
    };
    let Some(program) = argv.first() else {
        return running_matches_meta(meta, mode, client).await;
    };

    let mut expected_names = vec![daemon_name().to_owned(), format!("{}.exe", daemon_name())];
    if let Some(bin_path) = meta.get("bin_path") {
        expected_names.push(process_name(bin_path));
    }
    if !expected_names
        .iter()
        .any(|expected| process_name(program) == *expected)
    {
        return false;
    }
    if argv.get(1).is_none_or(|arg| arg != mode.arg()) {
        return false;
    }

    match mode {
        RldMode::Serve => match parse_optional_port(meta.get("injector_port")) {
            Some(port) => healthz(client, port)
                .await
                .is_none_or(|health| router_health_matches_meta(&health, meta)),
            None => true,
        },
        RldMode::Proxy => match parse_optional_port(meta.get("proxy_port")) {
            Some(port) => healthz(client, port)
                .await
                .is_none_or(|health| proxy_health_matches_meta(&health, meta)),
            None => true,
        },
    }
}

async fn running_matches_meta(
    meta: &BTreeMap<String, String>,
    mode: RldMode,
    client: &reqwest::Client,
) -> bool {
    match mode {
        RldMode::Serve => {
            let Some(port) = parse_optional_port(meta.get("injector_port")) else {
                return false;
            };
            healthz(client, port)
                .await
                .is_some_and(|health| router_health_matches_meta(&health, meta))
        }
        RldMode::Proxy => {
            let Some(port) = parse_optional_port(meta.get("proxy_port")) else {
                return false;
            };
            healthz(client, port)
                .await
                .is_some_and(|health| proxy_health_matches_meta(&health, meta))
        }
    }
}

fn router_health_matches_meta(health: &Value, meta: &BTreeMap<String, String>) -> bool {
    if let Some(expected_router_url) = meta.get("router_url") {
        let Some(actual_router_url) = health.get("router_url").and_then(Value::as_str) else {
            return false;
        };
        if actual_router_url.trim_end_matches('/') != expected_router_url.trim_end_matches('/') {
            return false;
        }
    }
    meta.get("local_model_id").is_none_or(|expected_model_id| {
        health.get("local_model_id").and_then(Value::as_str) == Some(expected_model_id.as_str())
    })
}

fn proxy_health_matches_meta(health: &Value, meta: &BTreeMap<String, String>) -> bool {
    if let Some(expected_router_url) = meta.get("router_url") {
        let Some(actual_router_url) = health.get("router_url").and_then(Value::as_str) else {
            return false;
        };
        if actual_router_url.trim_end_matches('/') != expected_router_url.trim_end_matches('/') {
            return false;
        }
    }
    if let Some(expected_port) = parse_optional_port(meta.get("proxy_port")) {
        if health.get("proxy_port").and_then(Value::as_u64) != Some(u64::from(expected_port)) {
            return false;
        }
    }
    if let Some(expected_mode) = meta.get("proxy_routing_mode") {
        if health.get("routing_mode").and_then(Value::as_str) != Some(expected_mode.as_str()) {
            return false;
        }
    }
    if let Some(expected_available) = meta.get("local_available") {
        let expected_available = expected_available == "true";
        if health.get("local_available").and_then(Value::as_bool) != Some(expected_available) {
            return false;
        }
    }
    if let Some(expected_model_id) = meta.get("local_model_id") {
        if health.get("local_model_id").and_then(Value::as_str) != Some(expected_model_id.as_str())
        {
            return false;
        }
    }
    if let Some(expected_adapter_port) = parse_optional_port(meta.get("local_adapter_port")) {
        if health.get("local_adapter_port").and_then(Value::as_u64)
            != Some(u64::from(expected_adapter_port))
        {
            return false;
        }
    }
    if meta.get("decision_plane").map(String::as_str) == Some(DECISION_PLANE_LOCAL) {
        return true;
    }
    health.get("has_router_key").and_then(Value::as_bool) == Some(true)
}

fn process_name(argv0: &str) -> String {
    let stripped = argv0.trim_matches(|ch| ch == '"' || ch == '\'');
    stripped
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(stripped)
        .to_ascii_lowercase()
}

#[cfg(unix)]
fn process_argv(pid: i32) -> Option<Vec<String>> {
    let proc_cmdline = Path::new("/proc").join(pid.to_string()).join("cmdline");
    if let Ok(raw) = std::fs::read(&proc_cmdline) {
        let argv = raw
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).into_owned())
            .collect::<Vec<_>>();
        if !argv.is_empty() {
            return Some(argv);
        }
    }

    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if command.is_empty() {
        None
    } else {
        Some(split_command_line_best_effort(&command))
    }
}

#[cfg(windows)]
fn process_argv(pid: i32) -> Option<Vec<String>> {
    let query = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}').CommandLine");
    let output = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if command.is_empty() {
        None
    } else {
        Some(split_command_line_best_effort(&command))
    }
}

fn split_command_line_best_effort(command: &str) -> Vec<String> {
    command.split_whitespace().map(str::to_owned).collect()
}

fn clear_router_state(paths: &RouterPaths) {
    let _ = std::fs::remove_file(&paths.pid_file);
    let _ = std::fs::remove_file(&paths.meta_file);
}

fn clear_proxy_state(paths: &RouterPaths) {
    let _ = std::fs::remove_file(&paths.proxy_pid_file);
    let _ = std::fs::remove_file(&paths.proxy_meta_file);
}

/// Report the `--isolated` proxy in `router status` so users can see (and clean
/// up with `router stop`) a proxy left running in the isolated `cc/` state dir
/// on its own port. No-op when no isolated proxy is running.
async fn append_isolated_proxy_status(output: &mut String, home: &Path, client: &reqwest::Client) {
    let isolated = RouterPaths::new_isolated(home);
    if read_pid(&isolated.proxy_pid_file).is_none() {
        return;
    }
    output.push_str("--isolated:\n");
    append_proxy_sidecar_status(output, &isolated, client).await;
}

async fn append_proxy_sidecar_status(
    output: &mut String,
    paths: &RouterPaths,
    client: &reqwest::Client,
) -> bool {
    let Some(proxy_pid) = read_pid(&paths.proxy_pid_file) else {
        return false;
    };

    let meta = read_meta(&paths.proxy_meta_file);
    let proxy_port = parse_port(meta.get("proxy_port"), DEFAULT_PROXY_PORT);
    if !process_exists(proxy_pid) {
        output.push_str(&format!(
            "{} proxy: stale pidfile (pid {proxy_pid} gone). Run `{} router stop` to clean up.\n",
            daemon_name(),
            cli_name()
        ));
        return true;
    }

    output.push_str(&format!("{} proxy: pid {proxy_pid}\n", daemon_name()));
    for key in ["router_url", "proxy_port", "ca_cert_path"] {
        if let Some(value) = meta.get(key) {
            output.push_str(&format!("  {key}: {value}\n"));
        }
    }
    append_proxy_health(output, client, proxy_port).await;
    output.push_str(&format!("  log: {}\n", paths.proxy_log_file.display()));
    true
}

async fn append_proxy_health(output: &mut String, client: &reqwest::Client, proxy_port: u16) {
    if let Some(health) = healthz(client, proxy_port).await {
        output.push_str(&format!(
            "  proxy    :{proxy_port} \u{2713} ({}, local={})\n",
            value_display_or_empty(health.get("router_url")),
            value_display_title_case(health.get("local_available")),
        ));
    } else {
        output.push_str(&format!("  proxy    :{proxy_port} \u{2717} (no healthz)\n"));
    }
}

async fn healthz(client: &reqwest::Client, port: u16) -> Option<Value> {
    let response = client
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .ok()?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    response.json::<Value>().await.ok()
}

fn read_pid(path: &Path) -> Option<i32> {
    let text = std::fs::read_to_string(path).ok()?;
    text.trim().parse::<i32>().ok()
}

fn read_meta(path: &Path) -> BTreeMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some((key.trim().to_owned(), value.trim().to_owned()))
        })
        .collect()
}

fn parse_port(value: Option<&String>, default: u16) -> u16 {
    parse_optional_port(value).unwrap_or(default)
}

fn parse_optional_port(value: Option<&String>) -> Option<u16> {
    value?.parse::<u16>().ok()
}

/// Metrics-control port the proxy self-hosts on, mirroring `resolve_proxy_port`:
/// an env override (`RAYLINE_METRICS_PORT` / `RAYLINE_ISOLATED_METRICS_PORT`)
/// wins, otherwise the per-isolation default. A malformed override falls back to
/// the default — metrics are best-effort and must not block a launch.
fn resolve_metrics_port(isolated: bool) -> u16 {
    let (env_var, default_port) = if isolated {
        (
            "RAYLINE_ISOLATED_METRICS_PORT",
            DEFAULT_ISOLATED_METRICS_PORT,
        )
    } else {
        (
            "RAYLINE_METRICS_PORT",
            rayline_metrics::DEFAULT_METRICS_PORT,
        )
    };
    metrics_port_from_env(std::env::var(env_var).ok().as_deref(), default_port)
}

/// A metrics-port override's value, or `default_port` when it is unset, empty,
/// unparsable or 0. Port 0 would let the OS pick a port nobody records, so it
/// reads as unset.
fn metrics_port_from_env(value: Option<&str>, default_port: u16) -> u16 {
    match value {
        Some(value) if !value.is_empty() => value
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .unwrap_or(default_port),
        _ => default_port,
    }
}

/// Metrics-control port for a `serve` daemon: the configured port
/// (`RAYLINE_METRICS_PORT`, else the default) when it is free, else a free
/// ephemeral port. Another rld, such as a running `rayline claude` proxy, may
/// already hold the default, and a second launcher must still start (#82).
/// `data_ports` are the ports this serve binds for routing; metrics never takes
/// one, or serve's own router bind would fail.
fn choose_serve_metrics_port(data_ports: &[u16]) -> Option<u16> {
    let preferred = resolve_metrics_port(false);
    pick_metrics_port(
        preferred,
        local_port_is_free(preferred),
        data_ports,
        ephemeral_local_port,
    )
}

/// How many ephemeral ports to try before giving up on one that is not a
/// data port.
const EPHEMERAL_METRICS_PORT_ATTEMPTS: usize = 8;

/// The preferred port when it is free and not a data port; otherwise an
/// ephemeral port that is not a data port. With neither, the preferred port
/// when it is not a data port (the daemon then runs without metrics if it is
/// held, rather than failing), else `None`: no metrics port is usable.
fn pick_metrics_port(
    preferred: u16,
    preferred_is_free: bool,
    data_ports: &[u16],
    mut ephemeral: impl FnMut() -> Option<u16>,
) -> Option<u16> {
    if preferred_is_free && !data_ports.contains(&preferred) {
        return Some(preferred);
    }
    for _ in 0..EPHEMERAL_METRICS_PORT_ATTEMPTS {
        match ephemeral() {
            Some(port) if !data_ports.contains(&port) => return Some(port),
            Some(_) => continue,
            None => break,
        }
    }
    (!data_ports.contains(&preferred)).then_some(preferred)
}

fn local_port_is_free(port: u16) -> bool {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).is_ok()
}

fn ephemeral_local_port() -> Option<u16> {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .ok()
}

/// Ordered, de-duplicated list of metrics-control ports `rayline top` should try,
/// most-authoritative first: the local-router `serve` daemon, then the
/// non-isolated proxy's self-hosted server, then the isolated proxy's, with the
/// default metrics port as a final fallback. The proxy only records its
/// `metrics_port` in meta when it self-hosts (i.e. when it is not forwarding to a
/// serve daemon), so a present entry always names a port the proxy owns.
fn metrics_port_candidates(
    serve_meta: &BTreeMap<String, String>,
    proxy_meta: &BTreeMap<String, String>,
    isolated_proxy_meta: &BTreeMap<String, String>,
) -> Vec<u16> {
    let mut ports = Vec::new();
    let mut push = |port: u16| {
        if !ports.contains(&port) {
            ports.push(port);
        }
    };
    for meta in [serve_meta, proxy_meta, isolated_proxy_meta] {
        if let Some(port) = parse_optional_port(meta.get("metrics_port")) {
            push(port);
        }
    }
    push(rayline_metrics::DEFAULT_METRICS_PORT);
    ports
}

/// Probe each candidate metrics port in precedence order and return the first
/// whose snapshot endpoint answers. A stale meta entry (port recorded but the
/// proxy gone) is skipped in favour of a live server. If none respond, fall back
/// to the highest-precedence candidate so the downstream "not available" error
/// names a sensible port. `candidates` is always non-empty in practice
/// (`metrics_port_candidates` appends the default), but an empty slice degrades
/// to the shared default rather than panicking.
async fn first_reachable_metrics_port(client: &reqwest::Client, candidates: &[u16]) -> u16 {
    for &port in candidates {
        if metrics_port_is_serving(client, port).await {
            return port;
        }
    }
    candidates
        .first()
        .copied()
        .unwrap_or(rayline_metrics::DEFAULT_METRICS_PORT)
}

/// The pid of the process whose metrics-control server answers on `port`, as
/// its `/healthz` reports it; `None` when nothing answers or no pid is given.
async fn metrics_port_owner_pid(client: &reqwest::Client, port: u16) -> Option<i32> {
    let url = format!("http://127.0.0.1:{port}/healthz");
    let response = client.get(&url).send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    body.get("pid")
        .and_then(Value::as_u64)
        .and_then(|pid| i32::try_from(pid).ok())
}

/// Whether a metrics-control server is answering snapshot requests on `port`.
async fn metrics_port_is_serving(client: &reqwest::Client, port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/v1/router/top/snapshot");
    matches!(client.get(&url).send().await, Ok(response) if response.status().is_success())
}

fn value_display_or_empty(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.to_owned(),
        Some(Value::Bool(value)) => title_case_bool(*value).to_owned(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn value_display_title_case(value: Option<&Value>) -> String {
    match value {
        Some(Value::Bool(value)) => title_case_bool(*value).to_owned(),
        Some(Value::String(value)) => value.to_owned(),
        Some(Value::Number(value)) => value.to_string(),
        Some(Value::Null) | None => "None".to_owned(),
        Some(other) => other.to_string(),
    }
}

fn title_case_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

#[cfg(unix)]
fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // Mirrors POSIX kill(pid, 0): no signal is delivered; errno tells
    // us whether the process is absent or merely owned by another user.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return !process_is_zombie(pid);
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
fn process_is_zombie(pid: i32) -> bool {
    Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .output()
        .ok()
        .and_then(|output| output.status.success().then_some(output.stdout))
        .map(|stdout| String::from_utf8_lossy(&stdout).contains('Z'))
        .unwrap_or(false)
}

#[cfg(unix)]
fn terminate_process(pid: i32) -> io::Result<()> {
    signal_process(pid, libc::SIGTERM)
}

#[cfg(unix)]
fn force_stop_process_group(pid: i32) -> io::Result<()> {
    let result = unsafe { libc::killpg(pid, libc::SIGKILL) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(error)
}

#[cfg(unix)]
fn signal_process(pid: i32, signal: i32) -> io::Result<()> {
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        return Err(io::Error::new(io::ErrorKind::NotFound, error));
    }
    Err(error)
}

#[cfg(windows)]
fn process_exists(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    let query = format!("(Get-CimInstance Win32_Process -Filter 'ProcessId = {pid}').ProcessId");
    Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &query])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .is_ok_and(|output| {
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).trim() == pid.to_string()
        })
}

#[cfg(windows)]
fn terminate_process(pid: i32) -> io::Result<()> {
    // Windows has no graceful equivalent of SIGTERM for our daemon: `taskkill`
    // without `/F` only posts WM_CLOSE, which a windowless console process never
    // receives, so the command fails ("can only be terminated forcefully") and
    // the daemon survives. The daemon's only shutdown handler is for ctrl-c,
    // which taskkill does not deliver either. So terminate by force-killing the
    // process tree (`/T /F`) — the same call `force_stop_process_group` makes —
    // which also reaps child processes (e.g. llama-server). Returning an Err here
    // (as the old graceful attempt did) made the callers' escalation unreachable,
    // breaking `rayline router stop` and the daemon-restart in `rayline claude`.
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        return Ok(());
    }
    // taskkill exits 128 when the PID no longer exists; surface that as NotFound
    // so callers can report "already gone" instead of a hard error.
    if status.code() == Some(128) {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("process {pid} not found"),
        ));
    }
    Err(io::Error::other(format!(
        "taskkill failed with status {status}"
    )))
}

#[cfg(windows)]
fn force_stop_process_group(pid: i32) -> io::Result<()> {
    let status = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "taskkill /T /F failed with status {status}"
        )))
    }
}

/// Acquire an exclusive advisory lock on `lock_path`, blocking until it is
/// available.  The lock is automatically released when the returned `File`
/// is dropped (i.e. when the guard goes out of scope or the process dies).
///
/// On non-unix platforms the lock is a no-op: the function opens the file and
/// returns it as a guard so call-sites compile unmodified, but no exclusion is
/// enforced.  macOS and Linux are the supported targets; Windows support is a
/// known gap documented in the crate README.
#[cfg(unix)]
fn acquire_router_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn acquire_router_lock(lock_path: &Path) -> io::Result<std::fs::File> {
    // Non-unix stub: open the file so the guard type is consistent.
    // No actual locking — Windows is a known gap.
    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
}

/// Write `pid` and `meta` to their respective paths atomically via
/// temp-file + rename.  A partial write from a concurrent process will never
/// be visible: readers see either the old content or the complete new content,
/// never a half-written state.
fn write_pid_meta_atomic(
    pid_path: &Path,
    meta_path: &Path,
    pid: i32,
    meta: &BTreeMap<String, String>,
) -> io::Result<()> {
    // Meta first, pid last: the pid file is the existence/commit marker
    // (read_pid gates "is a daemon running"), so it only appears once the meta
    // it describes is already fully on disk — closing the pid-without-meta window.
    atomic_write(meta_path, format_meta(meta).as_bytes())?;
    atomic_write(pid_path, format!("{pid}\n").as_bytes())?;
    Ok(())
}

/// Write `content` to `dest` atomically via a sibling temp file + rename.
fn atomic_write(dest: &Path, content: &[u8]) -> io::Result<()> {
    let dir = dest
        .parent()
        .ok_or_else(|| io::Error::other("dest path has no parent"))?;
    // Create a temp file in the same directory so rename is always on the same
    // filesystem (cross-filesystem rename is not atomic on Linux).
    let tmp_path = dir.join(format!(
        ".tmp-{}-{}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("write"),
        std::process::id()
    ));
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, dest)?;
    Ok(())
}

struct RouterPaths {
    pid_file: PathBuf,
    log_file: PathBuf,
    meta_file: PathBuf,
    proxy_pid_file: PathBuf,
    proxy_log_file: PathBuf,
    proxy_meta_file: PathBuf,
    /// Advisory lock file used by `acquire_router_lock` to serialize the
    /// serve daemon's read-pid → decide → spawn → liveness window.  The lock is
    /// auto-released when the returned `File` is dropped.
    lock_file: PathBuf,
    /// Separate advisory lock for the standalone proxy start path, so a proxy
    /// launch never serializes against a serve-daemon launch.
    proxy_lock_file: PathBuf,
}

impl RouterPaths {
    fn new(home: &Path) -> Self {
        Self::in_dir(home.join(crate::ROUTER_STATE_DIR))
    }

    /// State for the `--isolated` proxy/router, kept in a separate `cc/` subdir
    /// so an isolated session never stops or reconfigures the shared proxy a
    /// normal session is using.
    fn new_isolated(home: &Path) -> Self {
        Self::in_dir(home.join(crate::ROUTER_STATE_DIR).join("cc"))
    }

    fn for_isolation(home: &Path, isolated: bool) -> Self {
        if isolated {
            Self::new_isolated(home)
        } else {
            Self::new(home)
        }
    }

    fn in_dir(data_dir: PathBuf) -> Self {
        let prefix = crate::ROUTER_FILE_PREFIX;
        Self {
            pid_file: data_dir.join(format!("{prefix}.pid")),
            log_file: data_dir.join(format!("{prefix}.log")),
            meta_file: data_dir.join(format!("{prefix}.meta")),
            proxy_pid_file: data_dir.join(format!("{prefix}-proxy.pid")),
            proxy_log_file: data_dir.join(format!("{prefix}-proxy.log")),
            proxy_meta_file: data_dir.join(format!("{prefix}-proxy.meta")),
            lock_file: data_dir.join(format!("{prefix}.lock")),
            proxy_lock_file: data_dir.join(format!("{prefix}-proxy.lock")),
        }
    }

    #[cfg(test)]
    fn temp() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let data_dir =
            std::env::temp_dir().join(format!("rayline-lock-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&data_dir).expect("could not create temp test dir");
        Self::in_dir(data_dir)
    }

    fn data_dir(&self) -> &Path {
        self.pid_file
            .parent()
            .expect("router pid path should always have a parent")
    }
}
