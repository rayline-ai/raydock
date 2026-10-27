use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::claude_daemon::{LaunchPreflight, LaunchRecord, PreflightOutcome, RequestSpec};
use serde_json::Value;
#[cfg(target_os = "macos")]
use sha2::{Digest, Sha256};

const DEFAULT_CLAUDE_SETTINGS_SUFFIX: &str = ".claude/settings.json";
const DEFAULT_MODEL: &str = "rayline-router";
const DEFAULT_PROXY_SUBAGENTS_MODEL: &str = "claude-sonnet-5";
const DEFAULT_ROUTER_KEY_NAME: &str = "rayline-cli";
/// Claude Code's `--model` / `ANTHROPIC_MODEL` aliases (each may also carry a
/// `[1m]` suffix).
const CLAUDE_CODE_MODEL_ALIASES: [&str; 7] = [
    "default", "best", "opus", "sonnet", "haiku", "fable", "opusplan",
];
const DEFAULT_LOCAL_INJECTOR_PORT: u16 = 20809;
const DEFAULT_PROXY_PORT: u16 = 20810;
/// Distinct proxy port for `--isolated` proxy-mode runs, so an isolated session
/// binds its own proxy instead of restarting the shared proxy a normal session
/// may be using. Override with `RAYLINE_ISOLATED_PROXY_PORT`.
const DEFAULT_ISOLATED_PROXY_PORT: u16 = 20812;
const NODE_CA_BUNDLE_FILENAME: &str = "node-ca-bundle.pem";
pub(crate) const ROUTING_MODE_PROXY: &str = "proxy";
pub(crate) const ROUTING_MODE_PROXY_SUBAGENTS: &str = "proxy-subagents";
const ROUTING_MODE_OVERRIDE: &str = "override";
pub(crate) const AUTO_COMPACT_WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";
/// Claude Code's switch for message threads ("tether"): server-held history
/// that later requests continue by `previous_message_id`. It turns on for
/// first-party traffic, which is what proxy mode looks like, but the router
/// holds no thread, so every continuation costs a refused round trip.
const MESSAGE_THREADS_ENV: &str = "CLAUDE_CODE_TETHER_LIVE";
pub(crate) const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";
const CLAUDE_DISABLE_AGENT_VIEW_ENV: &str = "CLAUDE_CODE_DISABLE_AGENT_VIEW";
/// Claude Code's own refusal recovery, switched off for a session whose main
/// conversation the router serves: refusals are the router's to handle. The
/// fallback resends a refused turn to another Claude model (it fires for models
/// with the `refusal_fallback` capability, e.g. a Claude id, not for a router
/// alias); the retry (2.1.282+, on by default) resends it once to the same
/// model with a nudge that stays in the history, so every refused turn hits the
/// router twice.
const CLAUDE_REFUSAL_RECOVERY_OFF_ENV: [&str; 2] = [
    "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK",
    "CLAUDE_CODE_DISABLE_REFUSAL_RETRY",
];
pub(crate) const RAYLINE_ENV_NAME_ENV: &str = "RAYLINE_ENV_NAME";
const DIAG_PROBE_TIMEOUT_SECONDS: u64 = 8;
const LEGACY_STATUSLINE_MARKERS: [&str; 2] = ["wksp-route-statusline", "rl-route-statusline"];
const SHELL_COMPOSE_OPERATORS: [&str; 7] = ["&&", "||", ";", "|", "\n", "`", "$("];
const DIAG_EXTERNAL_PROBE_HOSTS: [&str; 3] = [
    "https://api.anthropic.com/v1/models",
    "https://platform.claude.com/v1/oauth/token",
    "https://claude.ai/",
];
const DIAG_ENV_FINGERPRINT_KEYS: [&str; 24] = [
    "HTTPS_PROXY",
    "https_proxy",
    "HTTP_PROXY",
    "http_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "NODE_EXTRA_CA_CERTS",
    "NODE_USE_SYSTEM_CA",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_MODEL",
    AUTO_COMPACT_WINDOW_ENV,
    RAYLINE_ENV_NAME_ENV,
    "RAYLINE_CLAUDE_ROUTING_MODE",
    "RAYLINE_ROUTER_URL",
    "RAYLINE_ROUTER_API_KEY",
    "RAYLINE_PROXY_PORT",
    "RAYLINE_UPSTREAM_CA_FILE",
    "RUST_LOG",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_VERTEX",
    CLAUDE_CONFIG_DIR_ENV,
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub args: Vec<OsString>,
    pub model: Option<String>,
    pub local_provider: Option<crate::providers::ProviderId>,
    pub local_provider_model: Option<String>,
    pub auto_compact_window: Option<u64>,
    /// Run entirely through the local static router. This bypasses hosted
    /// router auth/settings and points the local proxy/injector at the
    /// on-device decision plane.
    pub local_router: bool,
    /// Run against an isolated Claude config dir (`~/.<brand>/cc`) so this
    /// session can start its own background-agents supervisor alongside a
    /// standard Claude Code daemon instead of conflicting with it. The dir is a
    /// thin overlay: shared content (projects, sessions, history, skills,
    /// commands, agents, CLAUDE.md) is symlinked back to the user's main
    /// `~/.claude`, settings.json is seeded as a local copy, .claude.json is
    /// mirrored from the selected source profile, and daemon/runtime state stays
    /// local so two live supervisors never collide.
    pub isolated: bool,
    pub local_injector_port: Option<u16>,
    pub routing_mode: RoutingMode,
    /// Whether the user pinned the proxy scope with `--route`. When false and
    /// local routing engages (explicit `--local` or implicit account-local), the
    /// scope falls back to the router-dependent subagents-only default. Carried
    /// here because implicit local engagement is only known at run time, after a
    /// `/v1/settings` fetch the parser cannot perform.
    pub route_scope_explicit: bool,
    pub route_statusline_enabled: bool,
    pub diagnose: bool,
    pub upstream_ca_path: Option<PathBuf>,
    pub router_config_path: Option<PathBuf>,
    /// v2: a `RouterConfig` file (`--config`) that drives BOTH the main agent and
    /// subagents via the on-device router, without `--local`. Engages the local
    /// plane; the proxy scope is derived from `routes.main` (passthrough sentinel
    /// → subagents-only, else route-all). Distinct from `router_config_path`.
    pub config_path: Option<PathBuf>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoutingMode {
    Override,
    Proxy,
    ProxySubagents,
}

pub(crate) fn is_proxy_routing_mode(mode: RoutingMode) -> bool {
    matches!(mode, RoutingMode::Proxy | RoutingMode::ProxySubagents)
}

/// Whether an isolated session in this routing mode needs its own Claude Code
/// OAuth login. Env mode (`Override`) authenticates Claude Code via
/// `ANTHROPIC_BASE_URL`/`ANTHROPIC_AUTH_TOKEN` env vars, so it does not; the
/// proxy modes route through Claude's own credential and do.
fn isolated_needs_claude_login(mode: RoutingMode) -> bool {
    mode != RoutingMode::Override
}

fn default_model_for_routing_mode(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::ProxySubagents => DEFAULT_PROXY_SUBAGENTS_MODEL,
        RoutingMode::Override | RoutingMode::Proxy => DEFAULT_MODEL,
    }
}

/// Whether *implicit* account-local routing (the hosted `enable_local_router`
/// toggle + an on-device config) should engage for this run.
///
/// Whether the account-toggle *implicit* local path should engage. This is the
/// "no `--config`, no `--local`" fallback that turns on may-local purely from the
/// account toggle + a configured local model.
///
/// - `config_present`: a `--config` file is in play. When true, the config is the
///   sole driver of may-local (a `local_models` route → the Rcl-Rcl/S-Rcl arm), so the
///   implicit path is suppressed — e.g. a cloud-only `Rc-Rc` (`--local-model=off`)
///   stays pure cloud even with `raydock local on` + a local model configured.
/// - Env (`Override`) mode is cloud-only by contract (sets `ANTHROPIC_BASE_URL`
///   directly, can't reach local), so it never engages.
/// - `--isolated` opts out; an isolated local session must be requested with `--local`.
/// - Does not apply to explicit `--local` (handled upstream).
fn implicit_local_engages(
    mode: RoutingMode,
    isolated: bool,
    toggle_on: bool,
    config_present: bool,
) -> bool {
    !config_present && !matches!(mode, RoutingMode::Override) && !isolated && toggle_on
}

/// The routing mode after accounting for local engagement.
///
/// When local routing is engaged (explicit `--local` or implicit account-local)
/// and the user did not pin `--route`, the scope defaults to subagents-only —
/// the hybrid default where the main agent stays on cloud Claude and only
/// subagents are offloaded on-device. An explicit `--route` (which makes parse
/// time resolve to `Proxy` for route-all) is always respected.
fn effective_routing_mode(
    parsed: RoutingMode,
    local_engaged: bool,
    route_scope_explicit: bool,
) -> RoutingMode {
    if local_engaged && !route_scope_explicit && parsed == RoutingMode::Proxy {
        RoutingMode::ProxySubagents
    } else {
        parsed
    }
}

impl RunRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        crate::status::should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

#[derive(Debug)]
pub enum RunError {
    ClaudeMissing,
    HomeNotFound,
    DaemonConflict(String),
    MissingRouterKey(String),
    Router(String),
    UnknownEnvironment(String),
    HostedEnvironment(String),
    Auth(crate::status::AuthTokenError),
    Login(String),
    KeyProvision(String),
    NotLoggedIn(String),
}

impl std::fmt::Display for RunError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClaudeMissing => formatter.write_str(
                "`claude` not found on PATH. Install Claude Code: https://docs.claude.com/en/docs/claude-code/setup",
            ),
            Self::HomeNotFound => formatter.write_str("home directory not found"),
            Self::DaemonConflict(message) => formatter.write_str(message),
            Self::MissingRouterKey(env_name) => write!(
                formatter,
                "No {} router key stored for {env_name}. Run: {} auth login",
                crate::DISPLAY_NAME,
                crate::CLI_BIN
            ),
            Self::Router(message) => formatter.write_str(message),
            Self::UnknownEnvironment(env_name) => {
                write!(formatter, "Unknown environment for router: {env_name}")
            }
            Self::HostedEnvironment(message) => formatter.write_str(message),
            Self::Auth(error) => error.fmt(formatter),
            Self::Login(message) => formatter.write_str(message),
            Self::KeyProvision(message) => formatter.write_str(message),
            Self::NotLoggedIn(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for RunError {}

impl From<crate::status::AuthTokenError> for RunError {
    fn from(error: crate::status::AuthTokenError) -> Self {
        Self::Auth(error)
    }
}

async fn explicit_local_router_start_request(
    env_name: &str,
    request: &RunRequest,
    local_cfg: Option<&crate::local_model::LocalModelConfig>,
    home: &Path,
) -> Result<crate::router::RouterStartRequest, RunError> {
    let injector_port = resolve_injector_port(request.local_injector_port)?;
    let mut start_request =
        crate::router::RouterStartRequest::local_router_defaults(request.root_env_explicit);
    start_request.env_name = Some(env_name.to_owned());
    start_request.injector_port = injector_port;
    start_request.router_config_path = request.router_config_path.clone();

    // Default `--local` (subagents scope) routes only the conservative read-only
    // allowlist; materialize it as the proxy's selective config when the user
    // didn't supply their own. `--route all` (RoutingMode::Proxy) skips this and
    // routes all subagents to local.
    if start_request.router_config_path.is_none()
        && request.routing_mode == RoutingMode::ProxySubagents
    {
        start_request.router_config_path = Some(
            crate::onboarding::write_default_local_routes(home).map_err(|error| {
                RunError::Router(format!("failed to write default local routes: {error}"))
            })?,
        );
    }

    let cfg = local_cfg.filter(|cfg| cfg.is_engageable()).ok_or_else(|| {
        RunError::Router(format!(
            "No local model configured. Run `{} local onboard`.",
            crate::CLI_BIN
        ))
    })?;
    Ok(crate::router::RouterStartRequest::from_local_model(
        cfg,
        start_request,
    ))
}

async fn explicit_provider_config(
    request: &RunRequest,
    home: &Path,
) -> Result<Option<(crate::local_model::LocalModelConfig, PathBuf)>, RunError> {
    let Some(provider) = request.local_provider else {
        return Ok(None);
    };
    if provider == crate::providers::ProviderId::LlamaCpp {
        return Ok(None);
    }

    let endpoint = crate::providers::provider_endpoint(provider).map_err(RunError::Router)?;
    let endpoint = endpoint.ok_or_else(|| {
        RunError::Router(format!("{} has no provider endpoint", provider.label()))
    })?;
    let models = crate::providers::list_models_at(&endpoint)
        .await
        .map_err(|error| provider_unavailable_error(provider, &endpoint.base_url, &error))?;
    let model = resolve_provider_model(
        provider,
        &endpoint.base_url,
        &models,
        request.local_provider_model.as_deref(),
    )?;
    let cfg = crate::local_model::set_provider_endpoint_in_home(
        home,
        provider.as_str(),
        &endpoint.base_url,
        &model,
        "openai_chat",
    )
    .map_err(RunError::Router)?;
    let routes = crate::providers::write_provider_routes_for_config(
        home,
        provider,
        &crate::providers::provider_openai_base(&endpoint),
        &model,
        request.router_config_path.as_deref(),
    )
    .map_err(|error| RunError::Router(format!("failed to write provider routes: {error}")))?;
    Ok(Some((cfg, routes)))
}

async fn explicit_llamacpp_config(
    home: &Path,
    env_name: &str,
) -> Result<crate::local_model::LocalModelConfig, RunError> {
    if let Some(cfg) = crate::local_model::read_from_home(home).and_then(coerce_llamacpp_config) {
        return Ok(cfg);
    }
    if let Some(model) = crate::catalog::auto_select_downloaded(env_name).await {
        return crate::local_model::set_recommended_in_home(home, &model).map_err(|error| {
            RunError::Router(format!("failed to save built-in llama.cpp model: {error}"))
        });
    }
    Err(RunError::Router(format!(
        "No built-in llama.cpp model configured for `{cli} claude --local-provider llamacpp`. Run `{cli} local use <model-id>` first, or use `{cli} local models` to pick one.",
        cli = crate::CLI_BIN,
    )))
}

fn coerce_llamacpp_config(
    mut cfg: crate::local_model::LocalModelConfig,
) -> Option<crate::local_model::LocalModelConfig> {
    if !cfg.has_recommended_pick() {
        return None;
    }
    cfg.mode = crate::local_model::LocalModelMode::Recommended;
    cfg.provider = Some("llamacpp".to_owned());
    cfg.protocol = Some("anthropic_messages".to_owned());
    Some(cfg)
}

async fn provider_routes_for_config(
    home: &Path,
    cfg: &crate::local_model::LocalModelConfig,
    explicit_config_path: Option<&Path>,
) -> Result<Option<PathBuf>, RunError> {
    let Some(provider) = crate::providers::provider_from_local_config(cfg) else {
        return Ok(None);
    };
    let base_url = cfg
        .base_url
        .as_deref()
        .ok_or_else(|| RunError::Router("provider config is missing base_url".to_owned()))?;
    let model = cfg
        .model
        .as_deref()
        .ok_or_else(|| RunError::Router("provider config is missing model".to_owned()))?;
    let endpoint = crate::providers::explicit_provider_endpoint(provider, base_url)
        .map_err(RunError::Router)?;
    let models = crate::providers::list_models_at(&endpoint)
        .await
        .map_err(|error| provider_unavailable_error(provider, &endpoint.base_url, &error))?;
    if !models.iter().any(|candidate| candidate.model == model) {
        return Err(RunError::Router(format!(
            "{} is running at {}, but model `{model}` was not listed. Pick another model with `{} claude --local --local-provider {provider_name}`.",
            provider.label(),
            endpoint.base_url,
            crate::CLI_BIN,
            provider_name = provider.as_str(),
        )));
    }
    crate::providers::write_provider_routes_for_config(
        home,
        provider,
        &crate::providers::provider_openai_base(&endpoint),
        model,
        explicit_config_path,
    )
    .map(Some)
    .map_err(|error| RunError::Router(format!("failed to write provider routes: {error}")))
}

fn provider_config_cloud_fallback(
    request_local_router: bool,
    routing_mode: RoutingMode,
    isolated: bool,
    local_cfg: Option<&crate::local_model::LocalModelConfig>,
) -> bool {
    !request_local_router
        && is_proxy_routing_mode(routing_mode)
        && !isolated
        && local_cfg.is_some_and(|cfg| crate::providers::provider_from_local_config(cfg).is_some())
}

fn provider_unavailable_error(
    provider: crate::providers::ProviderId,
    base_url: &str,
    detail: &str,
) -> RunError {
    let hint = provider
        .start_hint()
        .map(|hint| format!(" Start it with: {hint}"))
        .unwrap_or_default();
    RunError::Router(format!(
        "{} isn't running at {base_url}.{hint}\nProbe failed: {detail}",
        provider.label()
    ))
}

fn resolve_provider_model(
    provider: crate::providers::ProviderId,
    base_url: &str,
    models: &[crate::providers::ProviderModel],
    requested: Option<&str>,
) -> Result<String, RunError> {
    if let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) {
        if models.iter().any(|model| model.model == requested) {
            return Ok(requested.to_owned());
        }
        let available = models
            .iter()
            .map(|model| model.model.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(RunError::Router(format!(
            "{} at {base_url} did not list model `{requested}`. Available models: {available}",
            provider.label(),
        )));
    }

    if models.is_empty() {
        return Err(RunError::Router(format!(
            "{} is running at {base_url}, but it did not list any models.",
            provider.label()
        )));
    }
    if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
        return Err(RunError::Router(format!(
            "No provider model specified. Run `{} claude --local --local-provider {} --model <MODEL>`.",
            crate::CLI_BIN,
            provider.as_str()
        )));
    }

    eprintln!("{} · {base_url}", provider.label());
    for (index, model) in models.iter().enumerate() {
        let size = model
            .size_bytes
            .map(crate::catalog::format_bytes)
            .unwrap_or_else(|| "external".to_owned());
        eprintln!("  {:>3}  {:<32}  {size}", index + 1, model.model);
    }
    eprint!("Model number or name — Enter to cancel › ");
    io::stderr().flush().ok();
    let mut input = String::new();
    io::stdin()
        .read_line(&mut input)
        .map_err(|error| RunError::Router(format!("failed to read provider selection: {error}")))?;
    let token = input.trim();
    if token.is_empty() {
        return Err(RunError::Router("No provider model chosen.".to_owned()));
    }
    if let Ok(index) = token.parse::<usize>() {
        if (1..=models.len()).contains(&index) {
            return Ok(models[index - 1].model.clone());
        }
    }
    models
        .iter()
        .find(|model| model.model == token)
        .map(|model| model.model.clone())
        .ok_or_else(|| RunError::Router(format!("Unknown provider model `{token}`.")))
}

