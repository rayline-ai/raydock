use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::claude::{
    AUTO_COMPACT_WINDOW_ENV, CLAUDE_CONFIG_DIR_ENV, RAYLINE_ENV_NAME_ENV, ROUTING_MODE_PROXY,
    ROUTING_MODE_PROXY_PASSTHROUGH, ROUTING_MODE_PROXY_SUBAGENTS, RoutingMode, RunError,
    is_proxy_routing_mode, routing_mode_name,
};

const STOP_COMMAND: &str = "claude daemon stop --any";
const RAYLINE_CLAUDE_LAUNCH_TTL_SECONDS: i64 = 24 * 60 * 60;
const DAEMON_SPAWN_MAX_DELAY_SECONDS: i64 = 60;

pub struct RequestSpec<'a> {
    pub env_name: &'a str,
    pub routing_mode: RoutingMode,
    pub auto_compact_window: &'a str,
    pub args: &'a [OsString],
    pub requested_local_port: Option<u16>,
    pub requested_proxy_port: Option<u16>,
}

pub struct LaunchPreflight<'a> {
    pub home: &'a Path,
    pub config_dir: &'a Path,
    pub request: &'a RequestSpec<'a>,
    pub claude_bin: &'a Path,
    pub allow_isolated: bool,
}

pub struct ProceedPreflight {
    pub preserve_spawned_by_pid: Option<u32>,
}

pub enum PreflightOutcome {
    Proceed(ProceedPreflight),
    SwitchToIsolated,
}

pub struct LaunchRecord<'a> {
    pub home: &'a Path,
    pub pid: u32,
    pub request: &'a RequestSpec<'a>,
    pub preserve_spawned_by_pid: Option<u32>,
}

impl LaunchPreflight<'_> {
    pub fn resolve(self) -> Result<PreflightOutcome, RunError> {
        let state = if crate::claude::claude_agent_view_disabled() {
            None
        } else {
            inspect_running_daemon(self.config_dir)
        };
        let proceed = proceed_preflight(self.config_dir, state.as_ref());
        let Some(state) = state else {
            return Ok(PreflightOutcome::Proceed(proceed));
        };

        if classify_launch_safety(&state, self.request, self.home) == Safety::Safe {
            return Ok(PreflightOutcome::Proceed(proceed));
        }

        let owner = if state.env_unreadable {
            DaemonOwner::Unverified
        } else {
            classify_daemon_owner(&state)
        };
        let message =
            format_daemon_conflict_error(self.request, &state, &owner, self.allow_isolated);
        match resolve_daemon_conflict(
            state.pid,
            &message,
            self.claude_bin,
            self.config_dir,
            self.allow_isolated,
        )? {
            ConflictOutcome::Continue => Ok(PreflightOutcome::Proceed(proceed)),
            ConflictOutcome::SwitchToIsolated => Ok(PreflightOutcome::SwitchToIsolated),
        }
    }
}

