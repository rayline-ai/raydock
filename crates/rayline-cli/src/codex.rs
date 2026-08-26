use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::json;

const CODEX_SUBSCRIPTION_CONFIG_FILENAME: &str = "codex-subscription-router.json";
pub const CODEX_SUBSCRIPTION_ENDPOINT_ID: &str = "codex-subscription";
pub const CODEX_SUBSCRIPTION_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
/// Default model for the ChatGPT-subscription endpoint. Mirrors what plain
/// `codex` picks on a ChatGPT account with no configured model: the
/// top-priority entry of the backend's `/models`, which changes without notice.
pub const CODEX_SUBSCRIPTION_DEFAULT_MODEL: &str = "gpt-6-astra";
/// Models advertised on the subscription endpoint, default first. The router
/// treats this as the allowlist for explicit `--model` values on this endpoint
/// (an unlisted `--model` falls back to `routes.main`), so it must track the
/// backend's listed models.
pub const CODEX_SUBSCRIPTION_MODELS: [&str; 5] = [
    CODEX_SUBSCRIPTION_DEFAULT_MODEL,
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];
/// The internal virtual-marker model Codex is pointed at when the user picks no
/// `--model`. Codex must send *some* `model` on every Responses request, so this
/// sentinel stands in for "no explicit model — let the router's config decide"
/// (mirrors Claude Code's `rayline-router`). The local router recognizes it as a
/// marker and applies main/subagent routing rather than treating it as a real
/// model. Users select a real model (e.g. one of CODEX_SUBSCRIPTION_DEFAULT_MODEL)
/// via `--model` instead.
pub const CODEX_DEFAULT_SENTINEL_MODEL: &str = "rayline-local";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CodexAuthMode {
    Auto,
    None,
    Subscription,
}