pub async fn run_command(request: &RunRequest) -> Result<Command, RunError> {
    let home = dirs::home_dir().ok_or(RunError::HomeNotFound)?;
    let claude_bin = find_claude_bin(&home).ok_or(RunError::ClaudeMissing)?;
    run_command_from_home(request, &home, claude_bin).await
}

/// The print-mode flag (`-p` / `--print`) in the forwarded Claude args, if any.
/// Print mode suppresses Claude Code's own startup output and is
/// non-interactive. Returns the exact flag the user passed, so messaging can
/// name it.
pub fn print_mode_flag(args: &[OsString]) -> Option<&'static str> {
    args.iter().find_map(|arg| {
        if arg == "-p" {
            Some("-p")
        } else if arg == "--print" {
            Some("--print")
        } else {
            None
        }
    })
}

async fn run_command_from_home(
    request: &RunRequest,
    home: &Path,
    claude_bin: PathBuf,
) -> Result<Command, RunError> {
    let env_name = crate::status::resolve_env(request.env_name.as_deref(), Some(home));

    // v2 config-driven routing: `--config <file>` drives BOTH main + subagents via
    // the on-device router, without `--local`. Engage the local plane when the
    // config routes something the hosted cloud router cannot serve on its own (a
    // `"local"`/custom/direct-Anthropic endpoint). `--via env` (Override) is
    // cloud-only and never engages local (already rejected with `--config` at parse
    // time). `--local` keeps its own config path separate.
    let effective_config = if request.local_router {
        None
    } else {
        request.config_path.clone()
    };
    let config_engages_local = request.routing_mode != RoutingMode::Override
        && effective_config
            .as_deref()
            .map(crate::router_config::config_needs_local_router)
            .unwrap_or(false);
    // Whether `routes.main` is the passthrough sentinel (or absent) → main stays on
    // the caller's subscription (proxy selective-subagents) instead of being routed.
    let config_main_passthrough = effective_config
        .as_deref()
        .map(crate::router_config::config_main_is_passthrough)
        .unwrap_or(false);
    // `--config` whose routes stay entirely on the hosted cloud router: no on-device
    // router, but the proxy still gets the config (subagent allowlist) and the scope
    // is derived from `routes.main`. Behaves like a cloud `raydock claude` with the
    // config's per-type subagent allowlist applied.
    let config_cloud_only = effective_config.is_some()
        && request.routing_mode != RoutingMode::Override
        && !config_engages_local;
    // Rcl-Rcl (may-local): a cloud-only config whose `rayline-cloud` route declares
    // `local_models`. Routing stays on the hosted cloud router (RCR), but a custom
    // adapter fronts the config's local endpoint so the RCR may 307-redirect a turn
    // to it — the same shape as the account-toggle may-local, driven by config. The
    // RCR makes the redirect decision (account-gated), so without it this behaves
    // like Rc-Rc. `N/A` under `--via env` (cloud-only override) and for local-plane
    // configs.
    let config_may_local = if config_cloud_only {
        effective_config
            .as_deref()
            .and_then(crate::router_config::config_may_local)
    } else {
        None
    };
    let local_plane = request.local_router || config_engages_local;

    if config_engages_local {
        if let Some(path) = effective_config.as_deref() {
            eprintln!("Routing via config: {}", path.display());
        }
    }

    let hosted = if local_plane {
        None
    } else {
        Some(
            crate::status::resolve_hosted_environment(&env_name, Some(home))
                .map_err(|error| RunError::HostedEnvironment(error.to_string()))?,
        )
    };
    let router_url = router_url_for_run(hosted.as_ref(), local_plane)?;
    let key = if local_plane {
        "rayline-local".to_owned()
    } else {
        ensure_router_key(
            &env_name,
            home,
            request.auth_token.as_deref(),
            request.root_env_explicit,
        )
        .await?
    };

    // `/v1/settings` fetch for the account `enable_local_router` toggle. Skip it
    // when this machine has no `local_model` config (so the toggle is moot —
    // even an incomplete config needs the toggle, to decide whether to warn).
    let local_cfg = crate::local_model::read_from_home(home);
    let need_settings = !local_plane && local_cfg.is_some();
    let settings = if need_settings {
        fetch_router_settings(&env_name, &router_url, request.auth_token.as_deref(), &key).await
    } else {
        None
    };

    // Explicit `--local` branch (request.local_router == true): runs first-run
    // onboarding via `ensure_local_model`, which may download the chosen model.
    // Returns a hard error (`NotConfigured → Err`) when no model is configured
    // so the user gets an actionable message instead of silently falling back.
    //
    // Implicit account-local branch (else): engages when a `local_model` config
    // exists, the account toggle is on, and the configured model is usable. This
    // implicit path stays off under `--isolated` and under env (`Override`) mode,
    // which is cloud-only by contract — see `implicit_local_engages`. Explicit
    // `--local-router --isolated` takes the local path above and uses an isolated
    // proxy sidecar. A failed settings fetch defaults the toggle to false (stay
    // cloud). The model always comes from the config (recommended pick, or the
    // user's custom endpoint); when none is picked yet, the best
    // already-downloaded curated model is adopted as the pick — this implicit
    // path never downloads anything, and an unusable config warns + launches with
    // cloud routing instead of blocking.
    let enable_local_router = settings
        .as_ref()
        .map(enable_local_router_from_router_settings)
        .unwrap_or(false);
    let provider_cloud_fallback = provider_config_cloud_fallback(
        request.local_router,
        request.routing_mode,
        request.isolated,
        local_cfg.as_ref(),
    );
    let local_start_request = if config_engages_local {
        // Config-driven (no `--local`): build a local-router start request straight
        // from the `--config` file. The local router reads `endpoints` + `routes`
        // (incl. per-subagent-type) directly; we only resolve the key and decide
        // whether a bundled local model is needed.
        let path = effective_config
            .clone()
            .expect("config_engages_local implies an effective config path");
        let injector_port = resolve_injector_port(request.local_injector_port)?;
        let mut start_request =
            crate::router::RouterStartRequest::local_router_defaults(request.root_env_explicit);
        start_request.env_name = Some(env_name.clone());
        start_request.injector_port = injector_port;
        // Hand the local router a config it can load: a passthrough `routes.main`
        // (subscription) is stripped (the proxy handles main); otherwise verbatim.
        start_request.router_config_path = Some(
            crate::router_config::materialize_for_local_router(&path, home).map_err(|error| {
                RunError::Router(format!("failed to prepare config for the router: {error}"))
            })?,
        );
        // Reuse `raydock auth login` for a hosted cloud-router endpoint — inject the
        // stored key so its `api_key_env` resolves with no manual env var. Best
        // effort: the daemon surfaces a clear error only if a cloud route is hit.
        if crate::router_config::config_uses_cloud_router(&path) {
            start_request.router_api_key_override = ensure_router_key(
                &env_name,
                home,
                request.auth_token.as_deref(),
                request.root_env_explicit,
            )
            .await
            .ok();
        }
        // A `"local"` route needs the bundled model; named endpoints (ollama,
        // direct-anthropic, rayline-cloud) are served by the local router directly.
        if crate::router_config::config_uses_local_endpoint(&path) {
            let cfg = local_cfg
                .as_ref()
                .filter(|cfg| cfg.is_engageable())
                .ok_or_else(|| {
                    RunError::Router(format!(
                        "Config routes to \"local\", but no local model is configured. Run `{cli} local use <model-id>` or `{cli} local custom ...`, or point that route at a named endpoint.",
                        cli = crate::CLI_BIN,
                    ))
                })?;
            start_request = crate::router::RouterStartRequest::from_local_model(cfg, start_request);
        } else {
            start_request.no_local_model = true;
        }
        Some(start_request)
    } else if let Some(may_local) = config_may_local.clone() {
        // Rcl-Rcl: keep routing on the hosted cloud router, but stand up a custom-mode
        // adapter fronting the config's local endpoint and advertise it, so the RCR
        // can 307-redirect to it (may-local). Mirrors the proven account-toggle
        // may-local path (`defaults()` keeps the hosted decision plane + cloud
        // `router_url`; `from_local_model`-style Custom fields point the adapter at
        // the upstream), sourced from `local_models` instead of `raydock local on`.
        eprintln!(
            "Routing via config: {} (may-local → {})",
            effective_config
                .as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_default(),
            may_local.model,
        );
        let injector_port = resolve_injector_port(request.local_injector_port)?;
        let mut start_request =
            crate::router::RouterStartRequest::defaults(request.root_env_explicit);
        start_request.env_name = Some(env_name.clone());
        start_request.router_url = router_url.to_owned();
        start_request.router_url_explicit = true;
        start_request.injector_port = injector_port;
        // Cloud-router endpoint: inject the stored `raydock auth login` key so the
        // embedded proxy authenticates to the RCR without a manual env var.
        start_request.router_api_key_override = ensure_router_key(
            &env_name,
            home,
            request.auth_token.as_deref(),
            request.root_env_explicit,
        )
        .await
        .ok();
        // Custom adapter fronting the config's local endpoint (the redirect target).
        start_request.upstream_url = Some(may_local.upstream_url);
        start_request.upstream_model = Some(may_local.model.clone());
        start_request.local_model_id = may_local.model;
        Some(start_request)
    } else if request.local_router {
        let provider_config = explicit_provider_config(request, home).await?;
        let (cfg, provider_routes_path) = if let Some((cfg, routes)) = provider_config {
            (cfg, Some(routes))
        } else if request.local_provider == Some(crate::providers::ProviderId::LlamaCpp) {
            (explicit_llamacpp_config(home, &env_name).await?, None)
        } else {
            let cfg = match crate::onboarding::ensure_local_model(home, &env_name)
                .await
                .map_err(|error| RunError::Router(error.to_string()))?
            {
                crate::onboarding::LocalModelReadiness::Ready(cfg) => *cfg,
                crate::onboarding::LocalModelReadiness::NotConfigured => {
                    return Err(RunError::Router(format!(
                        "No local model configured for `{cli} claude --local`. Run `{cli} local onboard` to set one up, or run `{cli} claude` for cloud routing.",
                        cli = crate::CLI_BIN,
                    )));
                }
            };
            let routes =
                provider_routes_for_config(home, &cfg, request.router_config_path.as_deref())
                    .await?;
            (cfg, routes)
        };
        let mut start_request =
            explicit_local_router_start_request(&env_name, request, Some(&cfg), home).await?;
        if let Some(path) = provider_routes_path {
            start_request.router_config_path = Some(path);
        }
        Some(start_request)
    } else {
        match local_cfg {
            Some(cfg)
                if implicit_local_engages(
                    request.routing_mode,
                    request.isolated,
                    enable_local_router,
                    effective_config.is_some(),
                ) =>
            {
                if crate::providers::provider_from_local_config(&cfg).is_some() {
                    eprintln!(
                        "Warning: local routing is enabled, but the configured provider endpoint requires `{cli} claude --local`. Continuing with cloud routing.",
                        cli = crate::CLI_BIN,
                    );
                    None
                } else {
                    match resolve_engageable_local_config(home, &env_name, cfg).await {
                        Some(cfg) => {
                            let injector_port = resolve_injector_port(request.local_injector_port)?;
                            let mut start_request = crate::router::RouterStartRequest::defaults(
                                request.root_env_explicit,
                            );
                            start_request.env_name = Some(env_name.clone());
                            start_request.router_url = router_url.to_owned();
                            start_request.router_url_explicit = true;
                            start_request.injector_port = injector_port;
                            Some(crate::router::RouterStartRequest::from_local_model(
                                &cfg,
                                start_request,
                            ))
                        }
                        None => None, // warning already printed; stay cloud
                    }
                }
            }
            _ => None,
        }
    };
    let requested_local_port = local_start_request
        .as_ref()
        .map(|start_request| start_request.injector_port);

    // Correct the parse-time routing mode now that local engagement is known.
    // The parser resolves bare cloud `raydock claude` to route-all `Proxy`, but
    // when account-local routing engages without an explicit `--route` the scope
    // must fall back to the hybrid subagents-only default (main agent on cloud
    // Claude, subagents on-device) — the same default as explicit `--local`.
    // Shadow `request` with the corrected mode so every downstream consumer
    // (model default, proxy wiring, status line) sees a single coherent value.
    let local_engaged = local_start_request.is_some();
    let request = &RunRequest {
        // Config-driven routing derives scope from `routes.main`: a passthrough
        // (subscription / absent) main stays on the caller's credential
        // (selective-subagents), otherwise route ALL so `routes.main` governs the
        // main thread. Applies to both local-plane and cloud-only configs.
        // Non-config runs use the normal local-engagement default.
        routing_mode: if config_engages_local || config_cloud_only {
            if config_main_passthrough {
                RoutingMode::ProxySubagents
            } else {
                RoutingMode::Proxy
            }
        } else {
            effective_routing_mode(
                request.routing_mode,
                local_engaged,
                request.route_scope_explicit,
            )
        },
        // Cloud-only `--config`: hand the config to the proxy so its subagent
        // allowlist applies. (Local-plane configs pass it via the local router's
        // own `router_config_path`, set on the start request above.)
        router_config_path: if config_cloud_only {
            effective_config.clone()
        } else {
            request.router_config_path.clone()
        },
        ..request.clone()
    };

    let inherited_anthropic_model = env::var_os("ANTHROPIC_MODEL").is_some();
    let model = request
        .model
        .clone()
        .or_else(|| env::var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| default_model_for_routing_mode(request.routing_mode).to_owned());
    let set_model_env = should_set_model_env(
        request.routing_mode,
        request.model.is_some(),
        inherited_anthropic_model,
    );
    // Config-driven route-all pins the main thread to the virtual model so the
    // local router's `routes.main` governs it (a concrete model name would hit
    // direct-model routing instead). An explicit `--model` still wins. Skipped for
    // a passthrough main, which stays on the caller's own model.
    let (model, set_model_env) =
        if config_engages_local && !config_main_passthrough && request.model.is_none() {
            ("rayline-router".to_owned(), true)
        } else {
            (model, set_model_env)
        };
    // An explicit flag/env wins. Otherwise Claude Code sizes the session itself
    // unless the hosted router knows a window for a launch model Claude Code
    // doesn't (`rayline-router`, an ARC id, another provider's model). A Claude
    // Code alias (`opus`, `sonnet[1m]`, ...) is Claude Code's own, so it skips the
    // lookup. The local plane has no hosted router and no served window to report.
    let auto_compact_window = match explicit_auto_compact_window(request) {
        Some(window) => Some(window),
        None if local_plane || is_claude_code_model_alias(&model) => None,
        None => {
            router_auto_compact_window(
                &env_name,
                &router_url,
                request.auth_token.as_deref(),
                &key,
                &model,
            )
            .await?
        }
    };

    // `--isolated` (or choosing `[i]` at the conflict prompt) targets a private
    // config dir and, in proxy mode, a private proxy port. Resolve both per the
    // isolation state and inspect the matching config dir for a daemon conflict.
    let mut isolated = request.isolated;
    let mut requested_proxy_port = if is_proxy_routing_mode(request.routing_mode) {
        Some(resolve_proxy_port(isolated)?)
    } else {
        None
    };
    let mut inspect_dir = claude_config_dir(home, isolated);
    let refusal_recovery = launch_env_switches(request.routing_mode, |key| env::var_os(key));
    let mut daemon_request = RequestSpec {
        env_name: &env_name,
        routing_mode: request.routing_mode,
        auto_compact_window: auto_compact_window.as_deref(),
        refusal_recovery: &refusal_recovery,
        args: &request.args,
        requested_local_port,
        requested_proxy_port,
    };
    let preflight = LaunchPreflight {
        home,
        config_dir: &inspect_dir,
        request: &daemon_request,
        claude_bin: &claude_bin,
        // Do not switch an implicit account-local run into isolation from the
        // conflict prompt. Users who want that shape should request it directly
        // with `--local-router --isolated` so the isolated proxy sidecar is
        // configured deliberately.
        allow_isolated: !isolated && local_start_request.is_none(),
    }
    .resolve()?;
    let preserve_spawned_by_pid = match preflight {
        PreflightOutcome::Proceed(preflight) => preflight.preserve_spawned_by_pid,
        PreflightOutcome::SwitchToIsolated => {
            // The user chose to run isolated: re-resolve the proxy port and re-check
            // the isolated config dir, which may already host a daemon for a
            // different env/mode. No further isolated escape from here.
            isolated = true;
            requested_proxy_port = if is_proxy_routing_mode(request.routing_mode) {
                Some(resolve_proxy_port(true)?)
            } else {
                None
            };
            inspect_dir = claude_config_dir(home, true);
            daemon_request.requested_proxy_port = requested_proxy_port;
            let isolated_preflight = LaunchPreflight {
                home,
                config_dir: &inspect_dir,
                request: &daemon_request,
                claude_bin: &claude_bin,
                allow_isolated: false,
            }
            .resolve()?;
            match isolated_preflight {
                PreflightOutcome::Proceed(preflight) => preflight.preserve_spawned_by_pid,
                PreflightOutcome::SwitchToIsolated => {
                    unreachable!("isolated re-check cannot isolate")
                }
            }
        }
    };
    if request.diagnose {
        diag_print_preamble(&env_name, &router_url, request.routing_mode, home).await;
    }

    let mut command = claude_command(&claude_bin);
    add_claude_bin_dir_to_child_path(&mut command, &claude_bin);
    let args = if request.diagnose {
        inject_claude_debug(&request.args)
    } else {
        request.args.clone()
    };
    command.args(args);
    // Apply the isolated overlay (and CLAUDE_CONFIG_DIR) before proxy/statusline
    // config so those target the isolated settings.json and proxy, not the shared
    // ones under ~/.claude.
    if isolated {
        let print_flag = print_mode_flag(&request.args);
        apply_isolated_config_dir(
            &mut command,
            home,
            &claude_bin,
            print_flag,
            isolated_needs_claude_login(request.routing_mode),
        )?;
    }
    match request.routing_mode {
        RoutingMode::Override => {
            configure_override_env(
                &mut command,
                local_start_request.as_ref(),
                home,
                &env_name,
                router_url.as_str(),
                &key,
                &model,
                auto_compact_window.as_deref(),
            )
            .await?;
        }
        RoutingMode::Proxy | RoutingMode::ProxySubagents => {
            // A prior local session may have left the shared `rld serve` daemon
            // running with its model loaded — wasting RAM/GPU and holding the
            // shared proxy port this non-isolated cloud launch is about to
            // rebind (`configure_proxy_env` → `start_proxy_from_home`). Stop it
            // so the replacement proxy can take the port. This is allowed only
            // when this launch is definitively cloud-only for the shared proxy:
            // either the account toggle is confirmed off, or the toggle is on
            // but the saved local provider requires explicit `--local` and this
            // run already warned that it is falling back to cloud routing.
            // Gated to non-isolated proxy mode — `--isolated` uses a private
            // port and override mode starts no proxy. The helper further
            // restricts the stop to a daemon that actually owns this proxy port.
            // Placed after the conflict check commits, so a cancelled launch
            // never tears the daemon down without a successor; and we preflight
            // the daemon binary the replacement proxy needs (`resolve_rld_bin`)
            // so a launch that could not start its own proxy never performs the
            // destructive stop and strands the session.
            let cloud_launch_replaces_local_proxy =
                (settings.is_some() && !enable_local_router) || provider_cloud_fallback;
            if local_start_request.is_none()
                && !isolated
                && cloud_launch_replaces_local_proxy
                && crate::router::resolve_rld_bin(home).is_ok()
            {
                let proxy_port = resolve_proxy_port(false)?;
                match crate::router::stop_serve_daemon_from_home(home, proxy_port).await {
                    Ok(true) => eprintln!("Local routing is off — stopped the on-device model."),
                    Ok(false) => {}
                    // Best-effort: a failed teardown must not block the launch.
                    Err(error) => {
                        eprintln!("Warning: could not stop the on-device model: {error}.")
                    }
                }
            }
            configure_proxy_env(
                &mut command,
                request,
                local_start_request.as_ref(),
                home,
                &env_name,
                router_url.as_str(),
                &key,
                set_model_env.then_some(model.as_str()),
                auto_compact_window.as_deref(),
                isolated,
            )
            .await?;
            configure_route_statusline(home, isolated, request.route_statusline_enabled);
        }
    }
    apply_refusal_recovery_env(&mut command, &refusal_recovery);
    if request.diagnose {
        diag_print_postamble_for_mode(request.routing_mode, &router_url, isolated, home).await;
    }
    if let Some(caller_cwd) = env::var_os("RAYLINE_CALLER_CWD") {
        let caller_cwd = PathBuf::from(caller_cwd);
        if caller_cwd.is_dir() {
            command.current_dir(caller_cwd);
        }
    }

    crate::claude_daemon::record_rayline_claude_launch(LaunchRecord {
        home,
        pid: std::process::id(),
        request: &daemon_request,
        preserve_spawned_by_pid,
    });
    Ok(command)
}