fn proceed_preflight(config_dir: &Path, state: Option<&DaemonState>) -> ProceedPreflight {
    let preserve_spawned_by_pid = state
        .and_then(|state| state.spawned_by_pid)
        .or_else(|| read_daemon_lock(config_dir).and_then(|(_, spawned_by_pid, _)| spawned_by_pid));
    ProceedPreflight {
        preserve_spawned_by_pid,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DaemonState {
    pid: u32,
    env_vars: BTreeMap<String, String>,
    env_unreadable: bool,
    spawned_by_pid: Option<u32>,
    started_at_ms: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DaemonOwner {
    Rayline { env_name: String, mode: RoutingMode },
    NonRayline,
    Unverified,
}

fn read_daemon_lock(config_dir: &Path) -> Option<(u32, Option<u32>, Option<i64>)> {
    let raw = fs::read_to_string(config_dir.join("daemon.lock")).ok()?;
    let lock = serde_json::from_str::<Value>(&raw).ok()?;
    let pid = lock.get("pid")?.as_u64()?;
    let pid = u32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    if !pid_is_alive(pid) {
        return None;
    }
    let spawned_by_pid = lock
        .get("spawnedBy")
        .and_then(Value::as_object)
        .and_then(|spawned_by| spawned_by.get("pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    let started_at_ms = lock.get("startedAt").and_then(Value::as_i64);
    Some((pid, spawned_by_pid, started_at_ms))
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) performs an existence/permission probe only; it
        // does not deliver a signal or mutate process state.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return true;
        }
        io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn inspect_running_daemon(config_dir: &Path) -> Option<DaemonState> {
    let (pid, spawned_by_pid, started_at_ms) = read_daemon_lock(config_dir)?;
    match read_process_env(pid) {
        Some(env_vars) => Some(DaemonState {
            pid,
            env_vars,
            env_unreadable: false,
            spawned_by_pid,
            started_at_ms,
        }),
        None => Some(DaemonState {
            pid,
            env_vars: BTreeMap::new(),
            env_unreadable: true,
            spawned_by_pid,
            started_at_ms,
        }),
    }
}

fn read_process_env(pid: u32) -> Option<BTreeMap<String, String>> {
    #[cfg(target_os = "linux")]
    {
        return read_proc_environ(pid);
    }
    #[cfg(target_os = "macos")]
    {
        return read_ps_environ(pid);
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        None
    }
}

#[cfg(target_os = "linux")]
fn read_proc_environ(pid: u32) -> Option<BTreeMap<String, String>> {
    let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
    let mut values = BTreeMap::new();
    for entry in raw.split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let decoded = String::from_utf8_lossy(entry);
        if let Some((key, value)) = decoded.split_once('=') {
            if !key.is_empty() {
                values.insert(key.to_owned(), value.to_owned());
            }
        }
    }
    Some(values)
}

#[cfg(target_os = "macos")]
fn read_ps_environ(pid: u32) -> Option<BTreeMap<String, String>> {
    let output = Command::new("/bin/ps")
        .arg("eww")
        .arg("-p")
        .arg(pid.to_string())
        .arg("-o")
        .arg("command=")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&output.stdout);
    parse_ps_environ_line(line.trim())
}

#[cfg(target_os = "macos")]
fn parse_ps_environ_line(line: &str) -> Option<BTreeMap<String, String>> {
    if line.is_empty() {
        return None;
    }
    let mut values = BTreeMap::new();
    for token in line.split(' ') {
        let Some((key, value)) = token.split_once('=') else {
            continue;
        };
        if is_upper_env_key(key) {
            values.insert(key.to_owned(), value.to_owned());
        }
    }
    Some(values)
}

#[cfg(target_os = "macos")]
fn is_upper_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first == '_' || first.is_ascii_uppercase()) {
        return false;
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_uppercase() || ch.is_ascii_digit())
}

fn classify_daemon_owner(state: &DaemonState) -> DaemonOwner {
    let explicit_env_name = state
        .env_vars
        .get(RAYLINE_ENV_NAME_ENV)
        .filter(|env_name| crate::status::is_valid_root_env(env_name))
        .cloned();
    if let Some(proxy_mode) = state
        .env_vars
        .get("RAYLINE_CLAUDE_ROUTING_MODE")
        .and_then(|mode| proxy_routing_mode_from_env(mode))
    {
        if daemon_proxy_port(&state.env_vars).is_none() {
            return DaemonOwner::NonRayline;
        }
        if let Some(env_name) = explicit_env_name.as_ref() {
            return DaemonOwner::Rayline {
                env_name: env_name.clone(),
                mode: proxy_mode,
            };
        }
        let router_url = state
            .env_vars
            .get("RAYLINE_ROUTER_URL")
            .map(|url| url.trim_end_matches('/'))
            .unwrap_or_default();
        for (env_name, expected_url) in router_urls() {
            if router_url == expected_url.trim_end_matches('/') {
                return DaemonOwner::Rayline {
                    env_name: env_name.to_owned(),
                    mode: proxy_mode,
                };
            }
        }
        return DaemonOwner::NonRayline;
    }

    let base_url = state
        .env_vars
        .get("ANTHROPIC_BASE_URL")
        .map(|url| url.trim_end_matches('/'))
        .unwrap_or_default();
    if let Some(env_name) = explicit_env_name {
        if !base_url.is_empty() {
            return DaemonOwner::Rayline {
                env_name,
                mode: RoutingMode::Override,
            };
        }
    }
    for (env_name, expected_url) in router_urls() {
        if base_url == expected_url.trim_end_matches('/') {
            return DaemonOwner::Rayline {
                env_name: env_name.to_owned(),
                mode: RoutingMode::Override,
            };
        }
    }
    DaemonOwner::NonRayline
}

fn daemon_proxy_port(env_vars: &BTreeMap<String, String>) -> Option<u16> {
    let proxy_url = env_vars
        .get("HTTPS_PROXY")
        .or_else(|| env_vars.get("https_proxy"))?;
    let parsed = url::Url::parse(proxy_url).ok()?;
    if parsed.scheme() != "http" {
        return None;
    }
    if !matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "::1")) {
        return None;
    }
    parsed.port()
}

