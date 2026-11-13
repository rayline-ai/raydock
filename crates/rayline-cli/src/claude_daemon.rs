use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::claude::{
    AUTO_COMPACT_WINDOW_ENV, CLAUDE_CONFIG_DIR_ENV, RAYLINE_ENV_NAME_ENV, RunError,
};

const STOP_COMMAND: &str = "claude daemon stop --any";
const RAYLINE_CLAUDE_LAUNCH_TTL_SECONDS: i64 = 24 * 60 * 60;
const DAEMON_SPAWN_MAX_DELAY_SECONDS: i64 = 60;

pub struct RequestSpec<'a> {
    pub env_name: &'a str,
    /// The `--env` name the user passed, `None` for the built-in prod. The
    /// conflict message's rerun hint repeats it.
    pub env_flag: Option<&'a str>,
    /// The hosted router the launch talks to (`ANTHROPIC_BASE_URL`). Two
    /// environments can share a name (the built-in prod and an `environments`
    /// entry named `prod`), so a daemon is this launch's only on the same router.
    pub router_url: &'a str,
    pub auto_compact_window: Option<&'a str>,
    /// The effective refusal-recovery switches (`refusal_recovery_values`). A
    /// reused daemon's background workers inherit its env, so a daemon that
    /// holds other values is not this launch's.
    pub refusal_recovery: &'a [(&'static str, Option<String>)],
    pub args: &'a [OsString],
}

pub struct LaunchPreflight<'a> {
    pub home: &'a Path,
    pub config_dir: &'a Path,
    pub request: &'a RequestSpec<'a>,
    pub claude_bin: &'a Path,
    /// The launch runs in the isolated config dir: `config_dir` is it, and
    /// the conflict prompt offers no further isolated escape.
    pub isolated: bool,
}

pub struct ProceedPreflight {
    pub preserve_spawned_by_pid: Option<u32>,
}

pub enum PreflightOutcome {
    Proceed(ProceedPreflight),
    /// The user chose isolated. Carries this check's spawner pid: the daemon
    /// it found keeps running, so its starter's record stays kept.
    SwitchToIsolated(ProceedPreflight),
}

pub struct LaunchRecord<'a> {
    pub home: &'a Path,
    pub pid: u32,
    pub request: &'a RequestSpec<'a>,
    /// Spawner pids whose records are kept whatever their age.
    pub preserve_spawned_by_pids: &'a [u32],
}

impl LaunchPreflight<'_> {
    pub fn resolve(self) -> Result<PreflightOutcome, RunError> {
        let state = if crate::claude::claude_agent_view_disabled() {
            None
        } else {
            inspect_running_daemon(self.config_dir, pid_is_alive).map_err(RunError::DaemonProbe)?
        };
        let proceed = proceed_preflight(self.config_dir, state.as_ref(), pid_is_alive)
            .map_err(RunError::DaemonProbe)?;
        let Some(state) = state else {
            return Ok(PreflightOutcome::Proceed(proceed));
        };

        if classify_launch_safety(&state, self.request, self.home)
            .map_err(RunError::LaunchRecord)?
            == Safety::Safe
        {
            return Ok(PreflightOutcome::Proceed(proceed));
        }

        let owner = if state.env_unreadable {
            DaemonOwner::Unverified
        } else {
            classify_daemon_owner(&state)
        };
        let message = format_daemon_conflict_error(
            self.request,
            &state,
            &owner,
            self.isolated.then_some(self.config_dir),
        );
        match resolve_daemon_conflict(
            state.pid,
            &message,
            self.home,
            self.claude_bin,
            self.config_dir,
            !self.isolated,
        )? {
            ConflictOutcome::Continue => Ok(PreflightOutcome::Proceed(proceed)),
            ConflictOutcome::SwitchToIsolated => Ok(PreflightOutcome::SwitchToIsolated(proceed)),
        }
    }
}

fn proceed_preflight(
    config_dir: &Path,
    state: Option<&DaemonState>,
    is_alive: impl Fn(u32) -> io::Result<bool>,
) -> io::Result<ProceedPreflight> {
    let preserve_spawned_by_pid = match state {
        Some(state) => state.spawned_by_pid,
        None => read_daemon_lock(config_dir, is_alive)?.and_then(|lock| lock.spawned_by_pid),
    };
    Ok(ProceedPreflight {
        preserve_spawned_by_pid,
    })
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
    Rayline { env_name: String },
    NonRayline,
    Unverified,
}