impl CodexAuthMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "none" | "no-auth" | "local" => Some(Self::None),
            "subscription" | "codex-subscription" | "chatgpt" | "codex" => Some(Self::Subscription),
            _ => None,
        }
    }

    pub fn effective_for_run(self, config_path: Option<&PathBuf>) -> EffectiveCodexAuthMode {
        match self {
            // `auto` with a `--config`: the config drives auth. If its `main`
            // routes to the `subscription` sentinel, resolve to subscription so
            // the ChatGPT backend is materialized — otherwise the sentinel has no
            // concrete endpoint and 502-loops on a placeholder address. Any other
            // main (local/provider/cloud) needs no client auth.
            Self::Auto => match config_path {
                Some(path) if crate::router_config::config_main_is_passthrough(path) => {
                    EffectiveCodexAuthMode::Subscription
                }
                Some(_) => EffectiveCodexAuthMode::None,
                None => EffectiveCodexAuthMode::Subscription,
            },
            Self::Subscription => EffectiveCodexAuthMode::Subscription,
            Self::None => EffectiveCodexAuthMode::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectiveCodexAuthMode {
    None,
    Subscription,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    /// The `--model` the user picked, or `None` when they picked none. `None`
    /// means "route by config": the CLI stamps the internal virtual-marker
    /// sentinel ([`CODEX_DEFAULT_SENTINEL_MODEL`]) so the local router applies
    /// main/subagent routing. A `Some(real_model)` is passed through verbatim and
    /// resolved by the router's direct-model routing.
    pub model: Option<String>,
    pub config_path: Option<PathBuf>,
    pub auth_mode: CodexAuthMode,
    pub codex_args: Vec<OsString>,
    pub root_env_explicit: bool,
    /// Hosted environment override (`--env`), forwarded to router-key
    /// provisioning when the config routes a leg to the cloud router.
    pub env_name: Option<String>,
    /// Account bearer (`--auth-token`) used to mint/read the `rlk-` router key,
    /// mirroring the Claude path. `None` falls back to stored credentials or the
    /// interactive `rayline auth login` prompt.
    pub auth_token: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigureRequest {
    /// See [`RunRequest::model`]. `None` writes the virtual-marker sentinel.
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub auth_mode: CodexAuthMode,
}

/// Provision the hosted-RCR router key (`rlk-`) when a `--config` routes a leg to
/// the cloud router, so `rayline codex --config <cloud>` (Rc-Rc/Rc-K/Rl-K/…)
/// authenticates from a plain `rayline auth login` — no manual
/// `RAYLINE_ROUTER_API_KEY`. Mirrors the Claude path
/// ([`crate::claude::ensure_router_key`]).
///
/// An explicit `RAYLINE_ROUTER_API_KEY` is respected and passed through verbatim
/// as the override (no provisioning): otherwise a stored `rlk-` key would be
/// returned instead and shadow the user's explicit value. Passing it as the
/// override also bakes it into the daemon reuse metadata, so a key change
/// re-fingerprints and restarts a stale daemon rather than silently reusing one
/// (the daemon otherwise inherits the env var, so a first run still works, but
/// the resolved value must match what it actually uses).
///
/// Best-effort otherwise: returns `None` when there is no config, the config
/// routes to no cloud endpoint (subscription `A*` / local `L*` modes), or
/// provisioning fails (e.g. not logged in and non-interactive) — the daemon then
/// surfaces a clear error only if a cloud route is actually hit. When interactive
/// and logged out, `ensure_router_key` drives the same `rayline auth login`
/// prompt Claude uses.
pub(crate) async fn resolve_cloud_router_key(
    config_path: Option<&Path>,
    env_name: Option<&str>,
    auth_token: Option<&str>,
    root_env_explicit: bool,
) -> Option<String> {
    let path = config_path?;
    if !crate::router_config::config_routes_to_hosted_rcr(path) {
        return None;
    }
    if let Some(explicit) = std::env::var_os("RAYLINE_ROUTER_API_KEY")
        .and_then(|value| value.into_string().ok())
        .filter(|value| !value.is_empty())
    {
        return Some(explicit);
    }
    let home = dirs::home_dir()?;
    let env = crate::status::resolve_env(env_name, Some(home.as_path()));
    crate::claude::ensure_router_key(&env, &home, auth_token, root_env_explicit)
        .await
        .ok()
}

/// Resolve the config that drives a `rayline codex` / `rayline codex app` run.
///
/// A user-supplied `--config` is used verbatim. Otherwise the **default** is Rc-Rc
/// — route main + subagents to the hosted cloud RCR (native OpenAI Responses),
/// mirroring `rayline claude`'s route-all cloud default. We reuse the shared
/// default config (`router_config::default_config_json`, materialized via
/// `ensure_default_config`) and drive it through the ordinary `--config` codex
/// path, so cloud-key provisioning ([`resolve_cloud_router_key`]) and the native
/// materialization ([`crate::router_config::materialize_codex_config_for_local_router`])
/// are shared, not re-implemented.
///
/// The default config follows `--env`: its cloud endpoint is the selected
/// environment's router (the same `resolve_env` + `resolve_hosted_environment`
/// resolution `rayline claude` uses), and each environment has its own file, so
/// the router URL always matches the environment whose `rlk-` key
/// [`resolve_cloud_router_key`] provisions. An unknown environment, or one whose
/// router is not a hosted Rayline router, is an error rather than a silent
/// fallback to prod.
///
/// Only the default `--auth auto` opts in. Explicit `--auth subscription` keeps
/// the ChatGPT-subscription default, and `--auth none` keeps the local default —
/// both resolve their own zero-config shapes downstream in `start_from_cli`.
pub(crate) fn resolve_codex_config_path(
    home: &Path,
    config_path: Option<PathBuf>,
    auth_mode: CodexAuthMode,
    env_name: Option<&str>,
) -> io::Result<Option<PathBuf>> {
    if config_path.is_some() || auth_mode != CodexAuthMode::Auto {
        return Ok(config_path);
    }
    let env = crate::status::resolve_env(env_name, Some(home));
    let hosted = crate::status::resolve_hosted_environment(&env, Some(home))
        .map_err(|error| io::Error::other(error.to_string()))?;
    // Key provisioning and the Responses rewrite only recognise the hosted
    // router hosts, so a default pointed anywhere else could never authenticate.
    if !crate::router_config::endpoint_base_url_is_hosted_rcr(Some(&hosted.router_url)) {
        return Err(io::Error::other(format!(
            "environment '{env}' routes to {}, which the default codex config does not \
             support; pass --config with a router config for it",
            hosted.router_url
        )));
    }
    Ok(Some(crate::router_config::ensure_default_config(
        home,
        &env,
        &hosted.router_url,
    )?))
}

/// Resolve the real home used for the Rayline router config + key (not the Codex
/// app's isolated `CODEX_HOME`), then apply [`resolve_codex_config_path`].
pub(crate) fn resolve_codex_config_path_from_home(
    config_path: Option<PathBuf>,
    auth_mode: CodexAuthMode,
    env_name: Option<&str>,
) -> io::Result<Option<PathBuf>> {
    let home = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    resolve_codex_config_path(&home, config_path, auth_mode, env_name)
}

pub async fn run(mut request: RunRequest) -> ExitCode {
    match resolve_codex_config_path_from_home(
        request.config_path.take(),
        request.auth_mode,
        request.env_name.as_deref(),
    ) {
        Ok(path) => request.config_path = path,
        Err(error) => {
            eprintln!("Error: failed to prepare the default Rayline codex config: {error}");
            return ExitCode::from(1);
        }
    }
    let router_api_key_override = resolve_cloud_router_key(
        request.config_path.as_deref(),
        request.env_name.as_deref(),
        request.auth_token.as_deref(),
        request.root_env_explicit,
    )
    .await;
    let start_request = crate::router::RouterStartCliRequest {
        env_name: request.env_name.clone(),
        api_mode: crate::router::ROUTER_API_MODE_CODEX.to_owned(),
        proxy_routing_mode: crate::router::PROXY_ROUTING_MODE_ALL.to_owned(),
        config_path: request.config_path.clone(),
        codex_auth_mode: request.auth_mode,
        root_env_explicit: request.root_env_explicit,
        router_api_key_override,
    };
    match crate::router::start_from_cli(&start_request).await {
        Ok(_) => {
            eprintln!(
                "Rayline Codex router ready at http://127.0.0.1:{}/v1",
                crate::router::DEFAULT_LOCAL_ROUTER_PORT
            );
        }
        Err(error) => {
            eprintln!("Error: failed to start Rayline Codex router: {error}");
            return ExitCode::from(1);
        }
    }

    let base_url = format!(
        "http://127.0.0.1:{}/v1",
        crate::router::DEFAULT_LOCAL_ROUTER_PORT
    );
    let model = request
        .model
        .as_deref()
        .unwrap_or(CODEX_DEFAULT_SENTINEL_MODEL);
    let mut command = codex_command();
    command
        .arg("-c")
        .arg("model_provider=\"rayline\"")
        .arg("-c")
        .arg(format!("model={}", toml_string(model)))
        .arg("-c")
        .arg("model_providers.rayline.name=\"Rayline Local\"")
        .arg("-c")
        .arg(format!(
            "model_providers.rayline.base_url={}",
            toml_string(&base_url)
        ))
        .arg("-c")
        .arg("model_providers.rayline.wire_api=\"responses\"");
    if request
        .auth_mode
        .effective_for_run(request.config_path.as_ref())
        == EffectiveCodexAuthMode::Subscription
    {
        command
            .arg("-c")
            .arg("model_providers.rayline.requires_openai_auth=true")
            .arg("-c")
            .arg("forced_login_method=\"chatgpt\"");
        if let Some(version) = codex_cli_version_header() {
            command.arg("-c").arg(format!(
                "model_providers.rayline.http_headers.version={}",
                toml_string(&version)
            ));
        }
    }
    command.args(request.codex_args);

    exec_or_status(&mut command)
}

/// Keep the router key out of Codex's environment (#84). Codex reaches the
/// local router, which holds the key and authenticates the hosted leg itself,
/// so Codex never needs it. Codex snapshots its environment for the shells it
/// spawns (`$CODEX_HOME/shell_snapshots`), so a key it inherited would be
/// written to disk in plain text. The launcher reads an explicit
/// `RAYLINE_ROUTER_API_KEY` before this, when it starts the router.
pub(crate) fn withhold_router_key(command: &mut Command) {
    command.env_remove("RAYLINE_ROUTER_API_KEY");
}

/// A `codex` child process with the router key withheld (#84). Every Codex
/// spawn goes through here (the run, the version probe and the app launch),
/// so none can inherit the key.
pub(crate) fn codex_command() -> Command {
    let mut command = Command::new("codex");
    withhold_router_key(&mut command);
    command
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_owned())
}

/// The default Rayline OpenAI Responses base URL (the local router's `/v1`).
pub fn default_rayline_base_url() -> String {
    format!(
        "http://127.0.0.1:{}/v1",
        crate::router::DEFAULT_LOCAL_ROUTER_PORT
    )
}

/// Render the `[model_providers.rayline]` TOML block that points Codex at the
/// Rayline local router.
///
/// This is the single source of truth for the Codex-side provider config,
/// shared by `rayline codex configure` (writes it as `$CODEX_HOME/
/// rayline.config.toml`) and `rayline codex app` (writes it as the default
/// `config.toml` inside an isolated `CODEX_HOME`, since the desktop app-server
/// ignores `-c` overrides and only reads persistent config).
pub fn rayline_provider_config_toml(
    model: &str,
    base_url: &str,
    subscription_auth: bool,
) -> String {
    let http_headers = if subscription_auth {
        codex_cli_version_header()
            .map(|version| format!("http_headers = {{ version = {} }}\n", toml_string(&version)))
            .unwrap_or_default()
    } else {
        String::new()
    };
    format!(
        "model = {}\nmodel_provider = \"rayline\"\n{}\
\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = {}\nwire_api = \"responses\"\n{}{}",
        toml_string(model),
        if subscription_auth {
            "forced_login_method = \"chatgpt\"\n"
        } else {
            ""
        },
        toml_string(base_url),
        if subscription_auth {
            "requires_openai_auth = true\n"
        } else {
            ""
        },
        http_headers,
    )
}

pub fn configure(request: &ConfigureRequest) -> io::Result<String> {
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    fs::create_dir_all(&codex_home)?;
    let profile_path = codex_home.join("rayline.config.toml");
    let base_url = request
        .base_url
        .clone()
        .unwrap_or_else(default_rayline_base_url);
    let subscription_auth =
        request.auth_mode.effective_for_run(None) == EffectiveCodexAuthMode::Subscription;
    let model = request
        .model
        .as_deref()
        .unwrap_or(CODEX_DEFAULT_SENTINEL_MODEL);
    let contents = rayline_provider_config_toml(model, &base_url, subscription_auth);
    fs::write(&profile_path, contents)?;
    Ok(format!(
        "Wrote Codex Rayline profile: {}\nStart Rayline with `rayline router start --mode codex --auth {}`, then use Codex profile `rayline`.\nBase URL: {base_url}\n",
        profile_path.display(),
        if subscription_auth {
            "subscription"
        } else {
            "none"
        }
    ))
}

pub fn write_subscription_router_config(home: &Path, subagents_local: bool) -> io::Result<PathBuf> {
    let path = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join(CODEX_SUBSCRIPTION_CONFIG_FILENAME);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&subscription_router_config_json(subagents_local))
        .map_err(io::Error::other)?;
    fs::write(&path, body)?;
    Ok(path)
}

/// The zero-config `rayline codex --auth subscription` router config.
///
/// `main` always routes to the ChatGPT subscription. When `subagents_local` is
/// true (a usable on-device model is available), `subagent` routes to the local
/// adapter — the hybrid default that mirrors `rayline claude`'s "main on cloud,
/// subagents on-device" shape. When false (no local model), subagents stay on
/// the subscription, preserving the zero-setup all-subscription behavior.
///
/// The `model_routes` sentinels stay pinned to the subscription: they resolve
/// the sentinel `--model` on **main** turns. Subagent turns skip these sentinel
/// model_routes and follow `routes.subagent` (see `select_route` in the local
/// router), which is what makes the main≠subagent split take effect.
pub fn subscription_router_config_json(subagents_local: bool) -> serde_json::Value {
    let subscription = || {
        json!({
            "endpoint": CODEX_SUBSCRIPTION_ENDPOINT_ID,
            "model": CODEX_SUBSCRIPTION_DEFAULT_MODEL
        })
    };
    let subagent = if subagents_local {
        json!({ "endpoint": "local" })
    } else {
        subscription()
    };
    json!({
        "endpoints": [{
            "id": CODEX_SUBSCRIPTION_ENDPOINT_ID,
            "protocol": "openai_responses",
            "base_url": CODEX_SUBSCRIPTION_BASE_URL,
            "auth": "client_bearer",
            "models": CODEX_SUBSCRIPTION_MODELS
        }],
        "routes": {
            "main": subscription(),
            "subagent": subagent,
            "default": subscription(),
            "model_routes": {
                "rayline-codex": subscription(),
                "rayline-local": subscription()
            }
        }
    })
}

fn codex_cli_version_header() -> Option<String> {
    let output = codex_command().arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(version) = parse_codex_version_text(&stdout) {
        return Some(version);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_codex_version_text(&stderr)
}

fn parse_codex_version_text(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|part| {
            part.as_bytes()
                .first()
                .is_some_and(|byte| byte.is_ascii_digit())
                && part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
        })
        .map(ToOwned::to_owned)
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command) -> ExitCode {
    use std::os::unix::process::CommandExt;

    let error = command.exec();
    eprintln!("rayline: failed to exec codex: {error}");
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::{
        CODEX_SUBSCRIPTION_DEFAULT_MODEL, CODEX_SUBSCRIPTION_ENDPOINT_ID, CodexAuthMode,
        EffectiveCodexAuthMode, codex_command, parse_codex_version_text, resolve_cloud_router_key,
        resolve_codex_config_path, subscription_router_config_json, withhold_router_key,
    };
    use serde_json::json;
    use std::path::{Path, PathBuf};

    fn temp_home() -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let home = std::env::temp_dir().join(format!(
            "rayline-codex-home-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn write_config(name: &str, body: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rayline-codex-auth-test-{}-{name}-{unique}.json",
            std::process::id()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    // Router-key provisioning is gated on the config routing a leg to the hosted
    // RCR. The negative cases must resolve to `None` *without* touching
    // credentials or the network, so subscription (`A*`) and local (`L*`) Codex
    // runs never trigger a `rayline auth login` prompt.
    #[tokio::test]
    async fn cloud_router_key_none_without_config() {
        assert!(
            resolve_cloud_router_key(None, None, None, false)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn cloud_router_key_none_for_local_only_config() {
        // A purely local (ollama) config — no hosted-RCR endpoint — must not
        // provision a key (would otherwise prompt/fail on a machine with no login).
        let path = write_config(
            "local-only",
            r#"{"endpoints":[{"id":"ollama","protocol":"openai_chat","base_url":"http://127.0.0.1:11434/v1","models":["qwen"]}],"routes":{"main":{"endpoint":"ollama","model":"qwen"}}}"#,
        );
        assert!(
            resolve_cloud_router_key(Some(path.as_path()), None, None, false)
                .await
                .is_none()
        );
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn cloud_router_key_passes_through_explicit_env_var() {
        // An explicit `RAYLINE_ROUTER_API_KEY` is returned verbatim as the
        // override (not shadowed by a stored key, and not dropped on the
        // proxy-less Codex daemon start). Network-free: returns before
        // `ensure_router_key`.
        let path = write_config(
            "rrc-cloud",
            r#"{"endpoints":[{"id":"rayline-cloud","protocol":"openai_responses","base_url":"https://api.rayline.ai","models":["rayline-router"],"api_key_env":"RAYLINE_ROUTER_API_KEY","auth":"bearer"}],"routes":{"main":{"endpoint":"rayline-cloud","model":"rayline-router"}}}"#,
        );
        assert!(crate::router_config::config_routes_to_hosted_rcr(&path));

        let var = "RAYLINE_ROUTER_API_KEY";
        let previous = std::env::var_os(var);
        // SAFETY: this test owns the var for its duration and the suite runs
        // single-threaded (`--test-threads=1`); the original value is restored.
        unsafe { std::env::set_var(var, "rlk-user-explicit") };
        assert_eq!(
            resolve_cloud_router_key(Some(path.as_path()), None, None, false).await,
            Some("rlk-user-explicit".to_owned()),
            "an explicit RAYLINE_ROUTER_API_KEY must pass through as the override"
        );
        // SAFETY: see the note above.
        unsafe {
            match previous {
                Some(value) => std::env::set_var(var, value),
                None => std::env::remove_var(var),
            }
        }
        let _ = std::fs::remove_file(&path);
    }

    // Default `rayline codex` / `codex app` (no `--config`, auto auth) → Rc-Rc:
    // synthesize the shared default config (route everything to the hosted RCR).
    #[test]
    fn default_auto_no_config_resolves_to_rrc() {
        let home = temp_home();
        let resolved = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, None).unwrap();
        let path = resolved.expect("auto + no --config should synthesize a default config");
        // It's the shared default config, and it routes to the hosted cloud RCR
        // (Rc-Rc), not a subscription passthrough.
        assert!(crate::router_config::config_routes_to_hosted_rcr(&path));
        assert!(!crate::router_config::config_main_is_passthrough(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn explicit_config_is_used_verbatim() {
        let home = temp_home();
        let user = write_config("user-cfg", r#"{"endpoints":[],"routes":{}}"#);
        let resolved =
            resolve_codex_config_path(&home, Some(user.clone()), CodexAuthMode::Auto, None)
                .unwrap();
        assert_eq!(resolved, Some(user.clone()));
        let _ = std::fs::remove_file(&user);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn explicit_subscription_and_none_keep_their_own_default() {
        // Only `auto` opts into the Rc-Rc default; `subscription` (ChatGPT) and
        // `none` (local) keep `None` here and resolve their shapes downstream.
        let home = temp_home();
        assert_eq!(
            resolve_codex_config_path(&home, None, CodexAuthMode::Subscription, None).unwrap(),
            None
        );
        assert_eq!(
            resolve_codex_config_path(&home, None, CodexAuthMode::None, None).unwrap(),
            None
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    // `--env` scopes the default config: the generated file's cloud endpoint is
    // the selected environment's router, never prod for a non-prod key.

    const DEV_ROUTER_URL: &str = "https://api-dev.rayline.ai";

    fn write_dev_environment(home: &Path) {
        let dir = home.join(".config").join(crate::CONFIG_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            serde_json::to_vec(&json!({
                "environments": {
                    "dev": {
                        "router_url": DEV_ROUTER_URL,
                        "cli_auth_url": "https://dev.platform.rayline.ai/cli-auth"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
    }

    fn cloud_base_url(path: &Path) -> String {
        let cfg: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        cfg["endpoints"][0]["base_url"].as_str().unwrap().to_owned()
    }

    #[test]
    fn default_config_uses_the_selected_env_router_url() {
        let home = temp_home();
        write_dev_environment(&home);

        let path = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("dev"))
            .unwrap()
            .expect("auto + no --config should synthesize a default config");

        assert_eq!(
            path,
            crate::router_config::default_config_path_for_env(&home, "dev")
        );
        assert_ne!(path, crate::router_config::default_config_path(&home));
        assert_eq!(cloud_base_url(&path), DEV_ROUTER_URL);
        assert!(crate::router_config::config_routes_to_hosted_rcr(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn existing_prod_default_is_not_reused_for_another_env() {
        // The original bug: a prod `router.json` from an earlier run was reused by
        // `--env dev`, sending the dev key to the prod router.
        let home = temp_home();
        write_dev_environment(&home);
        let prod = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, None)
            .unwrap()
            .unwrap();
        let prod_before = std::fs::read(&prod).unwrap();
        assert_eq!(cloud_base_url(&prod), crate::ROUTER_PROD_URL);

        let dev = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("dev"))
            .unwrap()
            .unwrap();
        assert_ne!(dev, prod);
        assert_eq!(cloud_base_url(&dev), DEV_ROUTER_URL);
        // Switching back to prod finds its own file, untouched.
        let prod_again = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("prod"))
            .unwrap()
            .unwrap();
        assert_eq!(prod_again, prod);
        assert_eq!(std::fs::read(&prod).unwrap(), prod_before);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn env_default_config_keeps_user_edits() {
        let home = temp_home();
        write_dev_environment(&home);
        let dev = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("dev"))
            .unwrap()
            .unwrap();
        let edited = r#"{"endpoints":[],"routes":{}}"#;
        std::fs::write(&dev, edited).unwrap();

        let again = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("dev"))
            .unwrap()
            .unwrap();
        assert_eq!(again, dev);
        assert_eq!(std::fs::read_to_string(&dev).unwrap(), edited);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn unknown_env_is_an_error_not_a_prod_default() {
        let home = temp_home();
        let error = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("staging"))
            .unwrap_err();
        assert!(error.to_string().contains("staging"), "{error}");
        assert!(!crate::router_config::default_config_path(&home).exists());
        assert!(!crate::router_config::default_config_path_for_env(&home, "staging").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn env_with_a_non_hosted_router_is_an_error() {
        // The codex default only knows how to reach the hosted router hosts;
        // any other router_url would get no key and no Responses rewrite.
        let home = temp_home();
        let dir = home.join(".config").join(crate::CONFIG_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("settings.json"),
            serde_json::to_vec(&json!({
                "environments": {
                    "staging": {
                        "router_url": "https://router.example.test",
                        "cli_auth_url": "https://platform.example.test/cli-auth"
                    }
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let error = resolve_codex_config_path(&home, None, CodexAuthMode::Auto, Some("staging"))
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("router.example.test"), "{message}");
        assert!(message.contains("--config"), "{message}");
        assert!(!crate::router_config::default_config_path_for_env(&home, "staging").exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn explicit_config_is_used_verbatim_under_another_env() {
        let home = temp_home();
        let user = write_config("user-cfg-env", r#"{"endpoints":[],"routes":{}}"#);
        let resolved =
            resolve_codex_config_path(&home, Some(user.clone()), CodexAuthMode::Auto, Some("dev"))
                .unwrap();
        assert_eq!(resolved, Some(user.clone()));
        assert!(!crate::router_config::default_config_path_for_env(&home, "dev").exists());
        let _ = std::fs::remove_file(&user);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn parses_codex_cli_version_output() {
        assert_eq!(
            parse_codex_version_text("codex-cli 0.142.0\n").as_deref(),
            Some("0.142.0")
        );
        assert_eq!(
            parse_codex_version_text("codex 1.2.3-beta.1+build\n").as_deref(),
            Some("1.2.3-beta.1+build")
        );
        assert_eq!(parse_codex_version_text("codex-cli\n"), None);
    }

    #[test]
    fn subscription_config_all_subscription_when_no_local_model() {
        // Fallback shape (no usable local model): main AND subagent → subscription.
        let cfg = subscription_router_config_json(false);
        assert_eq!(
            cfg["routes"]["main"]["endpoint"],
            CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        assert_eq!(
            cfg["routes"]["main"]["model"],
            CODEX_SUBSCRIPTION_DEFAULT_MODEL
        );
        assert_eq!(
            cfg["routes"]["subagent"]["endpoint"],
            CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        // Sentinel model_routes always pin main to the subscription.
        for m in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][m]["endpoint"],
                CODEX_SUBSCRIPTION_ENDPOINT_ID
            );
        }
    }

    #[test]
    fn subscription_config_hybrid_routes_subagents_local() {
        // Hybrid shape (usable local model): main → subscription, subagent → local.
        let cfg = subscription_router_config_json(true);
        assert_eq!(
            cfg["routes"]["main"]["endpoint"],
            CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        assert_eq!(cfg["routes"]["subagent"]["endpoint"], "local");
        // No model pinned on the local subagent route — the router fills the
        // configured local model id.
        assert!(cfg["routes"]["subagent"].get("model").is_none());
        // Sentinels still pin MAIN turns to the subscription; subagent turns skip
        // them (see the local router's select_route).
        for m in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][m]["endpoint"],
                CODEX_SUBSCRIPTION_ENDPOINT_ID
            );
        }
    }

    #[test]
    fn auto_without_config_resolves_to_subscription() {
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(None),
            EffectiveCodexAuthMode::Subscription
        );
    }

    #[test]
    fn auto_with_subscription_main_config_resolves_to_subscription() {
        // Regression: `--auth auto` + a `main: subscription` config must resolve to
        // subscription (materializing the ChatGPT backend) instead of "no auth",
        // which routed the sentinel to a dead placeholder and 502-looped.
        let path = write_config(
            "sub-main",
            r#"{"routes":{"main":{"endpoint":"subscription"},"subagent":{"endpoint":"local"}}}"#,
        );
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::Subscription
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn auto_with_absent_main_resolves_to_subscription() {
        // An absent `routes.main` is treated as the subscription passthrough.
        let path = write_config("no-main", r#"{"routes":{"subagent":{"endpoint":"local"}}}"#);
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::Subscription
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn auto_with_non_subscription_main_config_resolves_to_none() {
        // A local/provider main needs no client auth — auto stays "none".
        let path = write_config(
            "local-main",
            r#"{"endpoints":[{"id":"ollama","protocol":"openai_chat","base_url":"http://127.0.0.1:11434/v1","models":["qwen"]}],"routes":{"main":{"endpoint":"ollama","model":"qwen"}}}"#,
        );
        assert_eq!(
            CodexAuthMode::Auto.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::None
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn explicit_modes_ignore_config() {
        let path = write_config(
            "local-main2",
            r#"{"endpoints":[{"id":"ollama","protocol":"openai_chat","base_url":"http://127.0.0.1:11434/v1","models":["qwen"]}],"routes":{"main":{"endpoint":"ollama","model":"qwen"}}}"#,
        );
        // `--auth subscription` always subscription, even against a local-main config.
        assert_eq!(
            CodexAuthMode::Subscription.effective_for_run(Some(&path)),
            EffectiveCodexAuthMode::Subscription
        );
        // `--auth none` always none, even against a subscription-main config.
        let sub = write_config(
            "sub-main2",
            r#"{"routes":{"main":{"endpoint":"subscription"}}}"#,
        );
        assert_eq!(
            CodexAuthMode::None.effective_for_run(Some(&sub)),
            EffectiveCodexAuthMode::None
        );
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(sub);
    }

    /// #84: every Codex child is built by `codex_command`, which withholds the
    /// router key, so no spawn site can forget it: the version probe and the
    /// app launch included.
    #[test]
    fn every_codex_spawn_goes_through_codex_command() {
        let sources = [
            ("codex.rs", include_str!("codex.rs")),
            ("codex_app.rs", include_str!("codex_app.rs")),
        ];
        let needle = concat!("Command::new(", "\"codex\")");
        let total: usize = sources
            .iter()
            .map(|(_, text)| text.matches(needle).count())
            .sum();
        assert_eq!(
            total, 1,
            "a Codex child is spawned outside codex_command (found {total} spawn sites)"
        );
    }

    #[test]
    fn codex_command_withholds_the_router_key() {
        let command = codex_command();
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "RAYLINE_ROUTER_API_KEY" && value.is_none()),
            "codex_command does not remove RAYLINE_ROUTER_API_KEY"
        );
    }

    /// #84: a router key in the launcher's environment must not reach Codex,
    /// which snapshots its environment to disk for the shells it spawns.
    #[cfg(unix)]
    #[test]
    fn codex_child_never_inherits_the_router_key() {
        let var = "RAYLINE_ROUTER_API_KEY";
        let previous = std::env::var_os(var);
        unsafe { std::env::set_var(var, "rlk-test-not-a-real-key") };
        let mut command = std::process::Command::new("/usr/bin/env");
        withhold_router_key(&mut command);
        let output = command.output().expect("run env");
        match previous {
            Some(value) => unsafe { std::env::set_var(var, value) },
            None => unsafe { std::env::remove_var(var) },
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains(var),
            "the child environment still carries {var}"
        );
    }
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to run codex: {error}");
            ExitCode::from(127)
        }
    }
}