fn proxy_routing_mode_from_env(value: &str) -> Option<RoutingMode> {
    match value {
        ROUTING_MODE_PROXY => Some(RoutingMode::Proxy),
        ROUTING_MODE_PROXY_SUBAGENTS => Some(RoutingMode::ProxySubagents),
        ROUTING_MODE_PROXY_PASSTHROUGH => Some(RoutingMode::ProxyPassthrough),
        _ => None,
    }
}

fn daemon_owner_matches_request(state: &DaemonState, request: &RequestSpec<'_>) -> bool {
    let owner = classify_daemon_owner(state);
    let DaemonOwner::Rayline { env_name, mode } = owner else {
        return false;
    };
    if env_name != request.env_name || mode != request.routing_mode {
        return false;
    }
    if state
        .env_vars
        .get(AUTO_COMPACT_WINDOW_ENV)
        .map(String::as_str)
        != Some(request.auto_compact_window)
    {
        return false;
    }
    if !is_proxy_routing_mode(request.routing_mode) {
        return true;
    }
    daemon_proxy_port(&state.env_vars) == request.requested_proxy_port
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Safety {
    Safe,
    Conflict,
}

fn classify_launch_safety(state: &DaemonState, request: &RequestSpec<'_>, home: &Path) -> Safety {
    let env_verified_ours = !state.env_unreadable
        && request.requested_local_port.is_none()
        && daemon_owner_matches_request(state, request);
    let launch_log_verified_ours = daemon_was_launched_by_wksp(state, home, request);

    if env_verified_ours || launch_log_verified_ours {
        Safety::Safe
    } else {
        Safety::Conflict
    }
}

/// Whether a detected daemon conflict was resolved by stopping the other daemon
/// (continue normally) or by switching this run into an isolated config dir.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConflictOutcome {
    Continue,
    SwitchToIsolated,
}

/// Choice from the interactive daemon-conflict prompt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConflictChoice {
    StopAndContinue,
    RunIsolated,
    Cancel,
}

fn parse_conflict_choice(answer: &str, allow_isolated: bool) -> ConflictChoice {
    let answer = answer.trim();
    if allow_isolated
        && (answer.eq_ignore_ascii_case("i") || answer.eq_ignore_ascii_case("isolated"))
    {
        return ConflictChoice::RunIsolated;
    }
    if answer.is_empty()
        || answer.eq_ignore_ascii_case("s")
        || answer.eq_ignore_ascii_case("y")
        || answer.eq_ignore_ascii_case("yes")
    {
        ConflictChoice::StopAndContinue
    } else {
        ConflictChoice::Cancel
    }
}

/// Resolve a daemon conflict interactively. We offer to stop the conflicting
/// daemon (continue normally) and, when this run is not already isolated, to run
/// in a separate config dir that gets its own supervisor. Declining, or any
/// non-terminal session, surfaces the full manual instructions and aborts so
/// scripts and CI keep the prior hard-error behavior.
fn resolve_daemon_conflict(
    pid: u32,
    message: &str,
    claude_bin: &Path,
    config_dir: &Path,
    allow_isolated: bool,
) -> Result<ConflictOutcome, RunError> {
    // Only prompt for a genuinely interactive session. Print-mode pipelines
    // (`claude -p ... | cmd`) keep stdin/stderr on the terminal but pipe stdout;
    // there we must keep the prior hard error rather than block the pipeline or
    // let a stray Enter stop the shared daemon.
    if !(io::stdin().is_terminal() && io::stdout().is_terminal() && io::stderr().is_terminal()) {
        return Err(RunError::DaemonConflict(message.to_owned()));
    }

    let brand = crate::DISPLAY_NAME;
    if allow_isolated {
        let isolated_dir = dirs::home_dir()
            .map(|home| crate::claude::isolated_cc_dir(&home).display().to_string())
            .unwrap_or_else(|| format!("~/{}/cc", crate::DOT_CONFIG_DIR));
        eprintln!(
            "A conflicting Claude Code daemon is running (PID {pid}); it must be stopped or bypassed before this {brand} session can start.\n\n  [s] Stop it and start a {brand} daemon here  ({STOP_COMMAND})\n  [i] Run isolated  (separate daemon under {isolated_dir}; leaves the other running)\n  [c] Cancel"
        );
        eprint!("Choose [s/i/c] (s): ");
    } else {
        eprintln!(
            "A conflicting Claude Code daemon is running (PID {pid}); it must be stopped before this {brand} session can start."
        );
        eprint!("Stop it and continue? [Y/n] ");
    }
    if io::stderr().flush().is_err() {
        return Err(RunError::DaemonConflict(message.to_owned()));
    }

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return Err(RunError::DaemonConflict(message.to_owned()));
    }

    match parse_conflict_choice(&answer, allow_isolated) {
        ConflictChoice::StopAndContinue => {
            stop_conflicting_daemon(claude_bin, config_dir).map(|()| ConflictOutcome::Continue)
        }
        ConflictChoice::RunIsolated => Ok(ConflictOutcome::SwitchToIsolated),
        // Declined: print the full instructions so the user can switch by hand.
        ConflictChoice::Cancel => Err(RunError::DaemonConflict(message.to_owned())),
    }
}