struct DaemonLock {
    pid: u32,
    spawned_by_pid: Option<u32>,
    started_at_ms: Option<i64>,
}

/// The daemon `daemon.lock` names, when its process is running. A liveness
/// check that fails is an error.
fn read_daemon_lock(
    config_dir: &Path,
    is_alive: impl Fn(u32) -> io::Result<bool>,
) -> io::Result<Option<DaemonLock>> {
    let Some(lock) = parse_daemon_lock(config_dir) else {
        return Ok(None);
    };
    Ok(is_alive(lock.pid)?.then_some(lock))
}

fn parse_daemon_lock(config_dir: &Path) -> Option<DaemonLock> {
    let raw = fs::read_to_string(config_dir.join("daemon.lock")).ok()?;
    let lock = serde_json::from_str::<Value>(&raw).ok()?;
    let pid = lock.get("pid")?.as_u64()?;
    let pid = u32::try_from(pid).ok().filter(|pid| *pid > 0)?;
    let spawned_by_pid = lock
        .get("spawnedBy")
        .and_then(Value::as_object)
        .and_then(|spawned_by| spawned_by.get("pid"))
        .and_then(Value::as_u64)
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid > 0);
    let started_at_ms = lock.get("startedAt").and_then(Value::as_i64);
    Some(DaemonLock {
        pid,
        spawned_by_pid,
        started_at_ms,
    })
}

fn pid_is_alive(pid: u32) -> io::Result<bool> {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) performs an existence/permission probe only; it
        // does not deliver a signal or mutate process state.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if rc == 0 {
            return Ok(true);
        }
        Ok(io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH))
    }
    #[cfg(windows)]
    {
        let pid = i32::try_from(pid).map_err(|_| {
            io::Error::other(format!(
                "could not check whether process {pid} is running: not a Windows process id"
            ))
        })?;
        crate::hybrid::process_exists(pid)
    }
}