fn router_url_for_run(
    hosted: Option<&crate::status::HostedEnvironment>,
    local_router: bool,
) -> Result<String, RunError> {
    if local_router {
        return Ok(format!(
            "http://127.0.0.1:{}",
            crate::router::DEFAULT_LOCAL_ROUTER_PORT
        ));
    }
    hosted
        .map(|hosted| hosted.router_url.clone())
        .ok_or_else(|| RunError::UnknownEnvironment("missing hosted environment".to_owned()))
}

/// Resolve the router key for `env_name`, provisioning it on demand.
///
/// Walks the onboarding ladder so a bare `claude` run is all a new user needs:
/// if no key is stored we mint one, signing in first
/// when the user is not yet authenticated (the `auth login` step). Interactive
/// sign-in only fires on a real terminal; non-interactive callers keep the
/// previous hard error instead of blocking on a browser.
/// Provision (mint or read) the hosted-RCR router key (`rlk-`) for `env_name`.
///
/// Shared by the Claude run path and the Codex run/app paths (see
/// [`crate::codex::resolve_cloud_router_key`]) so a config whose route targets a
/// `rayline-cloud` endpoint authenticates from a plain `raydock auth login` with
/// no manual `RAYLINE_ROUTER_API_KEY`.
pub(crate) async fn ensure_router_key(
    env_name: &str,
    home: &Path,
    auth_token: Option<&str>,
    root_env_explicit: bool,
) -> Result<String, RunError> {
    if let Some(token) = auth_token.filter(|token| !token.is_empty()) {
        eprintln!("Provisioning {} router key...", crate::DISPLAY_NAME);
        return crate::status::provision_router_key(env_name, home, DEFAULT_ROUTER_KEY_NAME, token)
            .await
            .map_err(|error| RunError::KeyProvision(error.to_string()));
    }

    if let Some(key) = crate::status::load_claude_key_from_home(env_name, home) {
        return Ok(key);
    }

    let Some(token) = resolve_auth_token_or_login(env_name, auth_token, root_env_explicit).await?
    else {
        return Err(RunError::MissingRouterKey(env_name.to_owned()));
    };

    eprintln!("Provisioning {} router key...", crate::DISPLAY_NAME);
    crate::status::provision_router_key(env_name, home, DEFAULT_ROUTER_KEY_NAME, &token)
        .await
        .map_err(|error| RunError::KeyProvision(error.to_string()))
}

/// Return an account bearer token for `env_name`, launching the interactive
/// sign-in flow when no usable credentials are stored. Returns `Ok(None)` when
/// no token is available and we cannot prompt (non-interactive session), so the
/// caller can fall back to the existing "not logged in" error.
async fn resolve_auth_token_or_login(
    env_name: &str,
    auth_token: Option<&str>,
    root_env_explicit: bool,
) -> Result<Option<String>, RunError> {
    let token_request = crate::status::AuthTokenRequest {
        env_name: Some(env_name.to_owned()),
        auth_token: auth_token.map(ToOwned::to_owned),
        root_env_explicit,
    };

    // Auto sign-in only makes sense when we can actually drive a browser/device
    // prompt, so require both stdin and stdout to be a terminal. Print-mode
    // pipelines (`claude -p ... | cmd`) keep the previous non-interactive error.
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();

    match crate::status::resolve_auth_token(&token_request).await {
        Ok(crate::status::AuthTokenOutcome::Available(token)) => return Ok(Some(token)),
        // Credentials are missing or stale (expired/revoked refresh token). On a
        // terminal, re-running sign-in repairs both; otherwise preserve the prior
        // behavior (missing-key error, or surfacing the refresh failure).
        Ok(crate::status::AuthTokenOutcome::NotLoggedIn) if !interactive => {
            return Ok(None);
        }
        Ok(crate::status::AuthTokenOutcome::NotLoggedIn) => {}
        Err(error) if !interactive => return Err(error.into()),
        Err(_) => {}
    }

    eprintln!("Signing in to {}...", crate::DISPLAY_NAME);
    let login_request = crate::status::AuthLoginRequest {
        env_name: Some(env_name.to_owned()),
        root_env_explicit,
        no_browser: false,
        paste: false,
    };
    let message = crate::status::auth_login(&login_request)
        .await
        .map_err(|error| RunError::Login(error.to_string()))?;
    crate::status::write_auth_message(&message)
        .map_err(|error| RunError::Login(format!("failed to write login output: {error}")))?;

    match crate::status::resolve_auth_token(&token_request).await? {
        crate::status::AuthTokenOutcome::Available(token) => Ok(Some(token)),
        crate::status::AuthTokenOutcome::NotLoggedIn => Ok(None),
    }
}