/// Run `claude daemon stop --any` against `config_dir`, inheriting stdio so its
/// output is visible. The caller then proceeds with the normal launch, which
/// spawns a fresh router-configured daemon, equivalent to re-running by hand.
/// Setting `CLAUDE_CONFIG_DIR` is what makes the stop target the right daemon:
/// for an isolated conflict it must stop the daemon in `isolated_cc_dir`, not
/// the default (or inherited) config dir.
fn stop_conflicting_daemon(claude_bin: &Path, config_dir: &Path) -> Result<(), RunError> {
    eprintln!("Stopping the current daemon ({STOP_COMMAND})...");
    match Command::new(claude_bin)
        .args(["daemon", "stop", "--any"])
        .env(CLAUDE_CONFIG_DIR_ENV, config_dir)
        .status()
    {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(RunError::DaemonConflict(format!(
            "`{STOP_COMMAND}` exited with code {}. Stop the daemon manually and re-run.",
            status.code().unwrap_or(-1)
        ))),
        Err(error) => Err(RunError::DaemonConflict(format!(
            "Could not run `{STOP_COMMAND}`: {error}. Stop the daemon manually and re-run."
        ))),
    }
}

fn daemon_was_launched_by_wksp(
    state: &DaemonState,
    home: &Path,
    request: &RequestSpec<'_>,
) -> bool {
    let Some(spawned_by_pid) = state.spawned_by_pid else {
        return false;
    };
    let Some(started_at_ms) = state.started_at_ms else {
        return false;
    };
    for entry in read_rayline_claude_launches(home) {
        if entry.get("pid").and_then(Value::as_u64) != Some(u64::from(spawned_by_pid)) {
            continue;
        }
        if entry.get("env").and_then(Value::as_str) != Some(request.env_name) {
            continue;
        }
        if entry.get("routing_mode").and_then(Value::as_str)
            != Some(routing_mode_name(request.routing_mode))
        {
            continue;
        }
        if entry.get("auto_compact_window").and_then(Value::as_str)
            != Some(request.auto_compact_window)
        {
            continue;
        }
        if is_proxy_routing_mode(request.routing_mode)
            && entry.get("proxy_port").and_then(Value::as_u64)
                != request.requested_proxy_port.map(u64::from)
        {
            continue;
        }
        if entry.get("local_injector_port").and_then(Value::as_u64)
            != request.requested_local_port.map(u64::from)
        {
            continue;
        }
        let Some(ts) = entry.get("ts").and_then(Value::as_i64) else {
            continue;
        };
        let delta_ms = started_at_ms - (ts * 1000);
        if (0..=DAEMON_SPAWN_MAX_DELAY_SECONDS * 1000).contains(&delta_ms) {
            return true;
        }
    }
    false
}