/// The running daemon and its env. A daemon whose env cannot be read (every
/// daemon on Windows) is `env_unreadable`, so its owner is unverified.
fn inspect_running_daemon(
    config_dir: &Path,
    is_alive: impl Fn(u32) -> io::Result<bool>,
) -> io::Result<Option<DaemonState>> {
    let Some(DaemonLock {
        pid,
        spawned_by_pid,
        started_at_ms,
    }) = read_daemon_lock(config_dir, is_alive)?
    else {
        return Ok(None);
    };
    Ok(Some(match read_process_env(pid) {
        Some(env_vars) => DaemonState {
            pid,
            env_vars,
            env_unreadable: false,
            spawned_by_pid,
            started_at_ms,
        },
        None => DaemonState {
            pid,
            env_vars: BTreeMap::new(),
            env_unreadable: true,
            spawned_by_pid,
            started_at_ms,
        },
    }))
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
    let output = std::process::Command::new("/bin/ps")
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

/// A daemon is Rayline's only when it carries a valid `RAYLINE_ENV_NAME` and
/// a router URL; anything else is not ours.
fn classify_daemon_owner(state: &DaemonState) -> DaemonOwner {
    let has_base_url = state
        .env_vars
        .get("ANTHROPIC_BASE_URL")
        .is_some_and(|url| !url.trim_end_matches('/').is_empty());
    match state.env_vars.get(RAYLINE_ENV_NAME_ENV) {
        Some(env_name) if has_base_url && crate::status::is_valid_root_env(env_name) => {
            DaemonOwner::Rayline {
                env_name: env_name.clone(),
            }
        }
        _ => DaemonOwner::NonRayline,
    }
}

fn daemon_owner_matches_request(state: &DaemonState, request: &RequestSpec<'_>) -> bool {
    let owner = classify_daemon_owner(state);
    let DaemonOwner::Rayline { env_name } = owner else {
        return false;
    };
    if env_name != request.env_name {
        return false;
    }
    if state
        .env_vars
        .get("ANTHROPIC_BASE_URL")
        .map(|url| url.trim_end_matches('/'))
        != Some(request.router_url.trim_end_matches('/'))
    {
        return false;
    }
    if state
        .env_vars
        .get(AUTO_COMPACT_WINDOW_ENV)
        .map(String::as_str)
        != request.auto_compact_window
    {
        return false;
    }
    request.refusal_recovery.iter().all(|(key, value)| {
        let held = state.env_vars.get(*key).filter(|held| !held.is_empty());
        held.map(String::as_str) == value.as_deref()
    })
}

/// The refusal-recovery switches a launch record names, as `{key: value|null}`.
/// A record must name every switch of this launch with the same value; a
/// record without them, or without one of them, matches no launch.
fn launch_refusal_recovery_matches(entry: &Value, request: &RequestSpec<'_>) -> bool {
    let Some(recorded) = entry.get("refusal_recovery").and_then(Value::as_object) else {
        return false;
    };
    request
        .refusal_recovery
        .iter()
        .all(|(key, value)| match recorded.get(*key) {
            Some(Value::Null) => value.is_none(),
            Some(Value::String(held)) => value.as_deref() == Some(held.as_str()),
            _ => false,
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Safety {
    Safe,
    Conflict,
}

fn classify_launch_safety(
    state: &DaemonState,
    request: &RequestSpec<'_>,
    home: &Path,
) -> io::Result<Safety> {
    let env_verified_ours = !state.env_unreadable && daemon_owner_matches_request(state, request);
    let launch_log_verified_ours = daemon_was_launched_by_wksp(state, home, request)?;

    Ok(if env_verified_ours || launch_log_verified_ours {
        Safety::Safe
    } else {
        Safety::Conflict
    })
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
    home: &Path,
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

    eprint!("{}", conflict_prompt(pid, home, allow_isolated));
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

/// The daemon-conflict prompt. The isolated choice names the isolated config
/// dir under `home`.
fn conflict_prompt(pid: u32, home: &Path, allow_isolated: bool) -> String {
    let brand = crate::DISPLAY_NAME;
    if allow_isolated {
        let isolated_dir = crate::claude::isolated_cc_dir(home);
        format!(
            "A conflicting Claude Code daemon is running (PID {pid}); it must be stopped or bypassed before this {brand} session can start.\n\n  [s] Stop it and start a {brand} daemon here  ({STOP_COMMAND})\n  [i] Run isolated  (separate daemon under {}; leaves the other running)\n  [c] Cancel\nChoose [s/i/c] (s): ",
            isolated_dir.display()
        )
    } else {
        format!(
            "A conflicting Claude Code daemon is running (PID {pid}); it must be stopped before this {brand} session can start.\nStop it and continue? [Y/n] "
        )
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
    match crate::claude::claude_command(claude_bin)
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
) -> io::Result<bool> {
    let Some(spawned_by_pid) = state.spawned_by_pid else {
        return Ok(false);
    };
    let Some(started_at_ms) = state.started_at_ms else {
        return Ok(false);
    };
    for entry in read_rayline_claude_launches(home)? {
        if entry.get("pid").and_then(Value::as_u64) != Some(u64::from(spawned_by_pid)) {
            continue;
        }
        if entry.get("env").and_then(Value::as_str) != Some(request.env_name) {
            continue;
        }
        if entry.get("router_url").and_then(Value::as_str) != Some(request.router_url) {
            continue;
        }
        if entry.get("auto_compact_window").and_then(Value::as_str) != request.auto_compact_window {
            continue;
        }
        if !launch_refusal_recovery_matches(&entry, request) {
            continue;
        }
        let Some(ts) = entry.get("ts").and_then(Value::as_i64) else {
            continue;
        };
        let delta_ms = started_at_ms - (ts * 1000);
        if (0..=DAEMON_SPAWN_MAX_DELAY_SECONDS * 1000).contains(&delta_ms) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn format_daemon_conflict_error(
    request: &RequestSpec<'_>,
    state: &DaemonState,
    owner: &DaemonOwner,
    isolated_dir: Option<&Path>,
) -> String {
    let env_flag = request
        .env_flag
        .map(|name| format!("--env {name} "))
        .unwrap_or_default();
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
    // An isolated launch's daemon lives in the isolated config dir: the stop
    // must target it, and the rerun stays isolated.
    let (stop, isolated_flag) = match isolated_dir {
        Some(dir) => (
            format!("{STOP_COMMAND} (with CLAUDE_CONFIG_DIR={})", dir.display()),
            "--isolated ",
        ),
        None => (STOP_COMMAND.to_owned(), ""),
    };
    let rerun = format!(
        "{} {env_flag}run claude {isolated_flag}{arg_str}",
        crate::CLI_BIN
    )
    .trim_end()
    .to_owned();
    // When isolation is available, surface it as a third path that keeps the
    // other daemon running instead of stopping it.
    let isolated_alt = if isolated_dir.is_none() {
        let rerun_isolated = format!(
            "{} {env_flag}run claude --isolated {arg_str}",
            crate::CLI_BIN
        )
        .trim_end()
        .to_owned();
        format!(
            "\n\nOr run alongside it in an isolated config dir (~/{}/harness/claude-code):\n  {rerun_isolated}",
            crate::DOT_CONFIG_DIR
        )
    } else {
        String::new()
    };
    match owner {
        DaemonOwner::Rayline { env_name } => {
            format!(
                "A {} {env_name} daemon is running (PID {}).\n\nCannot start a {} daemon while the {env_name} daemon is active.\n\nTo switch:\n  1. Stop the current daemon:   {stop}\n  2. Re-run:                    {rerun}{isolated_alt}",
                crate::DISPLAY_NAME,
                state.pid,
                request.env_name,
            )
        }
        DaemonOwner::NonRayline => format!(
            "A non-{} Claude Code daemon is running (PID {}).\n\n`{} run claude` needs to start a daemon configured for the\n{} router, but cannot do so while another daemon is active.\nThe current daemon's API calls go directly to Anthropic, not the\n{} router.\n\nTo switch:\n  1. Stop the current daemon:   {stop}\n  2. Re-run:                    {rerun}{isolated_alt}",
            crate::DISPLAY_NAME,
            state.pid,
            crate::CLI_BIN,
            crate::DISPLAY_NAME,
            crate::DISPLAY_NAME
        ),
        DaemonOwner::Unverified => format!(
            "A Claude Code daemon is running (PID {}), but {} could not verify that it matches this launch.\n\n`{} run claude` needs to start or reuse a daemon configured for the\n{} router, but cannot do so while an unverified daemon is active.\n\nTo switch:\n  1. Stop the current daemon:   {stop}\n  2. Re-run:                    {rerun}{isolated_alt}",
            state.pid,
            crate::DISPLAY_NAME,
            crate::CLI_BIN,
            crate::DISPLAY_NAME,
        ),
    }
}

/// The launch records Rayline wrote. A missing file holds none; a file that
/// cannot be read, or is not a JSON array of objects, is an error with the
/// path.
fn read_rayline_claude_launches(home: &Path) -> io::Result<Vec<Value>> {
    let path = rayline_claude_launches_path(home);
    let raw = match fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(io::Error::new(
                error.kind(),
                format!("failed to read {}: {error}", path.display()),
            ));
        }
    };
    let invalid = |detail: &str| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {detail}", path.display()),
        )
    };
    let value = serde_json::from_str::<Value>(&raw)
        .map_err(|error| invalid(&format!("invalid JSON: {error}")))?;
    let Value::Array(entries) = value else {
        return Err(invalid("not a JSON array"));
    };
    if !entries.iter().all(Value::is_object) {
        return Err(invalid("an entry is not a JSON object"));
    }
    Ok(entries)
}

/// One launch record: what `daemon_was_launched_by_wksp` matches a daemon on.
fn launch_entry(request: &RequestSpec<'_>, pid: u32, ts: i64) -> Value {
    serde_json::json!({
        "pid": pid,
        "env": request.env_name,
        "router_url": request.router_url,
        "auto_compact_window": request.auto_compact_window,
        "refusal_recovery": request
            .refusal_recovery
            .iter()
            .map(|(key, value)| ((*key).to_owned(), serde_json::json!(value)))
            .collect::<serde_json::Map<_, _>>(),
        "ts": ts,
    })
}

/// Adds this launch to the launch records, dropping expired ones. Any failure
/// to read or write the file is an error with the path.
pub fn record_rayline_claude_launch(record: LaunchRecord<'_>) -> io::Result<()> {
    let now = unix_now_secs();
    let mut pruned = Vec::new();
    for entry in read_rayline_claude_launches(record.home)? {
        let entry_pid = entry.get("pid").and_then(Value::as_u64);
        if record
            .preserve_spawned_by_pids
            .iter()
            .any(|&pid| entry_pid == Some(u64::from(pid)))
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
    pruned.push(launch_entry(record.request, record.pid, now));
    let path = rayline_claude_launches_path(record.home);
    let failed = |action: String, error: io::Error| {
        io::Error::new(error.kind(), format!("failed to {action}: {error}"))
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| failed(format!("create {}", parent.display()), error))?;
    }
    // Per process, so an overlapping launch cannot rename this one's file away.
    let tmp_path = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let contents = serde_json::to_string(&pruned)
        .map_err(|error| failed(format!("serialise {}", path.display()), error.into()))?;
    fs::write(&tmp_path, contents)
        .map_err(|error| failed(format!("write {}", tmp_path.display()), error))?;
    fs::rename(&tmp_path, &path).map_err(|error| {
        failed(
            format!("rename {} to {}", tmp_path.display(), path.display()),
            error,
        )
    })
}

fn rayline_claude_launches_path(home: &Path) -> PathBuf {
    home.join(crate::CLAUDE_LAUNCHES_SUFFIX)
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
    const FALLBACK: &str = "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK";
    const RETRY: &str = "CLAUDE_CODE_DISABLE_REFUSAL_RETRY";

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

    fn launch_request() -> RequestSpec<'static> {
        RequestSpec {
            env_name: "prod",
            env_flag: None,
            router_url: crate::ROUTER_PROD_URL,
            auto_compact_window: Some("180000"),
            refusal_recovery: Box::leak(
                crate::claude::refusal_recovery_values().into_boxed_slice(),
            ),
            args: &[],
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

    fn rayline_env() -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(RAYLINE_ENV_NAME_ENV.to_owned(), "prod".to_owned());
        env.insert(
            "ANTHROPIC_BASE_URL".to_owned(),
            crate::ROUTER_PROD_URL.to_owned(),
        );
        env.insert(AUTO_COMPACT_WINDOW_ENV.to_owned(), "180000".to_owned());
        env.insert(FALLBACK.to_owned(), "1".to_owned());
        env.insert(RETRY.to_owned(), "1".to_owned());
        env
    }

    fn write_launch_log(home: &Path, request: &RequestSpec<'_>, pid: u32, ts: i64) {
        let path = rayline_claude_launches_path(home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            path,
            Value::Array(vec![launch_entry(request, pid, ts)]).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn rerun_hint_repeats_the_env_flag() {
        let mut request = launch_request();
        let state = state(rayline_env());
        let owner = DaemonOwner::NonRayline;
        let prod = format_daemon_conflict_error(&request, &state, &owner, None);
        assert!(
            prod.contains(&format!("{} run claude agents", crate::CLI_BIN)),
            "{prod}"
        );
        assert!(
            prod.contains("1. Stop the current daemon:   claude daemon stop --any\n"),
            "{prod}"
        );

        request.env_name = "dev";
        request.env_flag = Some("dev");
        let dev = format_daemon_conflict_error(&request, &state, &owner, None);
        assert!(
            dev.contains(&format!("{} --env dev run claude agents", crate::CLI_BIN)),
            "{dev}"
        );
        assert!(
            dev.contains(&format!(
                "{} --env dev run claude --isolated agents",
                crate::CLI_BIN
            )),
            "{dev}"
        );
    }

    #[test]
    fn an_isolated_conflict_stops_the_isolated_daemon_and_reruns_isolated() {
        let mut request = launch_request();
        request.env_name = "dev";
        request.env_flag = Some("dev");
        let state = state(rayline_env());
        let isolated_dir = crate::claude::isolated_cc_dir(&temp_home());
        for owner in [
            DaemonOwner::Rayline {
                env_name: "prod".to_owned(),
            },
            DaemonOwner::NonRayline,
            DaemonOwner::Unverified,
        ] {
            let message =
                format_daemon_conflict_error(&request, &state, &owner, Some(&isolated_dir));
            let expected = format!(
                "To switch:\n  1. Stop the current daemon:   claude daemon stop --any (with CLAUDE_CONFIG_DIR={})\n  2. Re-run:                    {} --env dev run claude --isolated agents",
                isolated_dir.display(),
                crate::CLI_BIN
            );
            assert!(message.ends_with(&expected), "{message}");
        }
    }

    #[test]
    fn the_isolated_choice_names_the_isolated_dir_under_home() {
        let home = temp_home();
        let prompt = conflict_prompt(42, &home, true);
        let isolated_dir = crate::claude::isolated_cc_dir(&home);
        assert!(
            prompt.contains(&format!(
                "(separate daemon under {}; leaves the other running)",
                isolated_dir.display()
            )),
            "{prompt}"
        );
        assert!(!conflict_prompt(42, &home, false).contains("[i] Run isolated"));
    }

    #[test]
    fn unreadable_env_without_launch_log_conflicts() {
        let home = temp_home();
        let request = launch_request();
        let state = unreadable_state(None, None);

        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn non_rayline_env_conflicts() {
        let home = temp_home();
        let request = launch_request();
        let state = state(BTreeMap::new());

        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn prod_router_url_without_env_name_is_not_rayline() {
        let home = temp_home();
        let request = launch_request();
        let mut unnamed = rayline_env();
        unnamed.remove(RAYLINE_ENV_NAME_ENV);
        let state = state(unnamed);

        assert!(matches!(
            classify_daemon_owner(&state),
            DaemonOwner::NonRayline
        ));
        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn router_matching_env_is_safe() {
        let home = temp_home();
        let request = launch_request();
        let state = state(rayline_env());

        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Safe
        );
    }

    // The built-in prod and an `environments` entry named `prod` share the
    // name, so a daemon on the other router is not this launch's.
    #[test]
    fn daemon_on_another_router_with_the_same_name_conflicts() {
        let home = temp_home();
        let request = launch_request();
        let mut other = rayline_env();
        other.insert(
            "ANTHROPIC_BASE_URL".to_owned(),
            "https://router.example.test".to_owned(),
        );

        assert_eq!(
            classify_launch_safety(&state(other), &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn launch_record_for_another_router_does_not_attribute() {
        let home = temp_home();
        let mut recorded = launch_request();
        recorded.router_url = "https://router.example.test";
        let state = unreadable_state(Some(9005), Some(1_700_000_040_000));
        write_launch_log(&home, &recorded, 9005, 1_700_000_040);

        assert_eq!(
            classify_launch_safety(&state, &launch_request(), &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn unpinned_compact_window_matches_only_an_unpinned_daemon() {
        let home = temp_home();
        let mut request = launch_request();
        request.auto_compact_window = None;
        let mut unpinned = rayline_env();
        unpinned.remove(AUTO_COMPACT_WINDOW_ENV);

        assert_eq!(
            classify_launch_safety(&state(unpinned), &request, &home).unwrap(),
            Safety::Safe
        );
        assert_eq!(
            classify_launch_safety(&state(rayline_env()), &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn unpinned_compact_window_launch_log_attribution_is_safe() {
        let home = temp_home();
        let mut request = launch_request();
        request.auto_compact_window = None;
        let state = unreadable_state(Some(9004), Some(1_700_000_030_000));
        write_launch_log(&home, &request, 9004, 1_700_000_030);

        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Safe
        );
    }

    #[test]
    fn launch_log_attribution_overrides_env_mismatch() {
        let home = temp_home();
        let request = launch_request();
        let mut state = state(BTreeMap::new());
        state.spawned_by_pid = Some(9001);
        state.started_at_ms = Some(1_700_000_000_000);
        write_launch_log(&home, &request, 9001, 1_700_000_000);

        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Safe
        );
    }

    #[test]
    fn unreadable_env_with_launch_log_attribution_is_safe() {
        let home = temp_home();
        let request = launch_request();
        let state = unreadable_state(Some(9003), Some(1_700_000_020_000));
        write_launch_log(&home, &request, 9003, 1_700_000_020);

        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Safe
        );
    }

    // A reused daemon's background workers inherit its env, so its refusal
    // switches must be this launch's: a daemon from before the switches, or
    // one holding another value, is not reused.
    #[test]
    fn daemon_without_the_refusal_switches_conflicts() {
        let home = temp_home();
        let mut env = rayline_env();
        env.remove(RETRY);
        assert_eq!(
            classify_launch_safety(&state(env), &launch_request(), &home).unwrap(),
            Safety::Conflict
        );
    }

    // Raydock sets message threads to `0`, so a daemon that holds another
    // value, or none, is not this launch's.
    #[test]
    fn daemon_holding_a_message_threads_value_conflicts() {
        let home = temp_home();
        let mut request = launch_request();
        let wanted = crate::claude::launch_env_switches();
        request.refusal_recovery = &wanted;
        let holding = |value: Option<&str>| {
            let mut env = rayline_env();
            if let Some(value) = value {
                env.insert("CLAUDE_CODE_TETHER_LIVE".to_owned(), value.to_owned());
            }
            classify_launch_safety(&state(env), &request, &home).unwrap()
        };
        assert_eq!(holding(Some("1")), Safety::Conflict);
        assert_eq!(holding(None), Safety::Conflict);
        // Positive control: a daemon holding `0` is reused.
        assert_eq!(holding(Some("0")), Safety::Safe);
    }

    #[test]
    fn daemon_holding_another_refusal_value_conflicts() {
        let home = temp_home();
        let mut env = rayline_env();
        env.insert(FALLBACK.to_owned(), "0".to_owned());
        assert_eq!(
            classify_launch_safety(&state(env), &launch_request(), &home).unwrap(),
            Safety::Conflict
        );
        // Positive control: a daemon holding Raydock's values is reused.
        assert_eq!(
            classify_launch_safety(&state(rayline_env()), &launch_request(), &home).unwrap(),
            Safety::Safe
        );
    }

    #[test]
    fn launch_record_without_the_refusal_switches_does_not_attribute() {
        let home = temp_home();
        let request = launch_request();
        let state = unreadable_state(Some(9004), Some(1_700_000_030_000));
        let mut entry = launch_entry(&request, 9004, 1_700_000_030);
        entry.as_object_mut().unwrap().remove("refusal_recovery");
        let path = rayline_claude_launches_path(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, Value::Array(vec![entry]).to_string()).unwrap();
        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    // A record that lacks one of the launch's switches attributes the daemon
    // to no launch, whether the launch sets that switch or leaves it unset.
    #[test]
    fn launch_record_missing_a_switch_does_not_attribute() {
        let home = temp_home();
        let recorded = launch_request();
        let state = unreadable_state(Some(9006), Some(1_700_000_050_000));
        write_launch_log(&home, &recorded, 9006, 1_700_000_050);
        // Positive control: the record names every switch of its own launch.
        assert_eq!(
            classify_launch_safety(&state, &recorded, &home).unwrap(),
            Safety::Safe
        );
        let mut request = launch_request();
        let mut unset: Vec<_> = recorded.refusal_recovery.to_vec();
        unset.push(("CLAUDE_CODE_SUBAGENT_MODEL", None));
        request.refusal_recovery = &unset;
        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Conflict
        );
        let mut set: Vec<_> = recorded.refusal_recovery.to_vec();
        set.push((
            "CLAUDE_CODE_SUBAGENT_MODEL",
            Some("arc-swift-1.0".to_owned()),
        ));
        request.refusal_recovery = &set;
        assert_eq!(
            classify_launch_safety(&state, &request, &home).unwrap(),
            Safety::Conflict
        );
    }

    #[test]
    fn launch_record_with_other_refusal_switches_does_not_attribute() {
        let home = temp_home();
        let mut recorded = launch_request();
        let other = [
            (FALLBACK, Some("1".to_owned())),
            (RETRY, Some("0".to_owned())),
        ];
        recorded.refusal_recovery = &other;
        let state = unreadable_state(Some(9005), Some(1_700_000_040_000));
        write_launch_log(&home, &recorded, 9005, 1_700_000_040);
        assert_eq!(
            classify_launch_safety(&state, &launch_request(), &home).unwrap(),
            Safety::Conflict
        );
    }

    fn record(home: &Path, request: &RequestSpec<'_>) -> io::Result<()> {
        record_rayline_claude_launch(LaunchRecord {
            home,
            pid: 9006,
            request,
            preserve_spawned_by_pids: &[],
        })
    }

    /// A launch that switched to isolated keeps the records of both checks'
    /// spawners, however old; other expired records are dropped.
    #[test]
    fn a_launch_keeps_every_preserved_spawner_record() {
        let home = temp_home();
        let request = launch_request();
        let path = rayline_claude_launches_path(&home);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let expired = [9101, 9102, 9103].map(|pid| launch_entry(&request, pid, 1));
        fs::write(path, Value::Array(expired.to_vec()).to_string()).unwrap();
        record_rayline_claude_launch(LaunchRecord {
            home: &home,
            pid: 9006,
            request: &request,
            preserve_spawned_by_pids: &[9101, 9103],
        })
        .unwrap();
        let pids: Vec<u64> = read_rayline_claude_launches(&home)
            .unwrap()
            .iter()
            .map(|entry| entry["pid"].as_u64().unwrap())
            .collect();
        assert_eq!(pids, [9101, 9103, 9006]);
    }

    /// No launch record file holds no launches; recording one creates it.
    #[test]
    fn a_missing_launch_record_holds_none() {
        let home = temp_home();
        assert!(read_rayline_claude_launches(&home).unwrap().is_empty());
        record(&home, &launch_request()).unwrap();
        assert_eq!(read_rayline_claude_launches(&home).unwrap().len(), 1);
    }

    /// A launch record that cannot be read, or is not a JSON array of
    /// objects, is an error with the path, both when a daemon is checked
    /// against it and when a launch is added to it.
    #[test]
    fn a_bad_launch_record_is_an_error_naming_it() {
        let request = launch_request();
        for bad in [None, Some("{not json"), Some("{}"), Some("[1]")] {
            let home = temp_home();
            let path = rayline_claude_launches_path(&home);
            match bad {
                // A directory where the file belongs cannot be read as one.
                None => fs::create_dir_all(&path).unwrap(),
                Some(text) => {
                    fs::create_dir_all(path.parent().unwrap()).unwrap();
                    fs::write(&path, text).unwrap();
                }
            }
            let state = unreadable_state(Some(9005), Some(1_700_000_040_000));
            let errors = [
                classify_launch_safety(&state, &request, &home).unwrap_err(),
                record(&home, &request).unwrap_err(),
            ];
            for error in errors {
                assert!(
                    error.to_string().contains(&path.display().to_string()),
                    "{bad:?}: {error}"
                );
            }
        }
    }

    /// A launch record that cannot be written is an error with the path.
    #[test]
    fn a_launch_record_that_cannot_be_written_is_an_error() {
        let request = launch_request();
        // The record's dir cannot be created under a file.
        let home = temp_home();
        let dot_dir = home.join(crate::DOT_CONFIG_DIR);
        fs::write(&dot_dir, "").unwrap();
        let error = record(&home, &request).unwrap_err();
        assert!(
            error.to_string().contains(&dot_dir.display().to_string()),
            "{error}"
        );
        // The temporary file cannot be written over a directory.
        let home = temp_home();
        let tmp_path = rayline_claude_launches_path(&home)
            .with_extension(format!("json.{}.tmp", std::process::id()));
        fs::create_dir_all(&tmp_path).unwrap();
        let error = record(&home, &request).unwrap_err();
        assert!(
            error.to_string().contains(&tmp_path.display().to_string()),
            "{error}"
        );
    }

    fn write_daemon_lock(config_dir: &Path, pid: u32) {
        fs::write(
            config_dir.join("daemon.lock"),
            serde_json::json!({ "pid": pid, "spawnedBy": { "pid": 7 } }).to_string(),
        )
        .unwrap();
    }

    /// The pid probe says a running process is alive and a finished one is
    /// not. On Windows this is the same probe the local process check uses.
    #[test]
    fn the_pid_probe_tells_a_live_process_from_a_finished_one() {
        assert!(pid_is_alive(std::process::id()).unwrap());
        let mut finished = if cfg!(windows) {
            let mut command = std::process::Command::new("cmd.exe");
            command.args(["/D", "/C", "exit 0"]);
            command
        } else {
            std::process::Command::new("true")
        }
        .spawn()
        .unwrap();
        let pid = finished.id();
        finished.wait().unwrap();
        assert!(!pid_is_alive(pid).unwrap());
    }

    /// A liveness check that fails is an error, never a daemon taken to be
    /// stopped.
    #[test]
    fn a_failed_pid_probe_is_an_error_not_a_stopped_daemon() {
        let home = temp_home();
        write_daemon_lock(&home, 4242);
        let failed = |pid: u32| -> io::Result<bool> {
            Err(io::Error::other(format!(
                "could not check whether process {pid} is running: probe failed"
            )))
        };
        let expected = "could not check whether process 4242 is running: probe failed";
        assert_eq!(
            inspect_running_daemon(&home, failed)
                .unwrap_err()
                .to_string(),
            expected
        );
        assert_eq!(
            proceed_preflight(&home, None, failed)
                .err()
                .unwrap()
                .to_string(),
            expected
        );
        assert_eq!(
            RunError::DaemonProbe(io::Error::other(expected)).to_string(),
            format!("Claude Code daemon: {expected}")
        );
        // A stopped daemon is no daemon; its spawner is not preserved.
        assert!(
            inspect_running_daemon(&home, |_| Ok(false))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            proceed_preflight(&home, None, |_| Ok(false))
                .unwrap()
                .preserve_spawned_by_pid,
            None
        );
    }

    /// Windows has no way to read another process's env, so a live daemon there
    /// is unverified: the launch asks or stops instead of reusing it.
    #[cfg(windows)]
    #[test]
    fn a_live_windows_daemon_is_unverified() {
        let home = temp_home();
        write_daemon_lock(&home, std::process::id());
        let state = inspect_running_daemon(&home, pid_is_alive)
            .unwrap()
            .expect("a running daemon");
        assert!(state.env_unreadable);
        assert_eq!(state.spawned_by_pid, Some(7));
        assert_eq!(
            classify_launch_safety(&state, &launch_request(), &home).unwrap(),
            Safety::Conflict
        );
    }
}