/// Start the on-device router for a local-router launch, surfacing its progress.
///
/// `run_command_from_home` otherwise discards the status string `start_from_home`
/// returns, so the terminal stays silent for the multi-second-to-minute wait
/// while the local model loads. We print a heads-up before the blocking start and
/// the router's own status afterward, so a slow launch reads as "working", not
/// "hung".
async fn start_local_router(
    home: &Path,
    start_request: &crate::router::RouterStartRequest,
) -> Result<(), RunError> {
    if start_request.no_local_model {
        eprintln!(
            "Starting router (config-driven; no on-device model).\nRouter progress: tail -f {}",
            crate::router::local_router_log_path(home).display()
        );
    } else {
        eprintln!(
            "Starting on-device model (first response can take a minute or two while it loads and reads your prompt).\nRouter progress: tail -f {}",
            crate::router::local_router_log_path(home).display()
        );
    }
    if start_request.enable_proxy {
        eprintln!(
            "Proxy routing decisions: tail -f {}",
            crate::router::proxy_log_path(home, false).display()
        );
    }
    let status = crate::router::start_from_home(home, start_request)
        .await
        .map_err(|error| RunError::Router(error.to_string()))?;
    eprint!("{status}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn configure_override_env(
    command: &mut Command,
    local_start_request: Option<&crate::router::RouterStartRequest>,
    home: &Path,
    env_name: &str,
    router_url: &str,
    key: &str,
    model: &str,
    auto_compact_window: Option<&str>,
) -> Result<(), RunError> {
    if let Some(start_request) = local_start_request {
        let injector_port = start_request.injector_port;
        start_local_router(home, start_request).await?;
        command.env(
            "ANTHROPIC_BASE_URL",
            format!("http://127.0.0.1:{injector_port}"),
        );
    } else {
        command.env("ANTHROPIC_BASE_URL", router_url);
    }
    command.env("ANTHROPIC_AUTH_TOKEN", key);
    command.env("ANTHROPIC_MODEL", model);
    set_auto_compact_window_env(command, auto_compact_window);
    command.env(RAYLINE_ENV_NAME_ENV, env_name);
    command.env_remove("ANTHROPIC_API_KEY");
    command.env_remove("RAYLINE_CLAUDE_ROUTING_MODE");
    command.env_remove("RAYLINE_ROUTER_URL");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn configure_proxy_env(
    command: &mut Command,
    request: &RunRequest,
    local_start_request: Option<&crate::router::RouterStartRequest>,
    home: &Path,
    env_name: &str,
    router_url: &str,
    key: &str,
    model: Option<&str>,
    auto_compact_window: Option<&str>,
    isolated: bool,
) -> Result<(), RunError> {
    let proxy_port = resolve_proxy_port(isolated)?;
    let proxy_routing_mode = proxy_routing_mode_name(request.routing_mode);
    if let Some(start_request) = local_start_request {
        if isolated {
            let mut start_request = start_request.clone();
            start_request.enable_proxy = false;
            start_local_router(home, &start_request).await?;
            eprintln!(
                "Proxy routing decisions: tail -f {}",
                crate::router::proxy_log_path(home, true).display()
            );
            let status = crate::router::start_local_proxy_from_home(
                home,
                &crate::router::LocalProxyStartRequest {
                    router_url: start_request.router_url.clone(),
                    proxy_port,
                    proxy_routing_mode: proxy_routing_mode.to_owned(),
                    router_config_path: start_request.router_config_path.clone(),
                    local_model_id: start_request.local_model_id.clone(),
                    adapter_port: start_request.adapter_port,
                    custom_mode: start_request.upstream_url.is_some(),
                    force_restart: request.diagnose,
                    diagnose: request.diagnose,
                    upstream_ca_path: request.upstream_ca_path.clone(),
                    isolated: true,
                },
            )
            .await
            .map_err(|error| RunError::Router(error.to_string()))?;
            eprint!("{status}");
        } else {
            // The engaged local daemon also binds the proxy port in proxy mode.
            // Clone the config-derived request and enable its embedded proxy.
            let mut start_request = start_request.clone();
            start_request.enable_proxy = true;
            start_request.proxy_port = proxy_port;
            start_request.proxy_routing_mode = proxy_routing_mode.to_owned();
            start_local_router(home, &start_request).await?;
        }
    } else {
        crate::router::start_proxy_from_home(
            home,
            router_url,
            key,
            proxy_port,
            proxy_routing_mode,
            request.diagnose,
            request.diagnose,
            request.upstream_ca_path.as_deref(),
            isolated,
            // v2 cloud-plane `--config`: hand the proxy the config so its subagent
            // allowlist (`routes.subagents`) routes the named types to the hosted
            // router and passes the rest through.
            request.router_config_path.as_deref(),
        )
        .await
        .map_err(|error| RunError::Router(error.to_string()))?;
    }

    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    command.env("HTTPS_PROXY", &proxy_url);
    command.env("https_proxy", &proxy_url);
    command.env("NODE_EXTRA_CA_CERTS", node_ca_bundle_value(home, request));
    if env::var_os("NODE_USE_SYSTEM_CA").is_none() {
        command.env("NODE_USE_SYSTEM_CA", "1");
    }
    if let Some(model) = model {
        command.env("ANTHROPIC_MODEL", model);
    }
    set_auto_compact_window_env(command, auto_compact_window);
    command.env(
        "RAYLINE_CLAUDE_ROUTING_MODE",
        routing_mode_name(request.routing_mode),
    );
    command.env(RAYLINE_ENV_NAME_ENV, env_name);
    command.env("RAYLINE_ROUTER_URL", router_url);
    let existing_no_proxy = [env::var("NO_PROXY").ok(), env::var("no_proxy").ok()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(",");
    let no_proxy = append_no_proxy(&existing_no_proxy, &["localhost", "127.0.0.1", "::1"]);
    command.env("NO_PROXY", &no_proxy);
    command.env("no_proxy", no_proxy);
    configure_proxy_auth_env(command, request.routing_mode);
    Ok(())
}

fn should_set_model_env(
    routing_mode: RoutingMode,
    request_model_explicit: bool,
    inherited_anthropic_model: bool,
) -> bool {
    routing_mode != RoutingMode::ProxySubagents
        || request_model_explicit
        || inherited_anthropic_model
}

/// The effective value of each refusal-recovery switch for this launch: what
/// the child must run with, and what a reused Claude Code daemon must already
/// hold, since its background workers inherit the daemon's env.
///
/// When the router serves the main conversation, a switch is the user's
/// non-empty value or `1`. Under `--route subagents` the main conversation goes
/// straight to Anthropic, so a switch is only the user's non-empty value (None:
/// Claude Code's default). An empty value is unset, as for the compact window:
/// Claude Code reads it as "not disabled".
pub(crate) fn refusal_recovery_values(
    routing_mode: RoutingMode,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, Option<String>)> {
    CLAUDE_REFUSAL_RECOVERY_OFF_ENV
        .into_iter()
        .map(|key| {
            let user = inherited(key)
                .filter(|value| !value.is_empty())
                .map(|value| value.to_string_lossy().into_owned());
            let value = if routing_mode == RoutingMode::ProxySubagents {
                user
            } else {
                user.or_else(|| Some("1".to_owned()))
            };
            (key, value)
        })
        .collect()
}

/// The refusal-recovery switches plus the message-threads switch: every env
/// switch the child runs with and a reused daemon must already hold. Any proxy
/// mode can send router-bound continuations (subagent turns too), so threads
/// default off there; elsewhere they are the user's value or Claude Code's.
pub(crate) fn launch_env_switches(
    routing_mode: RoutingMode,
    inherited: impl Fn(&str) -> Option<OsString>,
) -> Vec<(&'static str, Option<String>)> {
    let mut values = refusal_recovery_values(routing_mode, &inherited);
    let user = inherited(MESSAGE_THREADS_ENV)
        .filter(|value| !value.is_empty())
        .map(|value| value.to_string_lossy().into_owned());
    let threads = if is_proxy_routing_mode(routing_mode) {
        user.or_else(|| Some("0".to_owned()))
    } else {
        user
    };
    values.push((MESSAGE_THREADS_ENV, threads));
    values
}

/// Sets the refusal-recovery switches on the child, in every mode: a value
/// with a `Some` goes on the child, a `None` leaves the inherited env alone.
pub(crate) fn apply_refusal_recovery_env(
    command: &mut Command,
    values: &[(&'static str, Option<String>)],
) {
    for (key, value) in values {
        if let Some(value) = value {
            command.env(key, value);
        }
    }
}

fn configure_proxy_auth_env(command: &mut Command, routing_mode: RoutingMode) {
    if routing_mode == RoutingMode::ProxySubagents {
        command.env_remove("ANTHROPIC_BASE_URL");
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
        command.env_remove("ANTHROPIC_API_KEY");
    } else {
        command.env_remove("ANTHROPIC_AUTH_TOKEN");
        command.env_remove("ANTHROPIC_BASE_URL");
        command.env_remove("ANTHROPIC_API_KEY");
    }
}

/// The Claude Code session command, with Rayline's router keys withheld in
/// every mode. Claude Code never needs them: the proxy holds the key in proxy
/// modes, and `--via env` passes it as `ANTHROPIC_AUTH_TOKEN`. Its Bash tool
/// passes Claude Code's environment on to every command it runs, so a key
/// left there would reach a model-visible `env` call. Every `claude` spawn (the
/// session, `auth login`, `daemon stop`, the diagnostic version probe) goes
/// through here.
pub(crate) fn claude_command(claude_bin: &Path) -> Command {
    let mut command = Command::new(claude_bin);
    crate::codex::withhold_router_key(&mut command);
    command
}

fn resolve_injector_port(explicit: Option<u16>) -> Result<u16, RunError> {
    if let Some(port) = explicit {
        return Ok(port);
    }
    match env::var("RAYLINE_INJECTOR_PORT") {
        Ok(value) if !value.is_empty() => value.parse::<u16>().map_err(|_| {
            RunError::Router(format!(
                "RAYLINE_INJECTOR_PORT must be an integer, got {value:?}"
            ))
        }),
        _ => Ok(DEFAULT_LOCAL_INJECTOR_PORT),
    }
}

fn resolve_proxy_port(isolated: bool) -> Result<u16, RunError> {
    let (env_var, default_port) = if isolated {
        ("RAYLINE_ISOLATED_PROXY_PORT", DEFAULT_ISOLATED_PROXY_PORT)
    } else {
        ("RAYLINE_PROXY_PORT", DEFAULT_PROXY_PORT)
    };
    match env::var(env_var) {
        Ok(value) if !value.is_empty() => value
            .parse::<u16>()
            .map_err(|_| RunError::Router(format!("{env_var} must be an integer, got {value:?}"))),
        _ => Ok(default_port),
    }
}

/// The auto-compact window pinned by `--auto-compact-window` or
/// `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, if either is set.
fn explicit_auto_compact_window(request: &RunRequest) -> Option<String> {
    if let Some(window) = request.auto_compact_window {
        return Some(window.to_string());
    }
    env::var(AUTO_COMPACT_WINDOW_ENV)
        .ok()
        .filter(|value| !value.is_empty())
}

/// Whether `model` is a Claude Code model alias (`opus`, `sonnet[1m]`,
/// `opusplan`, ...) rather than a concrete model id. Claude Code resolves these
/// itself, so the hosted router's catalog cannot answer for them.
fn is_claude_code_model_alias(model: &str) -> bool {
    let model = model.trim().to_ascii_lowercase();
    let base = model.strip_suffix("[1m]").unwrap_or(&model);
    CLAUDE_CODE_MODEL_ALIASES.contains(&base)
}

/// Pin `CLAUDE_CODE_AUTO_COMPACT_WINDOW`, or clear it so Claude Code applies
/// its own window for the model.
fn set_auto_compact_window_env(command: &mut Command, auto_compact_window: Option<&str>) {
    match auto_compact_window {
        Some(window) => command.env(AUTO_COMPACT_WINDOW_ENV, window),
        None => command.env_remove(AUTO_COMPACT_WINDOW_ENV),
    };
}

/// Read the account-level `enable_local_router` toggle from a `GET /v1/settings`
/// response. Defaults to `false` (stay cloud) when the field is absent or not a
/// bool — the safe default the design mandates.
fn enable_local_router_from_router_settings(result: &Value) -> bool {
    result
        .get("settings")
        .and_then(|settings| settings.get("enable_local_router"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// The hosted router's auto-compact window for the launch model:
/// `GET /v1/settings?model=<model>&client_sizes=1` returns the window of the model serving a
/// launch id Claude Code doesn't know, and a null or absent `autoCompactWindow`
/// for one Claude Code sizes itself, which is `None` here. A failed request or a
/// window that isn't a positive integer fails the launch rather than guessing.
async fn router_auto_compact_window(
    env_name: &str,
    router_base: &str,
    auth_token: Option<&str>,
    router_key: &str,
    model: &str,
) -> Result<Option<String>, RunError> {
    get_router_settings(env_name, router_base, auth_token, router_key, Some(model))
        .await
        .and_then(|body| match body.get("autoCompactWindow") {
            None | Some(Value::Null) => Ok(None),
            Some(window) => window
                .as_u64()
                .filter(|window| *window > 0)
                .map(|window| Some(window.to_string()))
                .ok_or_else(|| {
                    format!("the response's autoCompactWindow {window} is not a positive integer")
                }),
        })
        .map_err(|error| {
            RunError::Router(format!(
                "Couldn't read the auto-compact window for {model} from {router_base}/v1/settings: \
                 {error}. Pass --auto-compact-window <tokens> to set it."
            ))
        })
}

/// Fetch the caller's router settings for the `enable_local_router` toggle.
/// `None` on request failure or an error payload, so the toggle stays off.
async fn fetch_router_settings(
    env_name: &str,
    router_base: &str,
    auth_token: Option<&str>,
    router_key: &str,
) -> Option<Value> {
    get_router_settings(env_name, router_base, auth_token, router_key, None)
        .await
        .ok()
}

/// `GET /v1/settings`, with `?model=` when given. Prefer account credentials
/// when available, but fall back to the already-provisioned router key because
/// the router accepts either bearer form.
async fn get_router_settings(
    env_name: &str,
    router_base: &str,
    auth_token: Option<&str>,
    router_key: &str,
    model: Option<&str>,
) -> Result<Value, String> {
    let token_request = crate::status::AuthTokenRequest {
        env_name: Some(env_name.to_owned()),
        // Honor an explicit `--auth-token` first; when account credentials are
        // absent or expired, the stored router key below can still read settings.
        auth_token: auth_token.map(ToOwned::to_owned),
        root_env_explicit: false,
    };
    let bearer_token = match crate::status::resolve_auth_token(&token_request).await {
        Ok(crate::status::AuthTokenOutcome::Available(token)) => token,
        Ok(crate::status::AuthTokenOutcome::NotLoggedIn) => router_key.to_owned(),
        Err(_) => router_key.to_owned(),
    };
    let mut request = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .map_err(|error| error.to_string())?
        .get(format!("{router_base}/v1/settings"))
        .bearer_auth(bearer_token);
    if let Some(model) = model {
        // `client_sizes=1` tells the router this client accepts a null window
        // (Claude Code sizes the model itself). Without it the router keeps
        // answering a number, because 0.2.10 fails the launch on null.
        request = request.query(&[("model", model), ("client_sizes", "1")]);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("HTTP {status}: {error}"))?;
    if let Some(error) = body.get("error") {
        let message = error
            .as_str()
            .map_or_else(|| error.to_string(), ToOwned::to_owned);
        return Err(format!("HTTP {status}: {message}"));
    }
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    Ok(body)
}

/// Resolve a present `local_model` config into one that can actually engage,
/// or print a warning and return `None` so the launch continues with cloud
/// routing (the plan's "warn before invoking claude" — never block, never
/// download):
/// - Custom complete → engage as-is; incomplete → warn.
/// - Recommended with a pick → engage when its GGUF is downloaded, else warn.
/// - Recommended without a pick (e.g. legacy `mode: "auto"`) → adopt the best
///   already-downloaded curated model and persist it as the pick (so the
///   other Rayline clients and `local show` reflect it); nothing downloaded or catalog
///   unreachable → warn.
async fn resolve_engageable_local_config(
    home: &Path,
    env_name: &str,
    cfg: crate::local_model::LocalModelConfig,
) -> Option<crate::local_model::LocalModelConfig> {
    let cli = crate::CLI_BIN;
    match cfg.mode {
        crate::local_model::LocalModelMode::Custom => {
            if cfg.is_engageable() {
                return Some(cfg);
            }
            eprintln!(
                "Warning: local routing is enabled, but the custom endpoint is incomplete. Continuing with cloud routing. Set it with `{cli} local custom --url <URL> --model <NAME>`."
            );
            None
        }
        crate::local_model::LocalModelMode::Recommended => {
            if cfg.has_recommended_pick() {
                if crate::router::hf_cache_has_verified_config_gguf(home, &cfg) {
                    return Some(cfg);
                }
                let model_id = cfg
                    .model_id
                    .as_deref()
                    .or(cfg.model_file.as_deref())
                    .unwrap_or("selected model");
                eprintln!(
                    "Warning: local routing is enabled, but the local model `{model_id}` is not downloaded. Continuing with cloud routing. Download it with `{cli} local download {model_id}`."
                );
                return None;
            }
            match crate::catalog::auto_select_downloaded(env_name).await {
                Some(model) => {
                    eprintln!(
                        "No local model selected — using downloaded `{id}` (saved as your selection; change with `{cli} local use <model-id>`).",
                        id = model.id,
                    );
                    match crate::local_model::set_recommended_in_home(home, &model) {
                        Ok(cfg) => Some(cfg),
                        Err(error) => {
                            // Engage anyway: the pick is valid, only the
                            // write-back failed — it will be retried next run.
                            eprintln!(
                                "Warning: could not save the local model selection: {error}."
                            );
                            Some(crate::local_model::LocalModelConfig {
                                mode: crate::local_model::LocalModelMode::Recommended,
                                provider: Some("llamacpp".to_owned()),
                                protocol: Some("anthropic_messages".to_owned()),
                                base_url: cfg.base_url,
                                model: cfg.model,
                                model_id: Some(model.id),
                                model_repo: Some(model.repo),
                                model_file: Some(model.filename),
                                model_revision: Some(model.revision),
                                model_sha256: Some(model.sha256),
                                custom_endpoints: cfg.custom_endpoints,
                            })
                        }
                    }
                }
                // Nothing downloaded — a sole saved custom endpoint is the
                // only added model, so select and use it.
                None => match cfg.custom_endpoints.as_slice() {
                    [endpoint] => {
                        eprintln!(
                            "No local model selected — using your saved custom endpoint `{model}` ({url}).",
                            model = endpoint.model,
                            url = endpoint.base_url,
                        );
                        match crate::local_model::activate_custom_endpoint_in_home(home, endpoint) {
                            Ok(cfg) => Some(cfg),
                            Err(error) => {
                                eprintln!(
                                    "Warning: could not save the local model selection: {error}. Continuing with cloud routing."
                                );
                                None
                            }
                        }
                    }
                    _ => {
                        eprintln!(
                            "Warning: local routing is enabled, but no local model is selected and none is downloaded. Continuing with cloud routing. Pick one with `{cli} local use <model-id>` (see `{cli} local models`)."
                        );
                        None
                    }
                },
            }
        }
    }
}

fn append_no_proxy(existing: &str, required: &[&str]) -> String {
    let mut values = existing
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut seen = values
        .iter()
        .map(|value| value.to_ascii_lowercase())
        .collect::<std::collections::BTreeSet<_>>();
    for value in required {
        if seen.insert(value.to_ascii_lowercase()) {
            values.push((*value).to_owned());
        }
    }
    values.join(",")
}

fn node_ca_bundle_value(home: &Path, request: &RunRequest) -> String {
    let proxy_ca_path = crate::router::default_proxy_ca_cert_path(home);
    let existing_node_ca = env::var_os("NODE_EXTRA_CA_CERTS").map(PathBuf::from);
    let bundle_path = proxy_ca_path.parent().map_or_else(
        || PathBuf::from(NODE_CA_BUNDLE_FILENAME),
        |parent| parent.join(NODE_CA_BUNDLE_FILENAME),
    );
    let mut extra = Vec::new();
    if let Some(existing_node_ca) = existing_node_ca {
        if existing_node_ca != proxy_ca_path && existing_node_ca != bundle_path {
            extra.push(existing_node_ca);
        }
    }
    if let Some(upstream_ca_path) = request.upstream_ca_path.as_ref() {
        if upstream_ca_path != &proxy_ca_path && upstream_ca_path != &bundle_path {
            extra.push(upstream_ca_path.clone());
        }
    }
    if extra.is_empty() {
        return proxy_ca_path.display().to_string();
    }

    let mut sections = Vec::new();
    for source in std::iter::once(proxy_ca_path.as_path()).chain(extra.iter().map(PathBuf::as_path))
    {
        let Ok(contents) = std::fs::read_to_string(source) else {
            continue;
        };
        let contents = contents.trim();
        if !contents.is_empty() {
            sections.push(contents.to_owned());
        }
    }
    if sections.is_empty() {
        return proxy_ca_path.display().to_string();
    }
    if let Some(parent) = bundle_path.parent() {
        if std::fs::create_dir_all(parent).is_ok()
            && std::fs::write(&bundle_path, sections.join("\n") + "\n").is_ok()
        {
            return bundle_path.display().to_string();
        }
    }
    proxy_ca_path.display().to_string()
}

fn inject_claude_debug(args: &[OsString]) -> Vec<OsString> {
    if args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "--debug" || arg.starts_with("--debug="))
    }) {
        return args.to_vec();
    }
    std::iter::once(OsString::from("--debug"))
        .chain(args.iter().cloned())
        .collect()
}

pub(crate) fn claude_agent_view_disabled() -> bool {
    env::var(CLAUDE_DISABLE_AGENT_VIEW_ENV).is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        )
    })
}

/// Resolve the Claude Code config dir this run targets. In isolated mode that is
/// the brand's private dir (`~/.<brand>/cc`); otherwise it honors an externally
/// set `CLAUDE_CONFIG_DIR`, falling back to the standard `~/.claude`.
fn claude_config_dir(home: &Path, isolated: bool) -> PathBuf {
    if isolated {
        return isolated_cc_dir(home);
    }
    env::var_os(CLAUDE_CONFIG_DIR_ENV)
        .map(PathBuf::from)
        .map(|path| expand_user_path(path, Some(home)))
        .unwrap_or_else(|| home.join(".claude"))
}

/// The brand-private Claude config dir used by `--isolated` (e.g. `~/.rayline/cc`).
pub(crate) fn isolated_cc_dir(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR).join("cc")
}

/// Write-target directories Claude creates session work under. Created in the
/// shared root if missing so the symlink always points at the shared copy, even
/// on a fresh `~/.claude`: otherwise Claude would create them locally under the
/// isolated config and isolated-mode work would never show up in standard runs.
/// Daemon/runtime state (daemon.lock, daemon/, jobs/, tasks/, teams/, todos/) is
/// deliberately absent so it stays local and the supervisors never collide.
const ISOLATED_SHARED_DIRS: [&str; 4] = ["projects", "sessions", "plans", "paste-cache"];

/// Write-target files: created empty in the shared root if missing, then
/// symlinked, so isolated and standard runs append to the same file.
const ISOLATED_SHARED_FILES: [&str; 1] = ["history.jsonl"];

/// Read-only customization shared via symlink only when present in the shared
/// root. Claude does not create these, so a missing entry cannot diverge; we
/// avoid materializing empty `CLAUDE.md`/statusline/skills entries the user
/// never had.
const ISOLATED_OPTIONAL_ENTRIES: [&str; 7] = [
    "skills",
    "plugins",
    "commands",
    "agents",
    "CLAUDE.md",
    "hooks",
    "statusline-worktree.js",
];

/// Per-profile config dir files seeded as independent copies so they can
/// diverge. `.claude.json` is seeded separately (see
/// [`seed_isolated_claude_json`]) because it lives outside the config dir.
const ISOLATED_SEED_FILES: [&str; 1] = ["settings.json"];

/// Lay down the isolated overlay and point Claude Code at it. Idempotent: safe
/// to call on every run.
fn apply_isolated_config_dir(
    command: &mut Command,
    home: &Path,
    claude_bin: &Path,
    print_flag: Option<&str>,
    needs_claude_login: bool,
) -> Result<(), RunError> {
    let source_dir = claude_config_dir(home, false);
    let isolated_dir = isolated_cc_dir(home);
    ensure_isolated_overlay(&source_dir, &isolated_dir);
    // `.claude.json` (global app state, project-trust, personal MCP) lives at
    // $HOME/.claude.json by default, NOT inside ~/.claude. Seed the isolated
    // profile from it so project-trust, MCP servers and settings carry over, but
    // strip the auth keys so it starts logged out (its own login below).
    let source_global_config = global_claude_json_path(home);
    let isolated_global_config = isolated_dir.join(".claude.json");

    command.env(CLAUDE_CONFIG_DIR_ENV, &isolated_dir);

    // Always (re)seed `.claude.json` first — it is idempotent (no-op when the
    // file already exists), so this restores project-trust/MCP/onboarding state
    // even if `~/.rayline/cc` was deleted while the credential survived (the
    // macOS keychain entry outlives the dir). Do it before any auth fast-path so
    // a recreated profile never loses the inherited app state.
    seed_isolated_claude_json(&source_global_config, &isolated_global_config);

    // Env mode authenticates Claude Code via ANTHROPIC_BASE_URL/AUTH_TOKEN, so it
    // needs no Claude OAuth login; the seed above is all it requires.
    if !needs_claude_login {
        return Ok(());
    }

    // Already authenticated → nothing more to do.
    if isolated_profile_is_authenticated(&isolated_dir) {
        return Ok(());
    }

    // Non-interactive (`-p`/`--print`, or piped streams): we can't drive a
    // browser sign-in. Fail before starting the router/proxy (so the output
    // isn't cluttered with a launch that can't succeed), telling the user how to
    // authenticate the isolated profile once.
    if !can_prompt_user(print_flag.is_some()) {
        let how = match print_flag {
            Some(flag) => format!("Re-run the same command without `{flag}` to sign in first."),
            None => "Run it again interactively (not piped) to sign in first.".to_owned(),
        };
        return Err(RunError::NotLoggedIn(format!(
            "This isolated session is not logged in to Claude Code. {how}"
        )));
    }

    // Interactive: sign the isolated profile in via Claude Code's own browser
    // OAuth, then proceed. Claude owns the credential, so we never touch the
    // secret.
    run_isolated_browser_login(&isolated_dir, claude_bin)?;

    // `claude auth login` rewrites `.claude.json`, resetting
    // `hasCompletedOnboarding` to false — which would launch the session into the
    // first-run onboarding wizard right after the user just signed in. Restore
    // the inherited app state (onboarding flag + project-trust + MCP) from the
    // source profile, keeping the `oauthAccount` login that auth-login just
    // wrote. This is sequential — login has fully exited before we rewrite, and
    // the session reads the file afterward — so there is no flash. (Per-folder
    // trust prompts are a separate, legitimate Claude Code behavior for folders
    // the user has not yet trusted.)
    restore_isolated_app_state(&source_global_config, &isolated_global_config);
    Ok(())
}

/// After login, merge the source profile's app state (onboarding, project-trust,
/// MCP) into the isolated `.claude.json` while preserving the freshly-written
/// auth. Source keys win for app state; the isolated file's `oauthAccount` (just
/// written by `auth login`) is kept. Best-effort no-op on any error.
fn restore_isolated_app_state(source: &Path, dest: &Path) {
    let Ok(dest_raw) = fs::read_to_string(dest) else {
        return;
    };
    let Ok(mut dest_value) = serde_json::from_str::<Value>(&dest_raw) else {
        return;
    };
    let Some(dest_obj) = dest_value.as_object_mut() else {
        return;
    };

    // Merge inherited app state from the source profile (trust, MCP, onboarding,
    // settings), but never the source's auth keys — login owns those now.
    if let Some(source_obj) = fs::read_to_string(source)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|value| value.as_object().cloned())
    {
        for (key, val) in source_obj {
            if CLAUDE_JSON_AUTH_KEYS.contains(&key.as_str()) {
                continue;
            }
            dest_obj.insert(key, val);
        }
    }
    // Belt-and-braces: ensure onboarding is complete even if the source lacked it.
    dest_obj.insert("hasCompletedOnboarding".to_owned(), Value::Bool(true));

    if let Ok(updated) = serde_json::to_string_pretty(&dest_value) {
        let _ = fs::write(dest, updated);
    }
}