fn format_daemon_conflict_error(
    request: &RequestSpec<'_>,
    state: &DaemonState,
    owner: &DaemonOwner,
    allow_isolated: bool,
) -> String {
    let env_flag = String::new();
    let arg_str = if request.args.is_empty() {
        "agents".to_owned()
    } else {
        request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    };
    // Proxy is the default routing mode, so only spell out non-default modes.
    let mode_flag = match request.routing_mode {
        RoutingMode::Proxy => String::new(),
        // No deprecated `--routing-mode` alias exists for passthrough; spell
        // the modern flag directly.
        RoutingMode::ProxyPassthrough => "--route none ".to_owned(),
        RoutingMode::Override | RoutingMode::ProxySubagents => {
            format!(
                "--routing-mode {} ",
                routing_mode_name(request.routing_mode)
            )
        }
    };
    let rerun = format!("{} {env_flag}claude {mode_flag}{arg_str}", crate::CLI_BIN)
        .trim_end()
        .to_owned();
    // When isolation is available, surface it as a third path that keeps the
    // other daemon running instead of stopping it.
    let isolated_alt = if allow_isolated {
        let rerun_isolated = format!(
            "{} {env_flag}claude --isolated {mode_flag}{arg_str}",
            crate::CLI_BIN
        )
        .trim_end()
        .to_owned();
        format!(
            "\n\nOr run alongside it in an isolated config dir (~/{}/cc):\n  {rerun_isolated}",
            crate::DOT_CONFIG_DIR
        )
    } else {
        String::new()
    };
    match owner {
        DaemonOwner::Rayline { env_name, mode } => {
            let existing_mode = routing_mode_name(*mode);
            let requested_mode = routing_mode_name(request.routing_mode);
            format!(
                "A {} {env_name} {existing_mode}-mode daemon is running (PID {}).\n\nCannot start a {} {requested_mode}-mode daemon while the {env_name} {existing_mode}-mode daemon is active.\n\nTo switch:\n  1. Stop the current daemon:   {STOP_COMMAND}\n  2. Re-run:                    {rerun}{isolated_alt}",
                crate::DISPLAY_NAME,
                state.pid,
                request.env_name,
            )
        }
        DaemonOwner::NonRayline => format!(
            "A non-{} Claude Code daemon is running (PID {}).\n\n{} claude needs to start a daemon configured for the\n{} router, but cannot do so while another daemon is active.\nThe current daemon's API calls go directly to Anthropic, not the\n{} router.\n\nTo switch:\n  1. Stop the current daemon:   {STOP_COMMAND}\n  2. Re-run:                    {rerun}{isolated_alt}",
            crate::DISPLAY_NAME,
            state.pid,
            crate::CLI_BIN,
            crate::DISPLAY_NAME,
            crate::DISPLAY_NAME
        ),
        DaemonOwner::Unverified => format!(
            "A Claude Code daemon is running (PID {}), but {} could not verify that it matches this launch.\n\n{} claude needs to start or reuse a daemon configured for the\n{} router, but cannot do so while an unverified daemon is active.\n\nTo switch:\n  1. Stop the current daemon:   {STOP_COMMAND}\n  2. Re-run:                    {rerun}{isolated_alt}",
            state.pid,
            crate::DISPLAY_NAME,
            crate::CLI_BIN,
            crate::DISPLAY_NAME,
        ),
    }
}

fn read_rayline_claude_launches(home: &Path) -> Vec<Value> {
    let raw = match fs::read_to_string(rayline_claude_launches_path(home)) {
        Ok(raw) => raw,
        Err(_) => return Vec::new(),
    };
    serde_json::from_str::<Value>(&raw)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter(Value::is_object)
        .collect()
}

pub fn record_rayline_claude_launch(record: LaunchRecord<'_>) {
    let now = unix_now_secs();
    let mut pruned = Vec::new();
    for entry in read_rayline_claude_launches(record.home) {
        if record
            .preserve_spawned_by_pid
            .is_some_and(|pid| entry.get("pid").and_then(Value::as_u64) == Some(u64::from(pid)))
        {
            pruned.push(entry);
            continue;
        }
        let Some(ts) = entry.get("ts").and_then(Value::as_i64) else {
            continue;
        };
        if now - ts < RAYLINE_CLAUDE_LAUNCH_TTL_SECONDS {
            pruned.push(entry);
        }
    }
    pruned.push(serde_json::json!({
        "pid": record.pid,
        "env": record.request.env_name,
        "routing_mode": routing_mode_name(record.request.routing_mode),
        "auto_compact_window": record.request.auto_compact_window,
        "proxy_port": record.request.requested_proxy_port,
        "local_injector_port": record.request.requested_local_port,
        "ts": now,
    }));
    let path = rayline_claude_launches_path(record.home);
    let Some(parent) = path.parent() else {
        return;
    };
    if fs::create_dir_all(parent).is_err() {
        return;
    }
    let tmp_path = path.with_extension("json.tmp");
    let Ok(contents) = serde_json::to_string(&pruned) else {
        return;
    };
    if fs::write(&tmp_path, contents).is_err() {
        return;
    }
    let _ = fs::rename(tmp_path, path);
}

fn rayline_claude_launches_path(home: &Path) -> PathBuf {
    home.join(crate::CLAUDE_LAUNCHES_SUFFIX)
}

fn router_urls() -> [(&'static str, &'static str); 1] {
    [("prod", crate::ROUTER_PROD_URL)]
}