/// Path of Claude Code's global `.claude.json` (app state, project-trust,
/// personal MCP servers, OAuth). It lives inside `CLAUDE_CONFIG_DIR` when that
/// is set, otherwise at `$HOME/.claude.json` (NOT inside `~/.claude`).
fn global_claude_json_path(home: &Path) -> PathBuf {
    match env::var_os(CLAUDE_CONFIG_DIR_ENV) {
        Some(dir) => expand_user_path(PathBuf::from(dir), Some(home)).join(".claude.json"),
        None => home.join(".claude.json"),
    }
}

/// Whether we may interactively prompt the user before launching Claude Code.
/// False in print mode (`-p`/`--print`) and whenever stdin/stdout/stderr are not
/// all TTYs (e.g. piped output), so non-interactive launches never block.
fn can_prompt_user(print_mode: bool) -> bool {
    !print_mode
        && io::stdin().is_terminal()
        && io::stdout().is_terminal()
        && io::stderr().is_terminal()
}

/// Sign the isolated profile in via Claude Code's own `auth login` (browser
/// OAuth), waiting for it to complete so the profile is authenticated before the
/// main launch. Caller has confirmed we can prompt (see [`can_prompt_user`]).
///
/// Fails (so the caller aborts the launch as a clean preflight failure, rather
/// than starting the proxy/router against an unauthenticated profile) when the
/// login command cannot run, exits non-zero (e.g. the user cancels or the OAuth
/// callback server cannot bind), or leaves the profile still logged out.
fn run_isolated_browser_login(isolated_dir: &Path, claude_bin: &Path) -> Result<(), RunError> {
    eprintln!(
        "\nThis isolated session needs its own Claude Code login. \
         Opening browser sign-in..."
    );
    // `claude auth login` runs the browser OAuth flow and exits (unlike passing
    // `/login`, which would open the REPL). It honors CLAUDE_CONFIG_DIR for the
    // credential location on Linux/Windows; on macOS the credential lands in the
    // keychain under the isolated dir's namespaced service. Claude sets up the
    // credential and its ACL itself, so there is no keychain prompt from us.
    let status = claude_command(claude_bin)
        .args(["auth", "login"])
        .env(CLAUDE_CONFIG_DIR_ENV, isolated_dir)
        .status()
        .map_err(|error| {
            RunError::NotLoggedIn(format!("Could not run `claude auth login`: {error}"))
        })?;
    eprintln!();

    // Don't trust the exit code alone — re-check that the credential actually
    // landed, so a partial/cancelled sign-in doesn't proceed as logged in.
    if !status.success() || !isolated_profile_is_authenticated(isolated_dir) {
        return Err(RunError::NotLoggedIn(
            "Claude Code sign-in did not complete, so this isolated session is still \
             logged out. Re-run and finish the browser sign-in."
                .to_owned(),
        ));
    }
    Ok(())
}

/// Make `isolated_dir` a thin overlay on `shared_root` (the user's main Claude
/// config dir): symlink shared content back to the shared root and seed
/// settings.json / .claude.json as local copies. Best-effort throughout; a
/// failed link or copy just means that entry is not shared, never a failed
/// launch.
fn ensure_isolated_overlay(shared_root: &Path, isolated_dir: &Path) {
    if shared_root == isolated_dir {
        // The shared root already is the isolated dir (e.g. the user pointed
        // CLAUDE_CONFIG_DIR at it); there is nothing to overlay onto itself.
        return;
    }
    if fs::create_dir_all(isolated_dir).is_err() {
        return;
    }
    for dir in ISOLATED_SHARED_DIRS {
        let source = shared_root.join(dir);
        // Create the shared target first so the symlink always resolves to it,
        // even on a brand-new ~/.claude.
        let _ = fs::create_dir_all(&source);
        link_shared_entry(&source, &isolated_dir.join(dir));
    }
    for file in ISOLATED_SHARED_FILES {
        let source = shared_root.join(file);
        if !source.exists() {
            let _ = fs::write(&source, b"");
        }
        link_shared_entry(&source, &isolated_dir.join(file));
    }
    for entry in ISOLATED_OPTIONAL_ENTRIES {
        link_shared_entry(&shared_root.join(entry), &isolated_dir.join(entry));
    }
    for file in ISOLATED_SEED_FILES {
        seed_local_copy(&shared_root.join(file), &isolated_dir.join(file));
    }
}

/// Symlink `link` -> `source` when the source exists and the slot is still
/// empty. An existing file or symlink is left untouched (respect the user).
fn link_shared_entry(source: &Path, link: &Path) {
    if !source.exists() {
        return;
    }
    if fs::symlink_metadata(link).is_ok() {
        return;
    }
    let _ = make_symlink(source, link);
}

/// Copy `source` -> `dest` once, when the destination does not yet exist. These
/// files hold credentials, so keep the copy (and its parent dir) user-private.
fn seed_local_copy(source: &Path, dest: &Path) {
    if fs::symlink_metadata(dest).is_ok() {
        return;
    }
    copy_private_file(source, dest);
}

/// Auth-bearing keys in `.claude.json` — stripped when seeding an isolated
/// profile so it starts logged out (and signs in via its own browser login),
/// while project-trust, MCP servers and settings carry over. `primaryApiKey` is
/// where Claude stores a Console/API key when keychain storage is unavailable;
/// `customApiKeyResponses` caches API-key approval prompts; `oauthAccount` is the
/// subscription login. None of these should be copied into the isolated profile.
const CLAUDE_JSON_AUTH_KEYS: [&str; 3] = ["oauthAccount", "customApiKeyResponses", "primaryApiKey"];

/// Seed the isolated `.claude.json` from the user's profile, minus the auth
/// keys, so the isolated session inherits project-trust, MCP servers and
/// settings but starts logged out. Falls back to a minimal onboarding-complete
/// stub when the source is missing or unreadable. No-op if the destination
/// already exists (a prior login must not be clobbered).
fn seed_isolated_claude_json(source: &Path, dest: &Path) {
    if fs::symlink_metadata(dest).is_ok() {
        return;
    }
    let contents = fs::read_to_string(source)
        .ok()
        .and_then(|raw| claude_json_without_auth(&raw))
        .unwrap_or_else(|| "{\n  \"hasCompletedOnboarding\": true\n}".to_owned());
    if fs::write(dest, contents).is_ok() {
        set_user_private_permissions(dest);
    }
}

/// Return `raw` (`.claude.json` contents) with the auth keys removed, or `None`
/// if it is not a JSON object. Used to seed a logged-out isolated profile that
/// still carries project-trust, MCP servers and settings.
fn claude_json_without_auth(raw: &str) -> Option<String> {
    let mut value = serde_json::from_str::<Value>(raw).ok()?;
    let object = value.as_object_mut()?;
    for key in CLAUDE_JSON_AUTH_KEYS {
        object.remove(key);
    }
    serde_json::to_string_pretty(&value).ok()
}

fn copy_private_file(source: &Path, dest: &Path) {
    if !source.is_file() {
        return;
    }
    if fs::copy(source, dest).is_ok() {
        set_user_private_permissions(dest);
    }
}

/// Whether the isolated profile already has its own Claude Code credential.
/// On macOS the credential is a keychain item namespaced by the config-dir path
/// hash; we look it up by attributes only (no `-w`), so this never prompts.
#[cfg(target_os = "macos")]
fn isolated_profile_is_authenticated(isolated_dir: &Path) -> bool {
    find_keychain_account(&claude_code_keychain_service(isolated_dir)).is_some()
}

/// On Linux/Windows Claude Code stores its credential as a plain
/// `<config_dir>/.credentials.json` file, so a logged-in isolated profile simply
/// has that file.
#[cfg(not(target_os = "macos"))]
fn isolated_profile_is_authenticated(isolated_dir: &Path) -> bool {
    isolated_dir.join(".credentials.json").is_file()
}

/// Claude Code's macOS keychain generic-password service name for a non-default
/// config dir: `Claude Code-credentials-<hex>` where `<hex>` is the first 4
/// bytes of `sha256(path)` of the config-dir path string (verified against
/// Claude Code v2.1.170). The isolated dir is always an explicit, non-default
/// config dir, so it always carries this suffix.
#[cfg(target_os = "macos")]
fn claude_code_keychain_service(config_dir: &Path) -> String {
    let digest = Sha256::digest(config_dir.to_string_lossy().as_bytes());
    format!(
        "Claude Code-credentials-{:02x}{:02x}{:02x}{:02x}",
        digest[0], digest[1], digest[2], digest[3]
    )
}

#[cfg(target_os = "macos")]
fn find_keychain_account(service: &str) -> Option<String> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    parse_keychain_account(std::str::from_utf8(&output.stdout).ok()?)
}

#[cfg(target_os = "macos")]
fn parse_keychain_account(output: &str) -> Option<String> {
    const PREFIX: &str = "\"acct\"<blob>=\"";
    for line in output.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix(PREFIX) else {
            continue;
        };
        let Some((account, _)) = value.split_once('"') else {
            continue;
        };
        if !account.is_empty() {
            return Some(account.to_owned());
        }
    }
    None
}

#[cfg(unix)]
fn make_symlink(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn make_symlink(source: &Path, link: &Path) -> io::Result<()> {
    if source.is_dir() {
        std::os::windows::fs::symlink_dir(source, link)
    } else {
        std::os::windows::fs::symlink_file(source, link)
    }
}

#[cfg(not(any(unix, windows)))]
fn make_symlink(_source: &Path, _link: &Path) -> io::Result<()> {
    Ok(())
}

pub(crate) fn routing_mode_name(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::Override => ROUTING_MODE_OVERRIDE,
        RoutingMode::Proxy => ROUTING_MODE_PROXY,
        RoutingMode::ProxySubagents => ROUTING_MODE_PROXY_SUBAGENTS,
    }
}

fn proxy_routing_mode_name(mode: RoutingMode) -> &'static str {
    match mode {
        RoutingMode::Proxy => crate::router::PROXY_ROUTING_MODE_ALL,
        RoutingMode::ProxySubagents => crate::router::PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS,
        // `Override` starts no proxy at all (it sets ANTHROPIC_BASE_URL directly),
        // so it never reaches this proxy-only translation — see the dispatch match
        // in `run` where `Override` takes the `configure_override_env` branch.
        RoutingMode::Override => unreachable!("override mode does not start a proxy"),
    }
}

async fn diag_print_preamble(
    env_name: &str,
    router_url: &str,
    routing_mode: RoutingMode,
    home: &Path,
) {
    eprintln!();
    eprintln!("================================================================");
    eprintln!("{} claude - DIAGNOSTIC MODE", crate::CLI_BIN);
    eprintln!("================================================================");

    diag_print_section("Versions");
    eprintln!("  platform     : {} {}", env::consts::OS, env::consts::ARCH);
    eprintln!("  {:<12} : {}", crate::CLI_BIN, crate::RAYLINE_VERSION);
    match find_claude_bin(home) {
        Some(claude_bin) => {
            let version = claude_command(&claude_bin)
                .arg("--version")
                .output()
                .ok()
                .map(|output| {
                    if output.stdout.is_empty() {
                        String::from_utf8_lossy(&output.stderr).trim().to_owned()
                    } else {
                        String::from_utf8_lossy(&output.stdout).trim().to_owned()
                    }
                })
                .filter(|version| !version.is_empty())
                .unwrap_or_else(|| "<probe error>".to_owned());
            eprintln!("  claude       : {version} ({})", claude_bin.display());
        }
        None => eprintln!("  claude       : NOT FOUND on PATH"),
    }
    eprintln!("  env          : {env_name}");
    eprintln!("  routing-mode : {}", routing_mode_name(routing_mode));
    eprintln!("  router-url   : {router_url}");

    diag_print_section("Relevant env vars (current shell)");
    let mut any_set = false;
    for key in DIAG_ENV_FINGERPRINT_KEYS {
        if let Ok(value) = env::var(key) {
            any_set = true;
            eprintln!("  {key:<28} = {}", diag_redact(key, &value));
        }
    }
    if !any_set {
        eprintln!("  (none of the proxy/claude env vars are set)");
    }

    diag_print_section("Stored credentials");
    let credentials = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join("credentials.json");
    let proxy_ca = crate::router::default_proxy_ca_cert_path(home);
    eprintln!(
        "  ~/.config/{}/credentials.json : {}",
        crate::CONFIG_DIR,
        if credentials.is_file() {
            "present"
        } else {
            "missing"
        }
    );
    eprintln!(
        "  proxy CA cert ({}) : {}",
        proxy_ca.display(),
        if proxy_ca.is_file() {
            "present"
        } else {
            "missing"
        }
    );

    diag_print_section("Network reachability - DIRECT (no proxy)");
    for url in diag_probe_hosts(router_url) {
        let result = diag_probe(&url, None, None).await;
        eprintln!("  HEAD {url:<48} -> {result}");
    }
}

async fn diag_print_postamble_for_mode(
    routing_mode: RoutingMode,
    router_url: &str,
    isolated: bool,
    home: &Path,
) {
    match routing_mode {
        RoutingMode::Proxy | RoutingMode::ProxySubagents => {
            let default_port = if isolated {
                DEFAULT_ISOLATED_PROXY_PORT
            } else {
                DEFAULT_PROXY_PORT
            };
            let proxy_port = resolve_proxy_port(isolated).unwrap_or(default_port);
            diag_print_postamble(proxy_port, router_url, isolated, home).await;
        }
        RoutingMode::Override => {
            diag_print_section("Logs to send back");
            eprintln!(
                "  --no-proxy / routing-mode=override: no {} proxy involved (calls go direct to ANTHROPIC_BASE_URL).",
                crate::DAEMON_BIN
            );
            eprintln!("  Capture the session with `script` so we see Claude --debug output:");
            eprintln!("    script -q ~/{}-diagnose-session.log \\", crate::CLI_BIN);
            eprintln!("      {} claude --no-proxy --diagnose", crate::CLI_BIN);
            eprintln!("================================================================");
            eprintln!();
        }
    }
}

async fn diag_print_postamble(proxy_port: u16, router_url: &str, isolated: bool, home: &Path) {
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let proxy_ca = crate::router::default_proxy_ca_cert_path(home);
    let verify_path = proxy_ca.is_file().then_some(proxy_ca.as_path());
    diag_print_section(&format!(
        "Network reachability - VIA {} proxy ({proxy_url})",
        crate::DAEMON_BIN
    ));
    for url in diag_probe_hosts(router_url) {
        if url.trim_end_matches('/') == router_url.trim_end_matches('/') {
            continue;
        }
        let result = diag_probe(&url, Some(&proxy_url), verify_path).await;
        eprintln!("  HEAD {url:<48} -> {result}");
    }
    diag_print_section("Logs to send back");
    eprintln!(
        "  proxy log : {}",
        crate::router::proxy_log_path(home, isolated).display()
    );
    eprintln!("  claude    : stderr below (--debug enabled)");
    eprintln!();
    eprintln!("  Tip: re-run with `script` to capture the full session:");
    eprintln!("    script -q ~/{}-diagnose-session.log \\", crate::CLI_BIN);
    eprintln!(
        "      {} claude --routing-mode proxy --diagnose",
        crate::CLI_BIN
    );
    eprintln!("================================================================");
    eprintln!();
}

fn diag_print_section(title: &str) {
    eprintln!();
    eprintln!(
        "-- {title} {}",
        "-".repeat(60usize.saturating_sub(title.len() + 4))
    );
}