fn unix_now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_HOME_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_home() -> PathBuf {
        let id = NEXT_HOME_ID.fetch_add(1, Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "rayline-cli-daemon-test-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).unwrap();
        home
    }

    fn proxy_request() -> RequestSpec<'static> {
        RequestSpec {
            env_name: "prod",
            routing_mode: RoutingMode::Proxy,
            auto_compact_window: "180000",
            args: &[],
            requested_local_port: None,
            requested_proxy_port: Some(20810),
        }
    }

    fn state(env_vars: BTreeMap<String, String>) -> DaemonState {
        DaemonState {
            pid: 42,
            env_vars,
            env_unreadable: false,
            spawned_by_pid: None,
            started_at_ms: None,
        }
    }

    fn unreadable_state(spawned_by_pid: Option<u32>, started_at_ms: Option<i64>) -> DaemonState {
        DaemonState {
            pid: 42,
            env_vars: BTreeMap::new(),
            env_unreadable: true,
            spawned_by_pid,
            started_at_ms,
        }
    }

    fn rayline_proxy_env(port: u16) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(RAYLINE_ENV_NAME_ENV.to_owned(), "prod".to_owned());
        env.insert(
            "RAYLINE_CLAUDE_ROUTING_MODE".to_owned(),
            routing_mode_name(RoutingMode::Proxy).to_owned(),
        );
        env.insert(AUTO_COMPACT_WINDOW_ENV.to_owned(), "180000".to_owned());
        env.insert("HTTPS_PROXY".to_owned(), format!("http://127.0.0.1:{port}"));
        env.insert(
            "RAYLINE_ROUTER_URL".to_owned(),
            crate::ROUTER_PROD_URL.to_owned(),
        );
        env
    }

    fn write_launch_log(home: &Path, request: &RequestSpec<'_>, pid: u32, ts: i64) {
        let path = rayline_claude_launches_path(home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            serde_json::json!([{
                "pid": pid,
                "env": request.env_name,
                "routing_mode": routing_mode_name(request.routing_mode),
                "auto_compact_window": request.auto_compact_window,
                "proxy_port": request.requested_proxy_port,
                "local_injector_port": request.requested_local_port,
                "ts": ts,
            }])
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn unreadable_env_without_launch_log_conflicts() {
        let home = temp_home();
        let request = proxy_request();
        let state = unreadable_state(None, None);

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Conflict
        );
    }

    #[test]
    fn non_rayline_env_conflicts() {
        let home = temp_home();
        let request = proxy_request();
        let state = state(BTreeMap::new());

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Conflict
        );
    }

    #[test]
    fn router_matching_env_is_safe() {
        let home = temp_home();
        let request = proxy_request();
        let state = state(rayline_proxy_env(20810));

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Safe
        );
    }

    #[test]
    fn rayline_env_with_mismatched_proxy_port_conflicts() {
        let home = temp_home();
        let request = proxy_request();
        let state = state(rayline_proxy_env(20999));

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Conflict
        );
    }

    #[test]
    fn launch_log_attribution_overrides_env_mismatch() {
        let home = temp_home();
        let request = proxy_request();
        let mut state = state(rayline_proxy_env(20999));
        state.spawned_by_pid = Some(9001);
        state.started_at_ms = Some(1_700_000_000_000);
        write_launch_log(&home, &request, 9001, 1_700_000_000);

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Safe
        );
    }

    #[test]
    fn local_request_with_env_only_match_conflicts() {
        let home = temp_home();
        let mut request = proxy_request();
        request.requested_local_port = Some(20809);
        let state = state(rayline_proxy_env(20810));

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Conflict
        );
    }

    #[test]
    fn local_request_with_launch_log_attribution_is_safe() {
        let home = temp_home();
        let mut request = proxy_request();
        request.requested_local_port = Some(20809);
        let mut state = state(rayline_proxy_env(20810));
        state.spawned_by_pid = Some(9002);
        state.started_at_ms = Some(1_700_000_010_000);
        write_launch_log(&home, &request, 9002, 1_700_000_010);

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Safe
        );
    }

    #[test]
    fn unreadable_env_with_launch_log_attribution_is_safe() {
        let home = temp_home();
        let request = proxy_request();
        let state = unreadable_state(Some(9003), Some(1_700_000_020_000));
        write_launch_log(&home, &request, 9003, 1_700_000_020);

        assert_eq!(
            classify_launch_safety(&state, &request, &home),
            Safety::Safe
        );
    }
}