fn diag_redact(name: &str, value: &str) -> String {
    if matches!(
        name,
        "ANTHROPIC_AUTH_TOKEN" | "ANTHROPIC_API_KEY" | "RAYLINE_ROUTER_API_KEY"
    ) {
        let prefix = if value.len() > 6 { &value[..6] } else { "" };
        return format!("<set, len={}, prefix={prefix:?}>", value.len());
    }
    value.to_owned()
}

async fn diag_probe(url: &str, via_proxy: Option<&str>, verify_path: Option<&Path>) -> String {
    let mut builder = reqwest::Client::builder()
        .timeout(Duration::from_secs(DIAG_PROBE_TIMEOUT_SECONDS))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(proxy_url) = via_proxy {
        match reqwest::Proxy::all(proxy_url) {
            Ok(proxy) => builder = builder.proxy(proxy),
            Err(error) => return format!("ERR Proxy: {error}"),
        }
    }
    if let Some(path) = verify_path {
        match fs::read(path)
            .ok()
            .and_then(|pem| reqwest::Certificate::from_pem(&pem).ok())
        {
            Some(cert) => builder = builder.add_root_certificate(cert),
            None => return format!("ERR Certificate: could not read {}", path.display()),
        }
    }
    let client = match builder.build() {
        Ok(client) => client,
        Err(error) => return format!("ERR Client: {error}"),
    };
    match client.head(url).send().await {
        Ok(response) => format!("HTTP {}", response.status().as_u16()),
        Err(error) => format!("ERR {}: {error}", error_kind(&error)),
    }
}

fn error_kind(error: &reqwest::Error) -> &'static str {
    if error.is_timeout() {
        "Timeout"
    } else if error.is_connect() {
        "Connect"
    } else if error.is_request() {
        "Request"
    } else if error.is_decode() {
        "Decode"
    } else {
        "Http"
    }
}

fn diag_probe_hosts(router_url: &str) -> Vec<String> {
    let mut hosts = DIAG_EXTERNAL_PROBE_HOSTS
        .iter()
        .map(|url| (*url).to_owned())
        .collect::<Vec<_>>();
    hosts.push(router_url.to_owned());
    hosts
}

fn shell_quote(value: &str) -> String {
    if value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct StatuslineInstallResult {
    settings_path: PathBuf,
    installed: bool,
    conflict: bool,
    existing_command: Option<String>,
}

fn configure_route_statusline(home: &Path, isolated: bool, enabled: bool) {
    // Isolated sessions read their own settings.json, so the route-statusline
    // entry must be written there, not into the shared ~/.claude/settings.json.
    let settings_path = if isolated {
        isolated_cc_dir(home).join("settings.json")
    } else {
        default_settings_path(Some(home))
    };
    if !enabled {
        let _ = uninstall_statusline_settings_from_path(settings_path, Some(home));
        return;
    }

    let Ok(rld_bin) = crate::router::resolve_rld_bin(home) else {
        return;
    };
    let Ok(result) = install_statusline_settings_from_path(settings_path, Some(home), &rld_bin)
    else {
        return;
    };
    if result.conflict {
        let existing = result
            .existing_command
            .as_deref()
            .unwrap_or("your custom status line");
        eprintln!(
            "{}: keeping your existing Claude Code status line ({existing}); not installing the router-transparency status line. To show the router's picked model, pipe the session JSON through `{}` statusline from your own status line, or pass --no-statusline to silence this.",
            crate::CLI_BIN,
            shell_quote(&rld_bin.display().to_string())
        );
    }
}

fn install_statusline_settings_from_path(
    settings_path: PathBuf,
    home: Option<&Path>,
    rld_bin: &Path,
) -> io::Result<StatuslineInstallResult> {
    let settings_path = expand_user_path(settings_path, home);
    let mut settings = read_json_object(&settings_path);
    if let Some(existing) = settings.get("statusLine") {
        if existing.is_object() && !is_our_statusline(existing) {
            let existing_command = existing
                .get("command")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            return Ok(StatuslineInstallResult {
                settings_path,
                installed: false,
                conflict: true,
                existing_command,
            });
        }
    }

    settings
        .as_object_mut()
        .expect("read_json_object always returns a JSON object")
        .insert(
            "statusLine".to_owned(),
            serde_json::json!({
                "type": "command",
                "command": statusline_command(rld_bin),
                "padding": 0,
            }),
        );
    write_json_pretty(&settings_path, &settings)?;
    Ok(StatuslineInstallResult {
        settings_path,
        installed: true,
        conflict: false,
        existing_command: None,
    })
}

fn uninstall_statusline_settings_from_path(
    settings_path: PathBuf,
    home: Option<&Path>,
) -> io::Result<PathBuf> {
    let settings_path = expand_user_path(settings_path, home);
    if !settings_path.exists() {
        return Ok(settings_path);
    }
    let mut settings = read_json_object(&settings_path);
    if is_our_statusline(settings.get("statusLine").unwrap_or(&Value::Null)) {
        settings
            .as_object_mut()
            .expect("read_json_object always returns a JSON object")
            .remove("statusLine");
        write_json_pretty(&settings_path, &settings)?;
    }
    Ok(settings_path)
}

fn statusline_command(rld_bin: &Path) -> String {
    let binary = rld_bin.display().to_string();
    #[cfg(target_os = "windows")]
    let binary = binary.replace('\\', "/");
    format!("{} statusline", shell_quote(&binary))
}

fn is_our_statusline(status_line: &Value) -> bool {
    let Some(command) = status_line
        .get("command")
        .and_then(Value::as_str)
        .filter(|command| !command.is_empty())
    else {
        return false;
    };
    if SHELL_COMPOSE_OPERATORS
        .iter()
        .any(|operator| command.contains(operator))
    {
        return false;
    }
    let is_rld_statusline = split_shell_words(command).is_some_and(|tokens| {
        if tokens.len() != 2 || tokens[1] != "statusline" {
            return false;
        }
        let Some(name) = Path::new(&tokens[0])
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return false;
        };
        let branded_windows_name = format!("{}.exe", crate::DAEMON_BIN);
        name == crate::DAEMON_BIN
            || name == branded_windows_name
            || name == "rld"
            || name == "rld.exe"
    });
    is_rld_statusline
        || LEGACY_STATUSLINE_MARKERS
            .iter()
            .any(|marker| command.contains(marker))
}

fn split_shell_words(command: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = command.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = chars.next() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\\' if !in_single => {
                current.push(chars.next()?);
            }
            ch if ch.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if in_single || in_double {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| resolve_executable_in_dir(&dir, name))
}

/// Resolve `name` to an executable inside `dir`, honoring Windows extensions.
///
/// On Windows an executable on PATH is stored with an extension (claude.exe
/// from the native installer, claude.cmd from npm) while callers pass the bare
/// stem, so we resolve it the way the shell does. The extension probe only runs
/// when `name` has no extension of its own, leaving explicit names unchanged;
/// non-Windows behavior is a plain existence check as before.
fn resolve_executable_in_dir(dir: &Path, name: &str) -> Option<PathBuf> {
    let candidate = dir.join(name);
    if candidate.exists() {
        return Some(candidate);
    }
    #[cfg(windows)]
    if Path::new(name).extension().is_none() {
        for ext in ["exe", "cmd", "bat", "com"] {
            let candidate = dir.join(format!("{name}.{ext}"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Resolve the `claude` binary, falling back to the canonical installer
/// locations when it is absent from `PATH`.
///
/// Menu-bar launches run `claude` through a non-interactive login shell
/// (`zsh -lc`), which sources `.zprofile`/`.zlogin` but not `.zshrc`. Both the
/// native installer (`~/.local/bin`) and the legacy local installer
/// (`~/.claude/local`) wire `claude` onto `PATH` from `.zshrc`, so a PATH-only
/// lookup misses it there even though an interactive terminal finds it fine.
/// Probing the install dirs makes resolution independent of shell-init quirks.
fn find_claude_bin(home: &Path) -> Option<PathBuf> {
    find_on_path("claude").or_else(|| first_existing_file(claude_fallback_candidates(home)))
}

fn claude_fallback_candidates(home: &Path) -> [PathBuf; 2] {
    [
        home.join(".local/bin/claude"),
        home.join(".claude/local/claude"),
    ]
}

fn first_existing_file(candidates: impl IntoIterator<Item = PathBuf>) -> Option<PathBuf> {
    candidates.into_iter().find(|candidate| candidate.is_file())
}

fn add_claude_bin_dir_to_child_path(command: &mut Command, claude_bin: &Path) {
    let Some(claude_dir) = claude_bin.parent() else {
        return;
    };
    let mut paths = env::var_os("PATH")
        .map(|raw| env::split_paths(&raw).collect::<Vec<_>>())
        .unwrap_or_default();
    if !paths.iter().any(|path| path == claude_dir) {
        paths.insert(0, claude_dir.to_owned());
    }
    if let Ok(joined) = env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

fn read_json_object(path: &Path) -> Value {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return Value::Object(serde_json::Map::new());
    };
    serde_json::from_str::<Value>(&contents)
        .ok()
        .filter(Value::is_object)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

fn write_json_pretty(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let contents = serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned()) + "\n";
    std::fs::write(path, contents)
}

fn set_user_private_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Some(parent) = path.parent() {
            let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
        }
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
}

fn default_settings_path(home: Option<&Path>) -> PathBuf {
    std::env::var_os("CLAUDE_SETTINGS_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| default_home_path(home, DEFAULT_CLAUDE_SETTINGS_SUFFIX))
}

fn default_home_path(home: Option<&Path>, suffix: &str) -> PathBuf {
    home.map_or_else(|| PathBuf::from(suffix), |home| home.join(suffix))
}

fn expand_user_path(path: PathBuf, home: Option<&Path>) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path;
    };
    if raw == "~" {
        return home.map_or(path, Path::to_path_buf);
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home.map_or(path.clone(), |home| home.join(rest));
    }
    path
}

#[cfg(test)]
mod refusal_recovery_env_tests {
    use super::*;

    const FALLBACK: &str = "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK";
    const RETRY: &str = "CLAUDE_CODE_DISABLE_REFUSAL_RETRY";

    fn values(
        mode: RoutingMode,
        fallback: Option<&str>,
        retry: Option<&str>,
    ) -> Vec<(&'static str, Option<String>)> {
        refusal_recovery_values(mode, |key| {
            match key {
                FALLBACK => fallback,
                RETRY => retry,
                _ => None,
            }
            .map(OsString::from)
        })
    }

    fn child_env(values: &[(&'static str, Option<String>)]) -> Vec<(String, Option<String>)> {
        let mut command = Command::new("claude");
        apply_refusal_recovery_env(&mut command, values);
        command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect()
    }

    #[test]
    fn routed_main_conversation_turns_each_switch_off_unless_the_user_set_it() {
        // Each switch on its own: unset and empty become "1"; a non-empty value is kept.
        for mode in [RoutingMode::Override, RoutingMode::Proxy] {
            for (inherited, expected) in [
                (None, "1"),
                (Some(""), "1"),
                (Some("0"), "0"),
                (Some("1"), "1"),
            ] {
                assert_eq!(
                    values(mode, inherited, None),
                    vec![
                        (FALLBACK, Some(expected.to_owned())),
                        (RETRY, Some("1".to_owned()))
                    ]
                );
                assert_eq!(
                    values(mode, None, inherited),
                    vec![
                        (FALLBACK, Some("1".to_owned())),
                        (RETRY, Some(expected.to_owned()))
                    ]
                );
            }
        }
    }

    #[test]
    fn subagent_only_routing_leaves_claude_code_defaults() {
        // Including implicit local routing, which resolves to subagents-only.
        let mode = effective_routing_mode(RoutingMode::Proxy, true, false);
        assert_eq!(mode, RoutingMode::ProxySubagents);
        assert_eq!(
            values(mode, None, Some("")),
            vec![(FALLBACK, None), (RETRY, None)]
        );
        assert_eq!(
            values(mode, Some("0"), None),
            vec![(FALLBACK, Some("0".to_owned())), (RETRY, None)]
        );
    }

    #[test]
    fn proxy_modes_turn_message_threads_off_unless_the_user_set_it() {
        let threads = |mode, user: Option<&str>| {
            launch_env_switches(mode, |key| {
                (key == MESSAGE_THREADS_ENV)
                    .then_some(user)
                    .flatten()
                    .map(OsString::from)
            })
            .into_iter()
            .find(|(key, _)| *key == MESSAGE_THREADS_ENV)
            .and_then(|(_, value)| value)
        };
        for mode in [RoutingMode::Proxy, RoutingMode::ProxySubagents] {
            assert_eq!(threads(mode, None).as_deref(), Some("0"));
            assert_eq!(threads(mode, Some("")).as_deref(), Some("0"));
            assert_eq!(threads(mode, Some("1")).as_deref(), Some("1"));
        }
        // `--via env` is not first-party to Claude Code, so it never threads.
        assert_eq!(threads(RoutingMode::Override, None), None);
    }

    /// The real launcher, `--via env` against the hosted prod router: with a
    /// stored key and an explicit compact window it needs no network and starts
    /// no process, so the child command it builds can be read directly. Each
    /// switch is driven independently through the inherited env.
    #[tokio::test]
    async fn the_launcher_sets_the_switches_on_the_child_it_builds() {
        static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _guard = ENV_LOCK.lock().await;
        let home =
            std::env::temp_dir().join(format!("rayline-cli-refusal-launch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let credentials = home
            .join(".config")
            .join(crate::CONFIG_DIR)
            .join("credentials.json");
        std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
        std::fs::write(
            &credentials,
            r#"{"router_keys":{"prod":{"apiKey":"rlk-test"}}}"#,
        )
        .unwrap();
        let request = RunRequest {
            env_name: Some("prod".to_owned()),
            auth_token: None,
            args: Vec::new(),
            model: None,
            local_provider: None,
            local_provider_model: None,
            auto_compact_window: Some(180_000),
            local_router: false,
            isolated: false,
            local_injector_port: None,
            routing_mode: RoutingMode::Override,
            route_scope_explicit: true,
            route_statusline_enabled: false,
            diagnose: false,
            upstream_ca_path: None,
            router_config_path: None,
            config_path: None,
            root_env_explicit: true,
        };
        let saved: Vec<_> = [FALLBACK, RETRY, MESSAGE_THREADS_ENV, CLAUDE_CONFIG_DIR_ENV]
            .map(|key| (key, std::env::var_os(key)))
            .into();
        // A private Claude config dir, so no real daemon on this machine is inspected.
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe { std::env::set_var(CLAUDE_CONFIG_DIR_ENV, home.join(".claude")) };
        let mut seen = Vec::new();
        // The message-threads switch goes through the same launch switches: a
        // user value is carried onto the child (and so into daemon reuse).
        for (fallback, retry, threads) in [(None, Some(""), None), (Some("0"), None, Some("1"))] {
            for (key, value) in [
                (FALLBACK, fallback),
                (RETRY, retry),
                (MESSAGE_THREADS_ENV, threads),
            ] {
                // SAFETY: serialized by ENV_LOCK; restored below.
                match value {
                    Some(value) => unsafe { std::env::set_var(key, value) },
                    None => unsafe { std::env::remove_var(key) },
                }
            }
            let command =
                run_command_from_home(&request, &home, PathBuf::from("/nonexistent/claude"))
                    .await
                    .expect("env-mode launch builds its command");
            let env: std::collections::BTreeMap<String, Option<String>> = command
                .get_envs()
                .map(|(key, value)| {
                    (
                        key.to_string_lossy().into_owned(),
                        value.map(|value| value.to_string_lossy().into_owned()),
                    )
                })
                .collect();
            seen.push((
                env.get(FALLBACK).cloned().flatten(),
                env.get(RETRY).cloned().flatten(),
                env.get(MESSAGE_THREADS_ENV).cloned().flatten(),
            ));
        }
        for (key, value) in saved {
            // SAFETY: serialized by ENV_LOCK.
            match value {
                Some(value) => unsafe { std::env::set_var(key, value) },
                None => unsafe { std::env::remove_var(key) },
            }
        }
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            seen,
            vec![
                (Some("1".to_owned()), Some("1".to_owned()), None),
                (
                    Some("0".to_owned()),
                    Some("1".to_owned()),
                    Some("1".to_owned())
                ),
            ]
        );
    }

    #[test]
    fn the_child_command_carries_the_effective_values() {
        assert_eq!(
            child_env(&values(RoutingMode::Proxy, Some(""), Some("0"))),
            vec![
                (FALLBACK.to_owned(), Some("1".to_owned())),
                (RETRY.to_owned(), Some("0".to_owned()))
            ]
        );
        // Subagents-only: nothing is set on the child.
        assert!(child_env(&values(RoutingMode::ProxySubagents, None, None)).is_empty());
    }
}

#[cfg(test)]
mod router_auto_compact_window_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A `/v1/settings` stub answering one request with `status` and `body`;
    /// the join handle yields the request line it received.
    async fn settings_stub(
        status: &'static str,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let read = stream.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..read]).into_owned();
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            request.lines().next().unwrap_or_default().to_owned()
        });
        (base, served)
    }

    async fn window(
        status: &'static str,
        body: &'static str,
    ) -> (Result<Option<String>, String>, String) {
        let (base, served) = settings_stub(status, body).await;
        let window =
            router_auto_compact_window("dev", &base, Some("token"), "key", "rayline-router[1m]")
                .await
                .map_err(|error| error.to_string());
        (window, served.await.unwrap())
    }

    #[tokio::test]
    async fn asks_the_router_with_the_launch_model_and_returns_its_window() {
        let (window, request_line) =
            window("200 OK", r#"{"settings":{},"autoCompactWindow":262144}"#).await;
        assert_eq!(window, Ok(Some("262144".to_owned())));
        assert!(
            request_line
                .starts_with("GET /v1/settings?model=rayline-router%5B1m%5D&client_sizes=1 "),
            "{request_line}"
        );
    }

    #[tokio::test]
    async fn a_null_or_absent_window_leaves_the_window_to_claude_code() {
        for body in [
            r#"{"settings":{},"autoCompactWindow":null}"#,
            r#"{"settings":{}}"#,
        ] {
            let (window, _) = window("200 OK", body).await;
            assert_eq!(window, Ok(None), "{body}");
        }
    }

    #[tokio::test]
    async fn fails_the_launch_on_a_router_error_or_a_malformed_window() {
        for (status, body, reason) in [
            (
                "400 Bad Request",
                r#"{"error":"Unknown model: rayline-router[1m]"}"#,
                "Unknown model: rayline-router[1m]",
            ),
            (
                "200 OK",
                r#"{"settings":{},"autoCompactWindow":"950000"}"#,
                r#"autoCompactWindow "950000" is not a positive integer"#,
            ),
            (
                "200 OK",
                r#"{"settings":{},"autoCompactWindow":0}"#,
                "autoCompactWindow 0 is not a positive integer",
            ),
        ] {
            let (window, _) = window(status, body).await;
            let error = window.unwrap_err();
            assert!(error.contains(reason), "{error}");
            assert!(error.contains("--auto-compact-window <tokens>"), "{error}");
        }
    }
}

#[cfg(test)]
mod claude_command_tests {
    use super::*;

    /// A router key in the launcher's environment must not reach Claude Code,
    /// whose Bash tool hands its environment to every command it runs.
    #[cfg(unix)]
    #[test]
    fn claude_session_never_inherits_a_router_key() {
        let vars = ["RAYLINE_ROUTER_API_KEY", crate::harness::KEY_ENV];
        let previous: Vec<_> = vars.iter().map(env::var_os).collect();
        for var in vars {
            // SAFETY: tests run with --test-threads=1 (AGENTS.md), so no other
            // thread reads the environment concurrently.
            unsafe { env::set_var(var, "rlk-test-not-a-real-key") };
        }
        let output = claude_command(Path::new("/usr/bin/env"))
            .output()
            .expect("run env");
        for (var, value) in vars.iter().zip(previous) {
            // SAFETY: as above.
            match value {
                Some(value) => unsafe { env::set_var(var, value) },
                None => unsafe { env::remove_var(var) },
            }
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains("rlk-test-not-a-real-key"),
            "the Claude Code environment still carries a router key"
        );
    }
}

#[cfg(test)]
mod claude_code_model_alias_tests {
    use super::*;

    #[test]
    fn recognizes_claude_code_aliases() {
        for model in [
            "opus",
            "sonnet",
            "haiku",
            "fable",
            "best",
            "default",
            "opusplan",
            "opus[1m]",
            "sonnet[1m]",
            "fable[1m]",
            "opusplan[1m]",
            " Opus ",
            "SONNET[1M]",
        ] {
            assert!(is_claude_code_model_alias(model), "{model}");
        }
    }

    #[test]
    fn concrete_and_virtual_models_are_not_aliases() {
        for model in [
            "claude-opus-4-8",
            "claude-opus-4-8[1m]",
            "claude-sonnet-5",
            "rayline-router",
            "opus-4",
            "",
        ] {
            assert!(!is_claude_code_model_alias(model), "{model}");
        }
    }

    #[test]
    fn unpinned_window_clears_an_inherited_env_value() {
        let mut command = Command::new("true");
        command.env(AUTO_COMPACT_WINDOW_ENV, "");
        set_auto_compact_window_env(&mut command, None);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == AUTO_COMPACT_WINDOW_ENV && value.is_none())
        );

        set_auto_compact_window_env(&mut command, Some("180000"));
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == AUTO_COMPACT_WINDOW_ENV
                    && value == Some(std::ffi::OsStr::new("180000")))
        );
    }
}

#[cfg(test)]
mod proxy_routing_mode_name_tests {
    use super::*;

    #[test]
    fn proxy_maps_to_all() {
        assert_eq!(
            proxy_routing_mode_name(RoutingMode::Proxy),
            crate::router::PROXY_ROUTING_MODE_ALL
        );
    }

    #[test]
    fn proxy_subagents_maps_to_selective_subagents() {
        assert_eq!(
            proxy_routing_mode_name(RoutingMode::ProxySubagents),
            crate::router::PROXY_ROUTING_MODE_SELECTIVE_SUBAGENTS
        );
    }
}

#[cfg(test)]
mod local_provider_tests {
    use super::*;

    fn config_with_recommended_pick(
        mode: crate::local_model::LocalModelMode,
    ) -> crate::local_model::LocalModelConfig {
        crate::local_model::LocalModelConfig {
            mode,
            provider: Some("ollama".to_owned()),
            protocol: Some("openai_chat".to_owned()),
            base_url: Some("http://localhost:11434".to_owned()),
            model: Some("qwen3-coder:30b".to_owned()),
            model_id: Some("managed-model".to_owned()),
            model_repo: Some("rayline/managed".to_owned()),
            model_file: Some("model.gguf".to_owned()),
            model_revision: Some("abc123".to_owned()),
            model_sha256: Some("0".repeat(64)),
            custom_endpoints: Vec::new(),
        }
    }

    #[test]
    fn llamacpp_provider_coerces_custom_config_with_managed_pick() {
        let cfg = config_with_recommended_pick(crate::local_model::LocalModelMode::Custom);

        let cfg = coerce_llamacpp_config(cfg).unwrap();

        assert_eq!(cfg.mode, crate::local_model::LocalModelMode::Recommended);
        assert_eq!(cfg.provider.as_deref(), Some("llamacpp"));
        assert_eq!(cfg.protocol.as_deref(), Some("anthropic_messages"));
        assert_eq!(cfg.model_id.as_deref(), Some("managed-model"));
    }

    #[test]
    fn llamacpp_provider_rejects_custom_only_config() {
        let mut cfg = config_with_recommended_pick(crate::local_model::LocalModelMode::Custom);
        cfg.model_id = None;
        cfg.model_repo = None;
        cfg.model_file = None;
        cfg.model_revision = None;
        cfg.model_sha256 = None;

        assert!(coerce_llamacpp_config(cfg).is_none());
    }

    #[test]
    fn provider_config_cloud_fallback_requires_shared_proxy_provider() {
        let provider_cfg = config_with_recommended_pick(crate::local_model::LocalModelMode::Custom);
        let mut non_provider_cfg = provider_cfg.clone();
        non_provider_cfg.protocol = Some("anthropic_messages".to_owned());

        assert!(provider_config_cloud_fallback(
            false,
            RoutingMode::Proxy,
            false,
            Some(&provider_cfg),
        ));
        assert!(provider_config_cloud_fallback(
            false,
            RoutingMode::ProxySubagents,
            false,
            Some(&provider_cfg),
        ));
        assert!(!provider_config_cloud_fallback(
            true,
            RoutingMode::Proxy,
            false,
            Some(&provider_cfg),
        ));
        assert!(!provider_config_cloud_fallback(
            false,
            RoutingMode::Override,
            false,
            Some(&provider_cfg),
        ));
        assert!(!provider_config_cloud_fallback(
            false,
            RoutingMode::Proxy,
            true,
            Some(&provider_cfg),
        ));
        assert!(!provider_config_cloud_fallback(
            false,
            RoutingMode::Proxy,
            false,
            Some(&non_provider_cfg),
        ));
    }
}

#[cfg(test)]
mod isolated_needs_claude_login_tests {
    use super::*;

    #[test]
    fn env_mode_skips_claude_login() {
        // Env mode authenticates via ANTHROPIC_* env vars; requiring a separate
        // Claude OAuth login would wrongly reject valid non-interactive runs.
        assert!(!isolated_needs_claude_login(RoutingMode::Override));
    }

    #[test]
    fn proxy_modes_require_claude_login() {
        assert!(isolated_needs_claude_login(RoutingMode::Proxy));
        assert!(isolated_needs_claude_login(RoutingMode::ProxySubagents));
    }
}

#[cfg(test)]
mod claude_json_without_auth_tests {
    use super::*;

    #[test]
    fn strips_auth_keys_but_keeps_trust_and_mcp() {
        let raw = r#"{
            "oauthAccount": {"emailAddress": "me@example.com"},
            "customApiKeyResponses": {"approved": ["k"]},
            "primaryApiKey": "sk-ant-secret",
            "hasCompletedOnboarding": true,
            "projects": {"/repo": {"hasTrustDialogAccepted": true, "mcpServers": {}}},
            "mcpServers": {"local": {"command": "x"}}
        }"#;
        let out = claude_json_without_auth(raw).expect("valid object");
        assert!(!out.contains("sk-ant-secret"), "API key leaked: {out}");
        let value: Value = serde_json::from_str(&out).unwrap();
        let object = value.as_object().unwrap();

        // Auth state removed.
        assert!(!object.contains_key("oauthAccount"));
        assert!(!object.contains_key("customApiKeyResponses"));
        assert!(!object.contains_key("primaryApiKey"));
        // Trust, MCP and onboarding state preserved.
        assert!(object.contains_key("projects"));
        assert!(object.contains_key("mcpServers"));
        assert_eq!(
            object.get("hasCompletedOnboarding"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn returns_none_for_non_object() {
        assert!(claude_json_without_auth("[1, 2, 3]").is_none());
        assert!(claude_json_without_auth("not json").is_none());
    }
}

#[cfg(test)]
mod implicit_local_routing_tests {
    use super::*;

    // ── Finding 2: env (Override) is cloud-only and never engages local ──

    #[test]
    fn env_mode_never_engages_implicit_local() {
        // Even with the account toggle on and no isolation, env mode stays cloud.
        assert!(!implicit_local_engages(
            RoutingMode::Override,
            false,
            true,
            false
        ));
    }

    #[test]
    fn proxy_mode_engages_implicit_local_when_toggle_on() {
        assert!(implicit_local_engages(
            RoutingMode::Proxy,
            false,
            true,
            false
        ));
    }

    #[test]
    fn isolation_blocks_implicit_local() {
        assert!(!implicit_local_engages(
            RoutingMode::Proxy,
            true,
            true,
            false
        ));
    }

    #[test]
    fn toggle_off_blocks_implicit_local() {
        assert!(!implicit_local_engages(
            RoutingMode::Proxy,
            false,
            false,
            false
        ));
    }

    #[test]
    fn config_suppresses_implicit_local() {
        // With a `--config` in play, the config is the sole may-local driver — the
        // account-toggle implicit path is suppressed even with the toggle on and no
        // isolation. (This is why `Rc-Rc --config` stays pure cloud when local is ON.)
        assert!(!implicit_local_engages(
            RoutingMode::Proxy,
            false,
            true,
            true
        ));
        // And plain `raydock claude` (no config) still honors the toggle.
        assert!(implicit_local_engages(
            RoutingMode::Proxy,
            false,
            true,
            false
        ));
    }

    // ── Finding 1: local engagement defaults to subagents-only ──

    #[test]
    fn implicit_local_without_explicit_route_becomes_subagents() {
        // Parse time resolved cloud + route-all to Proxy; engaging local without
        // an explicit --route must mirror the explicit --local subagents default.
        assert_eq!(
            effective_routing_mode(RoutingMode::Proxy, true, false),
            RoutingMode::ProxySubagents
        );
    }

    #[test]
    fn implicit_local_with_explicit_route_all_is_respected() {
        assert_eq!(
            effective_routing_mode(RoutingMode::Proxy, true, true),
            RoutingMode::Proxy
        );
    }

    #[test]
    fn cloud_proxy_is_unchanged_without_local() {
        assert_eq!(
            effective_routing_mode(RoutingMode::Proxy, false, false),
            RoutingMode::Proxy
        );
    }

    #[test]
    fn explicit_local_subagents_is_unchanged() {
        // Explicit --local already resolved to ProxySubagents at parse time.
        assert_eq!(
            effective_routing_mode(RoutingMode::ProxySubagents, true, false),
            RoutingMode::ProxySubagents
        );
    }
}
