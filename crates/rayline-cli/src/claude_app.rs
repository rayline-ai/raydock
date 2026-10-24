//! `rayline claude-desktop` — run Claude Code inside the **Claude desktop app**
//! against ARC through Rayline Local.
//!
//! Claude Desktop's Code sessions ignore `ANTHROPIC_BASE_URL`; the only way to
//! point them at a gateway is the app's third-party inference ("3P") mode,
//! configured through the local profile library or **Developer → Configure
//! Third-Party Inference…**. Three facts shape this module:
//!
//! 1. Desktop only accepts model ids that look like Anthropic models, so the
//!    app is configured with hidden compatibility ids ([`DESKTOP_MODELS`]),
//!    shown in the picker as `arc-sage` and `arc-swift`, and a dedicated local
//!    router maps each to its ARC tier through an explicit `model_routes` entry.
//!    ARC is not a Claude model; the ids only satisfy Desktop's model-id check.
//! 2. 3P mode applies to the whole app (it switches away from the claude.ai
//!    subscription), so the router runs as its **own** instance — its own state
//!    directory, ports, pid file and LaunchAgent — and never touches the shared
//!    `rayline claude` / `rayline codex` router or any Claude Code CLI config.
//! 3. The Rayline `rlk-` key stays inside the router process. Desktop receives a
//!    loopback URL and a per-instance gateway token (which only unlocks this
//!    router). The default command installs a recoverable local Desktop profile
//!    and launches the app; `--manual` retains the documented import workflow.

mod desktop;

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// One model the Desktop import offers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DesktopModel {
    /// The id Desktop sends. Desktop rejects ids that do not look like an
    /// Anthropic model, so this is a hidden local compatibility id; it never
    /// leaves the machine (the router rewrites it to `arc_model`).
    pub id: &'static str,
    /// The model-picker label Desktop shows (`labelOverride`).
    pub label: &'static str,
    /// The hosted ARC tier the id is routed to: the bare first-party id.
    pub arc_model: &'static str,
    /// The same tier's prefixed alias, which hosted routers that predate the
    /// bare ids accept. Used only with [`IdStyle::Prefixed`].
    pub prefixed_model: &'static str,
}

/// Which spelling of the tier ids the router forwards upstream.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum IdStyle {
    /// Bare first-party ids (`arc-sage`, `arc-swift`).
    #[default]
    Bare,
    /// Prefixed aliases (`rayline/arc-sage`, `rayline/arc-swift`), a fallback
    /// for a hosted router that does not serve the bare ids yet.
    Prefixed,
}

impl DesktopModel {
    /// The upstream id for this tier under `style`.
    pub fn upstream(&self, style: IdStyle) -> &'static str {
        match style {
            IdStyle::Bare => self.arc_model,
            IdStyle::Prefixed => self.prefixed_model,
        }
    }
}

/// The models the import offers, default first (Desktop's first
/// `inferenceModels` entry is its default).
pub const DESKTOP_MODELS: [DesktopModel; 2] = [
    DesktopModel {
        id: "claude-arc-sage",
        label: "arc-sage",
        arc_model: "arc-sage",
        prefixed_model: "rayline/arc-sage",
    },
    DesktopModel {
        id: "claude-arc-swift",
        label: "arc-swift",
        arc_model: "arc-swift",
        prefixed_model: "rayline/arc-swift",
    },
];
/// `x-rayline-client` value stamped on every upstream request.
pub const CLIENT_HEADER_VALUE: &str = "claude-desktop";
/// Prefix of the per-instance gateway token Desktop presents to the local
/// router. The token only unlocks this loopback router; it is not a Rayline
/// key and never leaves the machine.
pub const GATEWAY_TOKEN_PREFIX: &str = "rlt-";
/// Env var that disables every launchd action (the smoke test sets it so a
/// throwaway HOME can never load, reuse or unload the user's login service).
pub const NO_LAUNCHD_ENV: &str = "RAYLINE_CLAUDE_DESKTOP_NO_LAUNCHD";
/// launchd label of the optional login service.
pub const SERVICE_LABEL: &str = "ai.rayline.claude-desktop";
const STATE_DIR: &str = "claude-desktop";
const ENDPOINT_ID: &str = "rayline-arc";
const ROUTER_KEY_ENV: &str = "RAYLINE_ROUTER_API_KEY";
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
const HEALTH_POLL: Duration = Duration::from_millis(250);
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
/// Inherited variables with these prefixes are dropped before `rld` starts, so
/// a developer shell (`RAYLINE_PROXY_PORT`, `ANTHROPIC_BASE_URL`, ...) can never
/// reconfigure or leak into the Desktop instance.
const SCRUBBED_ENV_PREFIXES: [&str; 5] = [
    "RAYLINE_",
    "ANTHROPIC_",
    "CLAUDE_",
    "OPENAI_",
    "OPENROUTER_",
];

/// The Desktop instance's port block. Disjoint from the shared router
/// (20808-20811, metrics 20813) and the `--isolated` proxy (20814).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Ports {
    pub adapter: u16,
    pub injector: u16,
    pub router: u16,
    pub metrics: u16,
}

pub const PORTS: Ports = Ports {
    adapter: 20828,
    injector: 20829,
    router: 20831,
    metrics: 20833,
};

impl Ports {
    pub fn all(self) -> [u16; 4] {
        [self.adapter, self.injector, self.router, self.metrics]
    }
}

/// The URL Claude Desktop is pointed at.
pub fn gateway_url(ports: Ports) -> String {
    format!("http://127.0.0.1:{}", ports.router)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppAction {
    /// Start (or reuse) the gateway, configure Desktop and launch it.
    Start,
    /// Foreground: exec `rld` for the login service. Not for direct use.
    Serve,
    Status,
    Validate,
    Stop,
    InstallService,
    UninstallService,
    /// Restore the prior Desktop selection and stop the router.
    Off,
    Uninstall,
}

impl AppAction {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "start" => Self::Start,
            "serve" => Self::Serve,
            "status" => Self::Status,
            "validate" => Self::Validate,
            "stop" => Self::Stop,
            "install-service" => Self::InstallService,
            "uninstall-service" => Self::UninstallService,
            "off" => Self::Off,
            "uninstall" => Self::Uninstall,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppRequest {
    pub action: AppAction,
    /// Start the router and print import instructions without changing Desktop.
    pub manual: bool,
    /// `validate`: check this file instead of Desktop's applied configuration.
    pub desktop_config: Option<PathBuf>,
    /// Upstream tier-id spelling (`--prefixed-ids` selects the fallback).
    pub id_style: IdStyle,
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub root_env_explicit: bool,
}

/// Files belonging to the gateway. Desktop profile selection and recovery are
/// managed separately by `desktop`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppPaths {
    pub dir: PathBuf,
    pub router_config: PathBuf,
    pub desktop_import: PathBuf,
    pub pid_file: PathBuf,
    pub log_file: PathBuf,
    pub data_dir: PathBuf,
    pub launch_agent: PathBuf,
    /// The per-instance gateway token (mode 0600).
    pub token_file: PathBuf,
    /// What the running router was started with: its environment name and a
    /// digest of its key, token, config and `rld` (mode 0600).
    pub instance_file: PathBuf,
}

impl AppPaths {
    pub fn new(home: &Path) -> Self {
        let dir = home.join(crate::DOT_CONFIG_DIR).join(STATE_DIR);
        Self {
            router_config: dir.join("router.json"),
            desktop_import: dir.join("claude-desktop-import.json"),
            pid_file: dir.join("rld.pid"),
            log_file: dir.join("rld.log"),
            data_dir: dir.join("data"),
            token_file: dir.join("gateway-token"),
            instance_file: dir.join("instance.json"),
            launch_agent: home
                .join("Library")
                .join("LaunchAgents")
                .join(format!("{SERVICE_LABEL}.plist")),
            dir,
        }
    }
}

/// Router config: each Desktop id is routed to its ARC tier by an explicit
/// `model_routes` entry; any other id (main, subagent or default) goes to the
/// default tier. `replace_defaults` drops the router's built-in endpoints, so
/// no model id can reach anything but ARC. The key is referenced by env var
/// name only.
pub fn router_config_json(router_url: &str, style: IdStyle) -> Value {
    let target = |arc_model: &str| json!({ "endpoint": ENDPOINT_ID, "model": arc_model });
    let default = target(DESKTOP_MODELS[0].upstream(style));
    let model_routes: serde_json::Map<String, Value> = DESKTOP_MODELS
        .iter()
        .map(|model| (model.id.to_owned(), target(model.upstream(style))))
        .collect();
    let upstream: Vec<&str> = DESKTOP_MODELS
        .iter()
        .map(|model| model.upstream(style))
        .collect();
    json!({
        "replace_defaults": true,
        "endpoints": [{
            "id": ENDPOINT_ID,
            "protocol": "anthropic_messages",
            "base_url": router_url,
            "api_key_env": ROUTER_KEY_ENV,
            "models": upstream,
            "headers": { "x-rayline-client": CLIENT_HEADER_VALUE }
        }],
        "routes": {
            "main": default,
            "subagent": default,
            "default": default,
            "model_routes": model_routes,
            "subagents": {}
        }
    })
}

/// The configuration file the user loads with Desktop's **Import
/// configuration**. Keys are Desktop's documented managed-configuration keys.
pub fn desktop_import_json(gateway_url: &str, token: &str) -> Value {
    json!({
        "inferenceProvider": "gateway",
        "inferenceGatewayBaseUrl": gateway_url,
        "inferenceCredentialKind": "static",
        "inferenceGatewayApiKey": token,
        "modelDiscoveryEnabled": false,
        "inferenceModels": DESKTOP_MODELS
            .iter()
            .map(|model| json!({ "name": model.id, "labelOverride": model.label }))
            .collect::<Vec<_>>(),
        "coworkTabEnabled": false
    })
}

/// A fresh gateway token: [`GATEWAY_TOKEN_PREFIX`] and 32 random bytes in hex.
pub fn generate_gateway_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    random_bytes(&mut bytes)?;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{GATEWAY_TOKEN_PREFIX}{hex}"))
}

#[cfg(unix)]
fn random_bytes(buffer: &mut [u8]) -> io::Result<()> {
    use std::io::Read;
    fs::File::open("/dev/urandom")?.read_exact(buffer)
}

#[cfg(not(unix))]
fn random_bytes(_buffer: &mut [u8]) -> io::Result<()> {
    Err(io::Error::other(
        "the Claude Desktop router is only supported on macOS and Linux",
    ))
}

fn is_gateway_token(token: &str) -> bool {
    token
        .strip_prefix(GATEWAY_TOKEN_PREFIX)
        .is_some_and(|hex| hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

/// Create `dir` and narrow it to 0700, so no other user can rename or replace
/// the files in it whatever the umask.
fn private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `contents` readable only by the owner: into a new 0600 file beside
/// `path`, then renamed over it. Whatever was at `path` (a loosened file, a
/// planted symlink) is replaced, never written through.
fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or(Path::new("."));
    private_dir(dir)?;
    let name = path
        .file_name()
        .map(OsStr::to_string_lossy)
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    match fs::remove_file(&tmp) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let written = file.write_all(contents).and_then(|()| file.sync_all());
    if let Err(error) = written.and_then(|()| fs::rename(&tmp, path)) {
        let _ = fs::remove_file(&tmp);
        return Err(error);
    }
    Ok(())
}

/// This instance's gateway token, created on first use and kept across
/// restarts so an imported Desktop configuration stays valid.
pub fn ensure_gateway_token(paths: &AppPaths) -> io::Result<String> {
    if let Some(existing) = read_private(&paths.token_file) {
        let existing = existing.trim();
        if is_gateway_token(existing) {
            return Ok(existing.to_owned());
        }
    }
    let token = generate_gateway_token()?;
    write_private(&paths.token_file, format!("{token}\n").as_bytes())?;
    Ok(token)
}

/// `path`'s contents if it is a regular file owned by this user that no one
/// else can read or write, checked on the opened file (not followed through a
/// symlink, so it cannot be swapped between check and read). A token file
/// failing this (planted, or loosened) is replaced.
#[cfg(unix)]
fn read_private(path: &Path) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let mut file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    let uid = unsafe { libc::geteuid() };
    if !meta.file_type().is_file() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
        return None;
    }
    let mut contents = String::new();
    file.read_to_string(&mut contents).ok()?;
    Some(contents)
}

#[cfg(not(unix))]
fn read_private(_path: &Path) -> Option<String> {
    None
}

/// Arguments for `rld serve` (after the binary).
pub fn rld_args(paths: &AppPaths, ports: Ports) -> Vec<OsString> {
    let mut args: Vec<OsString> = [
        "serve",
        // An `rld` without the token guard rejects this flag and exits, so a
        // launchd restart onto an older binary never serves unguarded.
        "--require-inbound-token",
        "--no-local-model",
        "--decision-plane",
        "local",
        "--adapter-port",
        &ports.adapter.to_string(),
        "--injector-port",
        &ports.injector.to_string(),
        "--local-router-port",
        &ports.router.to_string(),
        "--metrics-port",
        &ports.metrics.to_string(),
    ]
    .iter()
    .map(OsString::from)
    .collect();
    args.push("--router-config-path".into());
    args.push(paths.router_config.clone().into_os_string());
    args.push("--data-dir".into());
    args.push(paths.data_dir.clone().into_os_string());
    args
}

/// The parent environment minus every variable that could reconfigure `rld` or
/// carry another tool's credentials.
pub fn scrubbed_env<I>(vars: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter(|(name, _)| {
            let name = name.to_string_lossy();
            !SCRUBBED_ENV_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .collect()
}

fn rld_command(rld_bin: &Path, paths: &AppPaths, key: &str, token: &str) -> Command {
    let mut command = Command::new(rld_bin);
    command.args(rld_args(paths, PORTS));
    command.env_clear();
    command.envs(scrubbed_env(std::env::vars_os()));
    command.env(ROUTER_KEY_ENV, key);
    command.env(
        rayline_local_router::inbound_guard::INBOUND_TOKEN_ENV,
        token,
    );
    command.env("RUST_LOG", "warn");
    command
}

/// What `stop` may signal: only a process whose command line names this
/// instance's router config.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StopTarget {
    NotRunning,
    Ours(i32),
    /// The pid file names a live process that is not this instance (pid reuse
    /// or a hand-edited file). It is left untouched.
    Foreign(i32),
}

pub fn stop_target(
    pid: Option<i32>,
    paths: &AppPaths,
    alive: impl Fn(i32) -> bool,
    command_line: impl Fn(i32) -> Option<String>,
) -> StopTarget {
    let Some(pid) = pid.filter(|pid| *pid > 1) else {
        return StopTarget::NotRunning;
    };
    if !alive(pid) {
        return StopTarget::NotRunning;
    }
    match command_line(pid) {
        Some(line) if command_line_is_ours(&line, paths) => StopTarget::Ours(pid),
        _ => StopTarget::Foreign(pid),
    }
}

/// Whether `line` (a `ps` command line) is `rld serve` with exactly this
/// instance's `--router-config-path`. Another process that merely mentions the
/// path (an editor, `tail`, a `.bak` copy) does not match.
pub fn command_line_is_ours(line: &str, paths: &AppPaths) -> bool {
    let needle = format!(
        " --router-config-path {}",
        paths.router_config.to_string_lossy()
    );
    let names_config = line.match_indices(&needle).any(|(at, _)| {
        let rest = &line[at + needle.len()..];
        rest.is_empty() || rest.starts_with(' ')
    });
    // The program itself must be `rld serve`: an absolute `.../rld` as the first
    // word (this launcher always spawns it by absolute path), not a later
    // argument of some other program.
    let mut words = line.split_whitespace();
    let runs_rld_serve = words.next().is_some_and(|program| {
        let program = Path::new(program);
        program.is_absolute() && program.file_name() == Some(OsStr::new("rld"))
    }) && words.next() == Some("serve");
    names_config && runs_rld_serve
}

fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // SAFETY: kill(pid, 0) delivers no signal; it only reports whether the pid
    // exists and is signalable by this user.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: i32) -> bool {
    false
}

fn process_command_line(pid: i32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !line.is_empty()).then_some(line)
}

#[cfg(unix)]
fn terminate(pid: i32) -> io::Result<()> {
    // SAFETY: `pid` was checked by `stop_target` to be this instance's rld.
    if unsafe { libc::kill(pid, libc::SIGTERM) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn terminate(_pid: i32) -> io::Result<()> {
    Err(io::Error::other(
        "stopping the router is not supported on this platform",
    ))
}

/// Stop this instance's detached router, never anything else.
pub fn stop_instance(paths: &AppPaths) -> io::Result<String> {
    stop_instance_with(paths, process_command_line)
}

/// [`stop_instance`] with the `ps` command-line lookup injectable, so a test
/// can stop a real child without depending on how a platform's `ps` prints it.
fn stop_instance_with(
    paths: &AppPaths,
    command_line: impl Fn(i32) -> Option<String>,
) -> io::Result<String> {
    let target = stop_target(
        read_pid(&paths.pid_file),
        paths,
        process_alive,
        command_line,
    );
    match target {
        StopTarget::NotRunning => {
            let _ = fs::remove_file(&paths.pid_file);
            Ok("The Claude Desktop router is not running.".to_owned())
        }
        StopTarget::Foreign(pid) => Err(io::Error::other(format!(
            "pid {pid} in {} is not the Claude Desktop router; left it untouched",
            paths.pid_file.display()
        ))),
        StopTarget::Ours(pid) => {
            terminate(pid)?;
            let deadline = Instant::now() + STOP_TIMEOUT;
            while process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(HEALTH_POLL);
            }
            if process_alive(pid) {
                return Err(io::Error::other(format!(
                    "the Claude Desktop router (pid {pid}) did not exit after SIGTERM"
                )));
            }
            let _ = fs::remove_file(&paths.pid_file);
            Ok(format!("Stopped the Claude Desktop router (pid {pid})."))
        }
    }
}

/// Ports of the block that something else is listening on.
fn busy_ports(ports: Ports) -> Vec<u16> {
    ports
        .all()
        .into_iter()
        .filter(|port| TcpListener::bind(("127.0.0.1", *port)).is_err())
        .collect()
}

// ---------------------------------------------------------------------------
// Desktop configuration validation (read-only).

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Finding {
    pub ok: bool,
    pub message: String,
}

/// Desktop stores arrays natively in its config library and as JSON-encoded
/// strings in macOS managed preferences; accept both.
fn json_or_string(value: Option<&Value>) -> Option<Value> {
    match value? {
        Value::String(text) => serde_json::from_str(text).ok(),
        other => Some(other.clone()),
    }
}

fn bool_like(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(flag) => Some(*flag),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Check a Desktop third-party configuration against this instance. Reads key
/// names and non-secret values only; the gateway key is never echoed.
pub fn validate_desktop_config(
    config: &Value,
    gateway_url: &str,
    token: Option<&str>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut check = |ok: bool, message: String| findings.push(Finding { ok, message });
    let provider = config.get("inferenceProvider").and_then(Value::as_str);
    check(
        provider == Some("gateway"),
        format!(
            "inferenceProvider is {} (want \"gateway\")",
            provider.map_or("unset".to_owned(), |value| format!("\"{value}\""))
        ),
    );
    let base_url = config
        .get("inferenceGatewayBaseUrl")
        .and_then(Value::as_str)
        .map(|url| url.trim_end_matches('/'));
    check(
        base_url == Some(gateway_url),
        format!(
            "inferenceGatewayBaseUrl is {} (want {gateway_url})",
            base_url.unwrap_or("unset")
        ),
    );
    let key = config
        .get("inferenceGatewayApiKey")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty());
    let (key_ok, key_message) = match (key, token) {
        (None, _) => (
            false,
            "inferenceGatewayApiKey is empty (re-import the configuration)",
        ),
        (Some(key), Some(token)) if key == token => (
            true,
            "inferenceGatewayApiKey matches this router's token (value not shown)",
        ),
        (Some(_), Some(_)) => (
            false,
            "inferenceGatewayApiKey does not match this router's token (value not shown); \
             re-import the configuration",
        ),
        (Some(_), None) => (
            false,
            "inferenceGatewayApiKey is set, but this router has no token yet; run \
             `rayline claude-desktop` and re-import",
        ),
    };
    check(key_ok, key_message.to_owned());
    let models = json_or_string(config.get("inferenceModels"));
    let names: Vec<String> = models
        .as_ref()
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| match entry {
                    Value::String(name) => Some(name.clone()),
                    Value::Object(object) => object
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let wanted: Vec<&str> = DESKTOP_MODELS.iter().map(|model| model.id).collect();
    check(
        wanted.iter().all(|id| names.iter().any(|name| name == id)),
        format!(
            "inferenceModels lists {} (want {})",
            if names.is_empty() {
                "nothing".to_owned()
            } else {
                names.join(", ")
            },
            wanted.join(" and ")
        ),
    );
    let discovery = bool_like(config.get("modelDiscoveryEnabled"));
    check(
        discovery == Some(false),
        format!(
            "modelDiscoveryEnabled is {} (want false: the router lists no Desktop models)",
            discovery.map_or("unset".to_owned(), |flag| flag.to_string())
        ),
    );
    findings
}

/// Desktop's applied third-party configuration on macOS:
/// `Claude-3p/configLibrary/<appliedId>.json`, named by `_meta.json`.
pub fn applied_desktop_config_path(home: &Path) -> io::Result<PathBuf> {
    let library = home
        .join("Library")
        .join("Application Support")
        .join("Claude-3p")
        .join("configLibrary");
    let meta: Value =
        serde_json::from_slice(&fs::read(library.join("_meta.json"))?).map_err(io::Error::other)?;
    let id = meta
        .get("appliedId")
        .and_then(Value::as_str)
        .filter(|id| {
            !id.is_empty()
                && id
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        })
        .ok_or_else(|| io::Error::other("Desktop has no applied third-party configuration"))?;
    Ok(library.join(format!("{id}.json")))
}

fn render_findings(source: &Path, findings: &[Finding]) -> (bool, String) {
    let ok = findings.iter().all(|finding| finding.ok);
    let mut out = format!("Claude Desktop configuration: {}\n", source.display());
    for finding in findings {
        out.push_str(if finding.ok { "  ok    " } else { "  FAIL  " });
        out.push_str(&finding.message);
        out.push('\n');
    }
    if !ok {
        out.push_str("Re-import the file `rayline claude-desktop` prints, then Apply Changes.\n");
    }
    (ok, out)
}

// ---------------------------------------------------------------------------
// LaunchAgent.

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// LaunchAgent that runs `rayline [--env <env>] claude-desktop serve` at login. The
/// plist carries no secret: `serve` reads the key from Rayline's credentials.
/// `RLD_BIN` pins the `rld` this command resolved, because launchd's PATH is
/// not the user's shell PATH.
pub fn launch_agent_plist(
    rayline_bin: &Path,
    rld_bin: &Path,
    env_name: Option<&str>,
    style: IdStyle,
    paths: &AppPaths,
) -> String {
    let mut program = vec![rayline_bin.to_string_lossy().into_owned()];
    if let Some(env) = env_name {
        program.push("--env".to_owned());
        program.push(env.to_owned());
    }
    program.extend(["claude-desktop", "serve"].map(str::to_owned));
    if style == IdStyle::Prefixed {
        program.push("--prefixed-ids".to_owned());
    }
    let arguments: String = program
        .iter()
        .map(|arg| format!("\t\t<string>{}</string>\n", xml_escape(arg)))
        .collect();
    let log = xml_escape(&paths.log_file.to_string_lossy());
    let dir = xml_escape(&paths.dir.to_string_lossy());
    let rld = xml_escape(&rld_bin.to_string_lossy());
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\">\n\
<dict>\n\
\t<key>Label</key>\n\t<string>{SERVICE_LABEL}</string>\n\
\t<key>ProgramArguments</key>\n\t<array>\n{arguments}\t</array>\n\
\t<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>RLD_BIN</key>\n\t\t<string>{rld}</string>\n\t</dict>\n\
\t<key>RunAtLoad</key>\n\t<true/>\n\
\t<key>KeepAlive</key>\n\t<true/>\n\
\t<key>ThrottleInterval</key>\n\t<integer>30</integer>\n\
\t<key>WorkingDirectory</key>\n\t<string>{dir}</string>\n\
\t<key>StandardOutPath</key>\n\t<string>{log}</string>\n\
\t<key>StandardErrorPath</key>\n\t<string>{log}</string>\n\
</dict>\n\
</plist>\n"
    )
}

#[cfg(unix)]
fn launchd_domain() -> String {
    // SAFETY: getuid has no preconditions and cannot fail.
    format!("gui/{}", unsafe { libc::getuid() })
}

#[cfg(not(unix))]
fn launchd_domain() -> String {
    String::new()
}

/// launchd actions run only on macOS, and never when [`NO_LAUNCHD_ENV`] is
/// set (an isolated test HOME must not touch the user's real login service).
fn launchd_enabled() -> bool {
    cfg!(target_os = "macos") && std::env::var_os(NO_LAUNCHD_ENV).is_none()
}

fn service_loaded() -> bool {
    launchd_enabled()
        && Command::new("launchctl")
            .args(["print", &format!("{}/{SERVICE_LABEL}", launchd_domain())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
}

/// Unload the login service if it is loaded. A failure that leaves it loaded
/// is reported, not ignored.
fn bootout_service() -> Result<bool, String> {
    if !service_loaded() {
        return Ok(false);
    }
    let output = Command::new("launchctl")
        .args(["bootout", &format!("{}/{SERVICE_LABEL}", launchd_domain())])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("launchctl bootout failed to run: {error}"))?;
    if !output.status.success() && service_loaded() {
        return Err(format!(
            "launchctl bootout {SERVICE_LABEL} failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(true)
}

fn bootstrap_service(plist: &Path) -> Result<(), String> {
    let output = Command::new("launchctl")
        .args([
            OsStr::new("bootstrap"),
            OsStr::new(&launchd_domain()),
            plist.as_os_str(),
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("launchctl bootstrap failed to run: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "launchctl bootstrap failed for {} ({}): {}",
            plist.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

// ---------------------------------------------------------------------------
// Runtime.

struct Resolved {
    home: PathBuf,
    env: String,
    router_url: String,
}

fn resolve(request: &AppRequest) -> Result<Resolved, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let env = crate::status::resolve_env(request.env_name.as_deref(), Some(&home));
    let hosted = crate::status::resolve_hosted_environment(&env, Some(&home))
        .map_err(|error| error.to_string())?;
    if !crate::router_config::endpoint_base_url_is_hosted_rcr(Some(&hosted.router_url)) {
        return Err(format!(
            "environment '{env}' routes to {}, which does not serve ARC",
            hosted.router_url
        ));
    }
    Ok(Resolved {
        home,
        env,
        router_url: hosted.router_url,
    })
}

fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        private_dir(dir)?;
    }
    let mut body = serde_json::to_vec_pretty(value).map_err(io::Error::other)?;
    body.push(b'\n');
    fs::write(path, body)
}

/// Write the router config and the Desktop import file. Returns the router
/// config, which is part of the instance digest.
fn write_instance_files(
    paths: &AppPaths,
    router_url: &str,
    style: IdStyle,
    token: &str,
) -> io::Result<Value> {
    let router = router_config_json(router_url, style);
    write_json(&paths.router_config, &router)?;
    let mut import = serde_json::to_vec_pretty(&desktop_import_json(&gateway_url(PORTS), token))
        .map_err(io::Error::other)?;
    import.push(b'\n');
    write_private(&paths.desktop_import, &import)?;
    Ok(router)
}

/// A digest of everything the running router was started with. A change to
/// any of them (a re-login or rotated key, a new token, a new config or `rld`)
/// means the router must restart.
pub fn instance_digest(router: &Value, key: &str, token: &str, rld_bin: &Path) -> String {
    let router = router.to_string();
    let rld_identity = binary_identity(rld_bin);
    let mut hasher = Sha256::new();
    for part in [
        router.as_bytes(),
        key.as_bytes(),
        token.as_bytes(),
        rld_bin.as_os_str().as_encoded_bytes(),
        &rld_identity,
    ] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    format!("{:x}", hasher.finalize())
}

/// What changes when the file at `path` is replaced or rewritten: its size,
/// modification time and (on unix) device and inode. Empty when unreadable.
fn binary_identity(path: &Path) -> Vec<u8> {
    let Ok(meta) = fs::metadata(path) else {
        return Vec::new();
    };
    let mut identity = meta.len().to_be_bytes().to_vec();
    if let Ok(modified) = meta.modified()
        && let Ok(since) = modified.duration_since(std::time::UNIX_EPOCH)
    {
        identity.extend(since.as_nanos().to_be_bytes());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        identity.extend(meta.dev().to_be_bytes());
        identity.extend(meta.ino().to_be_bytes());
    }
    identity
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Instance {
    pub env: String,
    pub digest: String,
}

fn write_instance(paths: &AppPaths, instance: &Instance) -> io::Result<()> {
    let mut body = serde_json::to_vec_pretty(&json!({
        "env": instance.env,
        "digest": instance.digest,
    }))
    .map_err(io::Error::other)?;
    body.push(b'\n');
    write_private(&paths.instance_file, &body)
}

fn read_instance(paths: &AppPaths) -> Option<Instance> {
    let value: Value = serde_json::from_slice(&fs::read(&paths.instance_file).ok()?).ok()?;
    Some(Instance {
        env: value.get("env")?.as_str()?.to_owned(),
        digest: value.get("digest")?.as_str()?.to_owned(),
    })
}

async fn router_healthy(client: &reqwest::Client) -> bool {
    client
        .get(format!("{}/healthz", gateway_url(PORTS)))
        .send()
        .await
        .is_ok_and(|response| response.status().is_success())
}

/// Whether the router on the gateway port is this instance, enforcing its
/// token: `/v1/models` (answered locally, never forwarded) is refused without
/// the token and served with it. A router from an `rld` that predates the
/// guard, or anything else on the port, fails one of the two.
async fn router_guarded(client: &reqwest::Client, token: &str) -> bool {
    let url = format!("{}/v1/models", gateway_url(PORTS));
    let refused = client
        .get(&url)
        .send()
        .await
        .is_ok_and(|response| response.status() == reqwest::StatusCode::UNAUTHORIZED);
    refused
        && client
            .get(&url)
            .bearer_auth(token)
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
}

/// Wait until the router answers as this guarded instance. With `pid` (a
/// router this command just spawned) a healthy but unguarded answer fails at
/// once; a launchd-run router is polled until the deadline, because the
/// previous process may still be exiting.
async fn wait_guarded(client: &reqwest::Client, token: &str, pid: Option<i32>) -> bool {
    let deadline = Instant::now() + HEALTH_TIMEOUT;
    while Instant::now() < deadline {
        if router_healthy(client).await {
            if router_guarded(client, token).await {
                return true;
            }
            if pid.is_some() {
                return false;
            }
        }
        if pid.is_some_and(|pid| !process_alive(pid)) {
            return false;
        }
        tokio::time::sleep(HEALTH_POLL).await;
    }
    false
}

fn unguarded_error(paths: &AppPaths, rld_bin: &Path) -> String {
    format!(
        "the router on {} did not come up enforcing its gateway token, so any local \
         process could spend your Rayline key; it was stopped. {} may be older than this \
         CLI: update Rayline and run `rayline claude-desktop` again. See {}",
        gateway_url(PORTS),
        rld_bin.display(),
        paths.log_file.display()
    )
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .no_proxy()
        .build()
        .unwrap_or_default()
}

fn setup_instructions(paths: &AppPaths, router_url: &str) -> String {
    format!(
        "\
Rayline ARC gateway for Claude Desktop is running.
  Gateway URL:  {gateway}
  Desktop models: arc-sage (default) and arc-swift, served by ARC at {router_url}.
  Desktop requires Claude-style model ids, so the import uses claude-arc-sage
  and claude-arc-swift internally. ARC is not a Claude model, and Rayline is
  not affiliated with Anthropic.
  Import file:  {import}
  Your Rayline key stays in the router process. Desktop stores only a gateway
  token that unlocks this local router.

Before you continue: third-party mode applies to the whole Claude Desktop app.
It switches away from your claude.ai subscription, and Chat and Cowork are
off while it is active. Run `rayline claude-desktop off` to see how to switch back.

Connect Claude Desktop (once):
  1. Open Claude Desktop. From the menu bar choose Help > Troubleshooting >
     Enable Developer Mode (skip if the Developer menu already exists).
  2. Choose Developer > Configure Third-Party Inference...
  3. Choose Import configuration and select the import file above.
  4. Click Apply Changes, then Save & Restart.
  5. On the sign-in screen, choose to continue with the configured gateway.
  6. Open Code and pick arc-sage, or arc-swift for the cheaper tier.

Check the setup with `rayline claude-desktop validate`. Keep the router running
across logins with `rayline claude-desktop install-service`.
Anthropic does not support routing Claude Code to non-Claude models through a
gateway, so a Desktop update can change this behaviour.
",
        gateway = gateway_url(PORTS),
        import = paths.desktop_import.display(),
    )
}

fn off_instructions() -> &'static str {
    "\
To return Claude Desktop to your claude.ai subscription:
  1. In Claude Desktop, open the account menu at the bottom left (it shows
     \"Gateway\") and choose Sign out.
  2. On the sign-in screen, sign in with your Anthropic account instead of the
     configured gateway.
Your subscription data is kept. The imported gateway configuration stays
available under Developer > Configure Third-Party Inference... for next time.
"
}

pub async fn run(request: AppRequest) -> ExitCode {
    match run_inner(&request).await {
        Ok(message) => {
            print!("{message}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("Error: {message}");
            ExitCode::from(1)
        }
    }
}

/// A cross-process operation lease, outside the removable instance directory.
/// LOCK_NB makes contenders fail immediately. This is intentionally held across
/// async gateway startup: it serializes lifecycle commands, not an in-process
/// mutex, and no executor thread blocks waiting for another command's lease.
#[cfg(unix)]
fn lifecycle_lock(home: &Path) -> io::Result<fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let root = home.join(crate::DOT_CONFIG_DIR);
    private_dir(&root)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("claude-desktop.lock"))?;
    // SAFETY: flock receives a live descriptor and does not retain pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(io::Error::other(
            "Another Rayline command is changing the Desktop lifecycle",
        ));
    }
    Ok(file)
}

#[cfg(not(unix))]
fn lifecycle_lock(_home: &Path) -> io::Result<fs::File> {
    Err(io::Error::other(
        "Claude Desktop lifecycle commands require macOS or Linux",
    ))
}

async fn run_inner(request: &AppRequest) -> Result<String, String> {
    // `serve` is the launchd child of a leased start/install-service command;
    // it must not attempt to reacquire its parent's lease before becoming healthy.
    let _lease = if matches!(
        request.action,
        AppAction::Start
            | AppAction::Stop
            | AppAction::Off
            | AppAction::Uninstall
            | AppAction::InstallService
            | AppAction::UninstallService
    ) {
        let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
        Some(lifecycle_lock(&home).map_err(|error| error.to_string())?)
    } else {
        None
    };
    match request.action {
        AppAction::Start => {
            let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
            if !request.manual {
                desktop::preflight(&home).map_err(|error| error.to_string())?;
            }
            let instructions = start(request).await?;
            if request.manual {
                return Ok(instructions);
            }
            let paths = AppPaths::new(&home);
            let token = fs::read_to_string(&paths.token_file).map_err(|error| error.to_string())?;
            let launch = desktop::launch(
                &home,
                &desktop_import_json(&gateway_url(PORTS), token.trim()),
            )
            .map_err(|error| {
                format!(
                    "{error}. The gateway remains running; use --manual for import instructions"
                )
            })?;
            Ok(format!(
                "Rayline ARC gateway is running at {}.\n{launch}",
                gateway_url(PORTS)
            ))
        }
        AppAction::Serve => serve(request),
        AppAction::Status => status(request).await,
        AppAction::Validate => validate(request),
        AppAction::Stop => stop(),
        AppAction::InstallService => install_service(request).await,
        AppAction::UninstallService => uninstall_service(),
        AppAction::Off => {
            let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
            let restored = desktop::restore(&home).map_err(|error| error.to_string())?;
            let stopped = stop()?;
            if restored {
                Ok(format!(
                    "Restored your previous Claude Desktop profile and deployment mode. You may need to sign in again.\n{stopped}\n"
                ))
            } else {
                Ok(format!("{}\n{stopped}\n", off_instructions()))
            }
        }
        AppAction::Uninstall => uninstall(),
    }
}

/// The `rld` this instance runs, as an absolute path whose file name is `rld`.
/// `stop` recognises the router by `rld serve` on its command line, and the
/// LaunchAgent runs from the state directory, so a renamed or relative
/// `RLD_BIN` is refused here rather than leaving a router `stop` cannot see or
/// a service that cannot start.
fn desktop_rld_bin(home: &Path) -> Result<PathBuf, String> {
    let rld_bin = crate::router::resolve_rld_bin(home).map_err(|error| error.to_string())?;
    let rld_bin =
        std::path::absolute(&rld_bin).map_err(|error| format!("{}: {error}", rld_bin.display()))?;
    if rld_bin.file_name() != Some(OsStr::new("rld")) {
        return Err(format!(
            "the Claude Desktop router needs an rld binary named `rld`, not {}",
            rld_bin.display()
        ));
    }
    // `stop` recognises the router by its `ps` command line, split on
    // whitespace, so the program path must be one word.
    if rld_bin.to_string_lossy().chars().any(char::is_whitespace) {
        return Err(format!(
            "the Claude Desktop router needs an rld path without spaces, not {}",
            rld_bin.display()
        ));
    }
    Ok(rld_bin)
}

fn home_paths() -> Result<AppPaths, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    Ok(AppPaths::new(&home))
}

async fn start(request: &AppRequest) -> Result<String, String> {
    let resolved = resolve(request)?;
    let paths = AppPaths::new(&resolved.home);
    // Provision or read the key first: an interactive login must happen before
    // anything starts. The key is only ever handed to rld's environment.
    let key = crate::claude::ensure_router_key(
        &resolved.env,
        &resolved.home,
        request.auth_token.as_deref(),
        request.root_env_explicit,
    )
    .await
    .map_err(|error| error.to_string())?;
    let token = ensure_gateway_token(&paths)
        .map_err(|error| format!("failed to create the gateway token: {error}"))?;
    let rld_bin = desktop_rld_bin(&resolved.home)?;
    let router = write_instance_files(&paths, &resolved.router_url, request.id_style, &token)
        .map_err(|error| format!("failed to write {}: {error}", paths.dir.display()))?;
    let instance = Instance {
        env: resolved.env.clone(),
        digest: instance_digest(&router, &key, &token, &rld_bin),
    };
    let current = read_instance(&paths).as_ref() == Some(&instance);
    let client = http_client();
    let instructions = setup_instructions(&paths, &resolved.router_url);
    let rayline_bin = std::env::current_exe().map_err(|error| error.to_string())?;
    let env_name = request.root_env_explicit.then_some(resolved.env.as_str());
    let plist = launch_agent_plist(&rayline_bin, &rld_bin, env_name, request.id_style, &paths);
    let installed_plist = fs::read_to_string(&paths.launch_agent).ok();

    if service_loaded() {
        // The service runs with the settings in its plist. Rewrite it for this
        // request first, so a restart can never serve one environment while
        // this command reports another.
        if installed_plist.as_deref() != Some(plist.as_str()) {
            bootout_service()?;
            fs::write(&paths.launch_agent, &plist).map_err(|error| error.to_string())?;
            bootstrap_service(&paths.launch_agent)?;
        } else if !current || !router_guarded(&client, &token).await {
            let status = Command::new("launchctl")
                .args([
                    "kickstart",
                    "-k",
                    &format!("{}/{SERVICE_LABEL}", launchd_domain()),
                ])
                .stdin(Stdio::null())
                .status()
                .map_err(|error| format!("launchctl kickstart failed to run: {error}"))?;
            if !status.success() {
                return Err(format!(
                    "launchctl kickstart {SERVICE_LABEL} failed ({status})"
                ));
            }
        }
        if !wait_guarded(&client, &token, None).await {
            let _ = bootout_service();
            return Err(unguarded_error(&paths, &rld_bin));
        }
        return Ok(instructions);
    }

    // An installed but unloaded service (after `stop`) loads its plist at the
    // next login: keep it on this request's settings too.
    if launchd_enabled()
        && installed_plist
            .as_deref()
            .is_some_and(|installed| installed != plist)
    {
        fs::write(&paths.launch_agent, &plist).map_err(|error| error.to_string())?;
    }

    let running = matches!(
        stop_target(
            read_pid(&paths.pid_file),
            &paths,
            process_alive,
            process_command_line
        ),
        StopTarget::Ours(_)
    );
    if running && current && router_guarded(&client, &token).await {
        return Ok(instructions);
    }
    if running {
        stop_instance(&paths).map_err(|error| error.to_string())?;
    }
    let busy = busy_ports(PORTS);
    if !busy.is_empty() {
        return Err(format!(
            "port(s) {busy:?} are in use by another process. The Claude Desktop router \
             needs {:?} and will not touch a process it did not start.",
            PORTS.all()
        ));
    }
    private_dir(&paths.dir).map_err(|error| error.to_string())?;
    fs::create_dir_all(&paths.data_dir).map_err(|error| error.to_string())?;
    let mut log_options = fs::OpenOptions::new();
    log_options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_options.mode(0o600);
    }
    let log = log_options
        .open(&paths.log_file)
        .map_err(|error| error.to_string())?;
    let mut command = rld_command(&rld_bin, &paths, &key, &token);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(
            log.try_clone().map_err(|error| error.to_string())?,
        ))
        .stderr(Stdio::from(log));
    // Detach from the launching terminal's session so closing it does not
    // SIGHUP the router (mirrors the shared router's spawn).
    #[cfg(unix)]
    // SAFETY: the pre_exec closure only calls setsid, which is
    // async-signal-safe, between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|error| format!("failed to start {}: {error}", rld_bin.display()))?;
    let pid = child.id() as i32;
    fs::write(&paths.pid_file, format!("{pid}\n")).map_err(|error| error.to_string())?;
    if !wait_guarded(&client, &token, Some(pid)).await {
        let _ = stop_instance(&paths);
        return Err(if process_alive(pid) || router_healthy(&client).await {
            unguarded_error(&paths, &rld_bin)
        } else {
            format!(
                "the Claude Desktop router did not become healthy; see {}",
                paths.log_file.display()
            )
        });
    }
    write_instance(&paths, &instance).map_err(|error| error.to_string())?;
    Ok(instructions)
}

/// Foreground mode for the LaunchAgent: write the config, then replace this
/// process with `rld` so launchd supervises the router directly.
fn serve(request: &AppRequest) -> Result<String, String> {
    let resolved = resolve(request)?;
    let paths = AppPaths::new(&resolved.home);
    let key = crate::status::load_claude_key_from_home(&resolved.env, &resolved.home).ok_or_else(
        || {
            format!(
                "no Rayline key for '{}'; run `rayline auth login` (the login service \
                 never prompts)",
                resolved.env
            )
        },
    )?;
    let token = ensure_gateway_token(&paths).map_err(|error| error.to_string())?;
    let rld_bin = desktop_rld_bin(&resolved.home)?;
    let router = write_instance_files(&paths, &resolved.router_url, request.id_style, &token)
        .map_err(|error| error.to_string())?;
    write_instance(
        &paths,
        &Instance {
            env: resolved.env.clone(),
            digest: instance_digest(&router, &key, &token, &rld_bin),
        },
    )
    .map_err(|error| error.to_string())?;
    private_dir(&paths.dir).map_err(|error| error.to_string())?;
    fs::create_dir_all(&paths.data_dir).map_err(|error| error.to_string())?;
    fs::write(&paths.pid_file, format!("{}\n", std::process::id()))
        .map_err(|error| error.to_string())?;
    let mut command = rld_command(&rld_bin, &paths, &key, &token);
    exec(&mut command)
}

#[cfg(unix)]
fn exec(command: &mut Command) -> Result<String, String> {
    use std::os::unix::process::CommandExt;
    Err(format!("failed to exec rld: {}", command.exec()))
}

#[cfg(not(unix))]
fn exec(_command: &mut Command) -> Result<String, String> {
    Err("the Claude Desktop login service is only supported on macOS".to_owned())
}

async fn status(request: &AppRequest) -> Result<String, String> {
    let paths = home_paths()?;
    let client = http_client();
    let mut out = String::new();
    let healthy = router_healthy(&client).await;
    let owner = if service_loaded() {
        "login service".to_owned()
    } else {
        match stop_target(
            read_pid(&paths.pid_file),
            &paths,
            process_alive,
            process_command_line,
        ) {
            StopTarget::Ours(pid) => format!("pid {pid}"),
            StopTarget::Foreign(pid) => format!("stale pid file (pid {pid} is not ours)"),
            StopTarget::NotRunning => "not running".to_owned(),
        }
    };
    out.push_str(&format!(
        "Claude Desktop router: {} ({owner})\n  Gateway URL: {}\n  State: {}\n",
        if healthy { "healthy" } else { "not answering" },
        gateway_url(PORTS),
        paths.dir.display()
    ));
    if let Some(instance) = read_instance(&paths) {
        out.push_str(&format!("  Environment: {}\n", instance.env));
    }
    if healthy {
        let snapshot = client
            .get(format!(
                "http://127.0.0.1:{}/v1/router/top/snapshot",
                PORTS.metrics
            ))
            .send()
            .await
            .ok();
        if let Some(response) = snapshot {
            if let Ok(body) = response.json::<Value>().await {
                if let Some(totals) = body.get("totals") {
                    out.push_str(&format!("  Totals: {totals}\n"));
                }
            }
        }
    }
    match validate(request) {
        Ok(report) | Err(report) => out.push_str(&report),
    }
    Ok(out)
}

fn validate(request: &AppRequest) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let source = match request.desktop_config.clone() {
        Some(path) => path,
        None => applied_desktop_config_path(&home).map_err(|error| {
            format!(
                "could not read Claude Desktop's applied third-party configuration \
                 ({error}). Import the file from `rayline claude-desktop` first.\n"
            )
        })?,
    };
    let bytes = fs::read(&source).map_err(|error| format!("{}: {error}\n", source.display()))?;
    let config: Value = serde_json::from_slice(&bytes)
        .map_err(|error| format!("{}: {error}\n", source.display()))?;
    let token = fs::read_to_string(AppPaths::new(&home).token_file)
        .ok()
        .map(|token| token.trim().to_owned())
        .filter(|token| is_gateway_token(token));
    let (ok, report) = render_findings(
        &source,
        &validate_desktop_config(&config, &gateway_url(PORTS), token.as_deref()),
    );
    if ok { Ok(report) } else { Err(report) }
}

fn stop() -> Result<String, String> {
    let paths = home_paths()?;
    let mut out = String::new();
    if bootout_service()? {
        out.push_str(&format!(
            "Unloaded the login service until next login ({SERVICE_LABEL}).\n"
        ));
    }
    let stopped = stop_instance(&paths).map_err(|error| error.to_string())?;
    out.push_str(&stopped);
    out.push('\n');
    Ok(out)
}

async fn install_service(request: &AppRequest) -> Result<String, String> {
    if !launchd_enabled() {
        return Err(format!(
            "the login service is only supported on macOS, and not with {NO_LAUNCHD_ENV} set"
        ));
    }
    let resolved = resolve(request)?;
    let paths = AppPaths::new(&resolved.home);
    // Make sure a key exists now; the service itself never prompts.
    crate::claude::ensure_router_key(
        &resolved.env,
        &resolved.home,
        request.auth_token.as_deref(),
        request.root_env_explicit,
    )
    .await
    .map_err(|error| error.to_string())?;
    let token = ensure_gateway_token(&paths).map_err(|error| error.to_string())?;
    // Resolve rld now, with this shell's PATH and RLD_BIN, and pin it in the
    // plist: launchd's PATH would not find a custom location.
    let rld_bin = desktop_rld_bin(&resolved.home)?;
    write_instance_files(&paths, &resolved.router_url, request.id_style, &token)
        .map_err(|error| error.to_string())?;
    // Hand the ports over from a detached instance to launchd.
    stop_instance(&paths).map_err(|error| error.to_string())?;
    bootout_service()?;
    let rayline_bin = std::env::current_exe().map_err(|error| error.to_string())?;
    let env_name = request.root_env_explicit.then_some(resolved.env.as_str());
    if let Some(dir) = paths.launch_agent.parent() {
        fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    }
    fs::write(
        &paths.launch_agent,
        launch_agent_plist(&rayline_bin, &rld_bin, env_name, request.id_style, &paths),
    )
    .map_err(|error| error.to_string())?;
    bootstrap_service(&paths.launch_agent)?;
    if !wait_guarded(&http_client(), &token, None).await {
        let _ = bootout_service();
        return Err(unguarded_error(&paths, &rld_bin));
    }
    Ok(format!(
        "Installed the login service {SERVICE_LABEL} ({}). The router now starts at login.\n",
        paths.launch_agent.display()
    ))
}

fn uninstall_service() -> Result<String, String> {
    let paths = home_paths()?;
    bootout_service()?;
    match fs::remove_file(&paths.launch_agent) {
        Ok(()) => Ok(format!(
            "Removed the login service ({}).\n",
            paths.launch_agent.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Ok("The login service is not installed.\n".to_owned())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn uninstall() -> Result<String, String> {
    let paths = home_paths()?;
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let restored = desktop::restore(&home).map_err(|error| error.to_string())?;
    let mut out = if restored {
        "Restored your previous Claude Desktop profile.\n".to_owned()
    } else {
        String::new()
    };
    out.push_str(&uninstall_service()?);
    out.push_str(&stop()?);
    match fs::remove_dir_all(&paths.dir) {
        Ok(()) => out.push_str(&format!("Removed {}.\n", paths.dir.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    out.push_str(
        "The saved Rayline ARC profile remains in Desktop. To remove the gateway \
         configuration, open Developer > Configure Third-Party Inference... in Desktop.\n\n",
    );
    out.push_str(off_instructions());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "rayline-claude-app-test-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        home
    }

    const TEST_TOKEN: &str = "rlt-0000000000000000000000000000000000000000000000000000000000000000";

    #[cfg(unix)]
    #[test]
    fn gateway_token_is_random_private_and_stable() {
        let home = temp_home();
        let paths = AppPaths::new(&home);
        let first = ensure_gateway_token(&paths).expect("token");
        assert!(is_gateway_token(&first), "{first}");
        assert_eq!(ensure_gateway_token(&paths).expect("token again"), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.token_file)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_ne!(generate_gateway_token().expect("other"), first);
        // A corrupted file is replaced, never trusted.
        fs::write(&paths.token_file, "rayline-local-placeholder").expect("corrupt");
        let replaced = ensure_gateway_token(&paths).expect("replaced");
        assert!(is_gateway_token(&replaced));
        assert_ne!(replaced, first);
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(not(unix))]
    #[test]
    fn gateway_token_is_unsupported_off_unix() {
        let home = temp_home();
        let paths = AppPaths::new(&home);
        assert!(ensure_gateway_token(&paths).is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn write_private_narrows_an_existing_file_before_writing() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        let path = home.join("secret");
        fs::write(&path, "old").expect("seed");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&path, b"new").expect("write");
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(fs::read_to_string(&path).expect("read"), "new");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn instance_digest_tracks_key_token_config_and_rld() {
        let router = router_config_json("https://api.rayline.ai", IdStyle::Bare);
        let base = instance_digest(&router, "rlk-a", TEST_TOKEN, Path::new("/bin/rld"));
        assert_eq!(
            base,
            instance_digest(&router, "rlk-a", TEST_TOKEN, Path::new("/bin/rld"))
        );
        let prefixed = router_config_json("https://api.rayline.ai", IdStyle::Prefixed);
        for other in [
            instance_digest(&router, "rlk-b", TEST_TOKEN, Path::new("/bin/rld")),
            instance_digest(&router, "rlk-a", "rlt-other", Path::new("/bin/rld")),
            instance_digest(&router, "rlk-a", TEST_TOKEN, Path::new("/opt/rld")),
            instance_digest(&prefixed, "rlk-a", TEST_TOKEN, Path::new("/bin/rld")),
        ] {
            assert_ne!(base, other);
        }
        assert!(!base.contains("rlk-a"));
    }

    #[test]
    fn instance_digest_changes_when_rld_is_replaced_in_place() {
        let home = temp_home();
        let rld = home.join("rld");
        fs::write(&rld, b"old build").expect("write rld");
        let router = router_config_json("https://api.rayline.ai", IdStyle::Bare);
        let before = instance_digest(&router, "rlk-a", TEST_TOKEN, &rld);
        assert_eq!(before, instance_digest(&router, "rlk-a", TEST_TOKEN, &rld));
        // An update that keeps the path (a new build renamed over it).
        let staged = home.join("rld.new");
        fs::write(&staged, b"new build, longer").expect("write new rld");
        fs::rename(&staged, &rld).expect("replace rld");
        assert_ne!(before, instance_digest(&router, "rlk-a", TEST_TOKEN, &rld));
        let _ = fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn gateway_token_is_replaced_unless_private_and_ours() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        let paths = AppPaths::new(&home);
        let first = ensure_gateway_token(&paths).expect("token");
        assert_eq!(ensure_gateway_token(&paths).expect("reuse"), first);
        let dir_mode = fs::metadata(&paths.dir).expect("dir").permissions().mode();
        assert_eq!(dir_mode & 0o777, 0o700);
        // A token file others could have written is not trusted.
        let planted = format!("{GATEWAY_TOKEN_PREFIX}{}", "a".repeat(64));
        fs::write(&paths.token_file, format!("{planted}\n")).expect("plant");
        fs::set_permissions(&paths.token_file, fs::Permissions::from_mode(0o664)).expect("loosen");
        let replaced = ensure_gateway_token(&paths).expect("replace");
        assert_ne!(replaced, planted);
        let mode = fs::metadata(&paths.token_file)
            .expect("file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // A symlink at the token path is replaced, never read or written through.
        let target = home.join("victim");
        fs::write(&target, format!("{planted}\n")).expect("victim");
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).expect("narrow");
        fs::remove_file(&paths.token_file).expect("unlink");
        std::os::unix::fs::symlink(&target, &paths.token_file).expect("symlink");
        let fresh = ensure_gateway_token(&paths).expect("replace symlink");
        assert_ne!(fresh, planted);
        assert_eq!(
            fs::read_to_string(&target).expect("victim intact"),
            format!("{planted}\n")
        );
        assert!(
            !fs::symlink_metadata(&paths.token_file)
                .expect("token")
                .file_type()
                .is_symlink()
        );
        let _ = fs::remove_dir_all(home);
    }

    #[cfg(unix)]
    #[test]
    fn instance_file_round_trips_privately() {
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        let paths = AppPaths::new(&home);
        assert_eq!(read_instance(&paths), None);
        let instance = Instance {
            env: "dev".to_owned(),
            digest: "abc".to_owned(),
        };
        write_instance(&paths, &instance).expect("write");
        assert_eq!(read_instance(&paths), Some(instance));
        let mode = fs::metadata(&paths.instance_file)
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn import_file_is_private_and_carries_the_token() {
        let home = temp_home();
        let paths = AppPaths::new(&home);
        let token = ensure_gateway_token(&paths).expect("token");
        write_instance_files(&paths, "https://api.rayline.ai", IdStyle::Bare, &token)
            .expect("files");
        let import: Value =
            serde_json::from_slice(&fs::read(&paths.desktop_import).expect("read")).expect("json");
        assert_eq!(import["inferenceGatewayApiKey"], json!(token));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&paths.desktop_import)
                .expect("meta")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        // The router config references the key by env name and holds no token.
        let router = fs::read_to_string(&paths.router_config).expect("router");
        assert!(!router.contains(&token));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn validation_flags_a_stale_token_without_echoing_it() {
        let config = desktop_import_json(&gateway_url(PORTS), TEST_TOKEN);
        let other = "rlt-1111111111111111111111111111111111111111111111111111111111111111";
        let findings = validate_desktop_config(&config, &gateway_url(PORTS), Some(other));
        let key = findings
            .iter()
            .find(|finding| finding.message.contains("inferenceGatewayApiKey"))
            .expect("key finding");
        assert!(!key.ok);
        assert!(
            findings
                .iter()
                .all(|f| !f.message.contains(TEST_TOKEN) && !f.message.contains(other))
        );
    }

    #[test]
    fn rld_command_carries_the_inbound_token_env() {
        let paths = AppPaths::new(Path::new("/home/u"));
        let command = rld_command(Path::new("/bin/rld"), &paths, "rlk-k", TEST_TOKEN);
        let token_env = command
            .get_envs()
            .find(|(name, _)| *name == rayline_local_router::inbound_guard::INBOUND_TOKEN_ENV)
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(token_env.as_deref(), Some(TEST_TOKEN));
    }

    #[test]
    fn router_config_golden() {
        let expected = json!({
            "replace_defaults": true,
            "endpoints": [{
                "id": "rayline-arc",
                "protocol": "anthropic_messages",
                "base_url": "https://api.rayline.ai",
                "api_key_env": "RAYLINE_ROUTER_API_KEY",
                "models": ["arc-sage", "arc-swift"],
                "headers": { "x-rayline-client": "claude-desktop" }
            }],
            "routes": {
                "main": { "endpoint": "rayline-arc", "model": "arc-sage" },
                "subagent": { "endpoint": "rayline-arc", "model": "arc-sage" },
                "default": { "endpoint": "rayline-arc", "model": "arc-sage" },
                "model_routes": {
                    "claude-arc-sage": { "endpoint": "rayline-arc", "model": "arc-sage" },
                    "claude-arc-swift": { "endpoint": "rayline-arc", "model": "arc-swift" }
                },
                "subagents": {}
            }
        });
        assert_eq!(
            router_config_json("https://api.rayline.ai", IdStyle::Bare),
            expected
        );
    }

    #[test]
    fn router_config_parses_as_local_router_config_with_explicit_aliases() {
        let value = router_config_json("https://api-dev.rayline.ai", IdStyle::Bare);
        let config: rayline_local_router::RouterConfig =
            serde_json::from_value(value).expect("router config parses");
        for (id, tier) in [
            ("claude-arc-sage", "arc-sage"),
            ("claude-arc-swift", "arc-swift"),
        ] {
            let alias = config
                .routes
                .model_routes
                .get(id)
                .expect("explicit alias route");
            assert_eq!(alias.endpoint, ENDPOINT_ID);
            assert_eq!(alias.model, tier);
        }
        assert_eq!(config.routes.model_routes.len(), 2);
        let endpoint = &config.endpoints[0];
        assert_eq!(endpoint.api_key_env.as_deref(), Some(ROUTER_KEY_ENV));
    }

    #[test]
    fn prefixed_fallback_routes_the_same_desktop_ids_to_prefixed_tiers() {
        let value = router_config_json("https://api-dev.rayline.ai", IdStyle::Prefixed);
        let routes = &value["routes"]["model_routes"];
        assert_eq!(routes["claude-arc-sage"]["model"], "rayline/arc-sage");
        assert_eq!(routes["claude-arc-swift"]["model"], "rayline/arc-swift");
        assert_eq!(value["routes"]["main"]["model"], "rayline/arc-sage");
        assert_eq!(
            value["endpoints"][0]["models"],
            json!(["rayline/arc-sage", "rayline/arc-swift"])
        );
        // The Desktop side is the same either way.
        let bare = router_config_json("https://api-dev.rayline.ai", IdStyle::Bare);
        assert_eq!(
            bare["routes"]["model_routes"]
                .as_object()
                .map(|routes| routes.keys().cloned().collect::<Vec<_>>()),
            routes
                .as_object()
                .map(|routes| routes.keys().cloned().collect::<Vec<_>>())
        );
    }

    #[test]
    fn router_config_never_contains_a_key() {
        let text = router_config_json("https://api.rayline.ai", IdStyle::Bare).to_string();
        assert!(!text.contains("rlk-"));
        let import = desktop_import_json(&gateway_url(PORTS), TEST_TOKEN).to_string();
        assert!(!import.contains("rlk-"));
    }

    #[test]
    fn desktop_import_golden() {
        let expected = json!({
            "inferenceProvider": "gateway",
            "inferenceGatewayBaseUrl": "http://127.0.0.1:20831",
            "inferenceCredentialKind": "static",
            "inferenceGatewayApiKey": TEST_TOKEN,
            "modelDiscoveryEnabled": false,
            "inferenceModels": [
                { "name": "claude-arc-sage", "labelOverride": "arc-sage" },
                { "name": "claude-arc-swift", "labelOverride": "arc-swift" }
            ],
            "coworkTabEnabled": false
        });
        assert_eq!(
            desktop_import_json(&gateway_url(PORTS), TEST_TOKEN),
            expected
        );
    }

    #[test]
    fn generated_import_validates() {
        let findings = validate_desktop_config(
            &desktop_import_json(&gateway_url(PORTS), TEST_TOKEN),
            &gateway_url(PORTS),
            Some(TEST_TOKEN),
        );
        assert!(findings.iter().all(|finding| finding.ok), "{findings:?}");
    }

    #[test]
    fn validation_accepts_string_encoded_values_and_flags_mismatches() {
        let config = json!({
            "inferenceProvider": "gateway",
            "inferenceGatewayBaseUrl": "http://127.0.0.1:20831/",
            "inferenceGatewayApiKey": TEST_TOKEN,
            "modelDiscoveryEnabled": "false",
            "inferenceModels": "[\"claude-arc-sage\", \"claude-arc-swift\"]"
        });
        let findings = validate_desktop_config(&config, &gateway_url(PORTS), Some(TEST_TOKEN));
        assert!(findings.iter().all(|finding| finding.ok), "{findings:?}");

        let wrong = json!({
            "inferenceProvider": "gateway",
            "inferenceGatewayBaseUrl": "http://127.0.0.1:21811",
            "inferenceGatewayApiKey": "rlk-secret-value",
            "inferenceModels": [{ "name": "claude-sonnet-5" }]
        });
        let findings = validate_desktop_config(&wrong, &gateway_url(PORTS), Some(TEST_TOKEN));
        let failed: Vec<_> = findings.iter().filter(|finding| !finding.ok).collect();
        assert_eq!(failed.len(), 4, "{findings:?}");
        // The gateway key value is never echoed.
        assert!(
            findings
                .iter()
                .all(|finding| !finding.message.contains("rlk-secret-value"))
        );
    }

    #[test]
    fn applied_config_path_follows_meta_and_rejects_traversal() {
        let home = temp_home();
        let library = home.join("Library/Application Support/Claude-3p/configLibrary");
        fs::create_dir_all(&library).expect("library");
        fs::write(library.join("_meta.json"), r#"{"appliedId":"abc-123"}"#).expect("meta");
        assert_eq!(
            applied_desktop_config_path(&home).expect("path"),
            library.join("abc-123.json")
        );
        fs::write(library.join("_meta.json"), r#"{"appliedId":"../../x"}"#).expect("meta");
        assert!(applied_desktop_config_path(&home).is_err());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn ports_are_disjoint_from_every_other_rayline_router() {
        let shared = [
            20808,
            20809,
            20810,
            crate::router::DEFAULT_LOCAL_ROUTER_PORT,
            rayline_metrics::DEFAULT_METRICS_PORT,
            20814,
        ];
        for port in PORTS.all() {
            assert!(!shared.contains(&port), "port {port} collides");
        }
        let mut unique = PORTS.all().to_vec();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn state_lives_outside_the_shared_router_state() {
        let home = Path::new("/home/u");
        let paths = AppPaths::new(home);
        let shared = home.join(crate::ROUTER_STATE_DIR);
        for path in [
            &paths.dir,
            &paths.router_config,
            &paths.pid_file,
            &paths.data_dir,
        ] {
            assert!(!path.starts_with(&shared), "{} is shared", path.display());
        }
        assert!(
            !paths
                .dir
                .starts_with(home.join(".config").join(crate::CONFIG_DIR))
        );
        assert!(!paths.dir.starts_with(home.join(".claude")));
    }

    #[test]
    fn rld_args_name_the_instance_ports_and_config() {
        let paths = AppPaths::new(Path::new("/home/u"));
        let args: Vec<String> = rld_args(&paths, PORTS)
            .into_iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        let value_of = |flag: &str| {
            let index = args.iter().position(|arg| arg == flag).expect(flag);
            args[index + 1].clone()
        };
        assert_eq!(args[0], "serve");
        assert!(args.contains(&"--no-local-model".to_owned()));
        assert_eq!(value_of("--local-router-port"), "20831");
        assert_eq!(value_of("--adapter-port"), "20828");
        assert_eq!(value_of("--injector-port"), "20829");
        assert_eq!(value_of("--metrics-port"), "20833");
        assert_eq!(value_of("--decision-plane"), "local");
        assert_eq!(
            value_of("--router-config-path"),
            paths.router_config.to_string_lossy()
        );
        assert!(!args.iter().any(|arg| arg.contains("proxy")));
    }

    #[test]
    fn env_is_scrubbed_of_rayline_anthropic_and_claude_vars() {
        let env = scrubbed_env(
            [
                ("PATH", "/bin"),
                ("RAYLINE_PROXY_PORT", "1"),
                ("RAYLINE_ROUTER_API_KEY", "rlk-old"),
                ("ANTHROPIC_BASE_URL", "http://x"),
                ("CLAUDE_CONFIG_DIR", "/c"),
                ("OPENAI_API_KEY", "sk-x"),
                ("OPENROUTER_API_KEY", "sk-or-x"),
                ("HOME", "/home/u"),
            ]
            .map(|(name, value)| (OsString::from(name), OsString::from(value))),
        );
        let names: Vec<_> = env
            .iter()
            .map(|(name, _)| name.to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["PATH", "HOME"]);
    }

    #[test]
    fn stop_target_only_claims_our_command_line() {
        let paths = AppPaths::new(Path::new("/home/u"));
        let ours = format!(
            "/home/u/.rayline/bin/rld serve --router-config-path {}",
            paths.router_config.display()
        );
        assert_eq!(
            stop_target(Some(42), &paths, |_| true, |_| Some(ours.clone())),
            StopTarget::Ours(42)
        );
        assert_eq!(
            stop_target(
                Some(42),
                &paths,
                |_| true,
                |_| Some("/usr/bin/python3 start.py".to_owned())
            ),
            StopTarget::Foreign(42)
        );
        assert_eq!(
            stop_target(Some(42), &paths, |_| true, |_| None),
            StopTarget::Foreign(42)
        );
        // Mentioning the config path is not enough: it must be `rld serve`
        // with exactly this `--router-config-path`.
        let config = paths.router_config.display().to_string();
        for line in [
            format!("vim {config}"),
            format!("tail -f {config}"),
            format!("/home/u/.rayline/bin/rld serve --router-config-path {config}.bak"),
            format!("/home/u/.rayline/bin/rld status --router-config-path {config}"),
            format!("/usr/bin/not-rld serve --router-config-path {config}"),
            // `rld serve` as another program's arguments.
            format!(
                "python3 diagnostics.py --sample /opt/rayline/rld serve --router-config-path {config} --inspect"
            ),
            format!("/usr/bin/python3 /opt/rayline/rld serve --router-config-path {config}"),
            format!("rld serve --router-config-path {config}"),
        ] {
            assert_eq!(
                stop_target(Some(42), &paths, |_| true, |_| Some(line.clone())),
                StopTarget::Foreign(42),
                "{line}"
            );
        }
        let with_more_args = format!(
            "/home/u/.rayline/bin/rld serve --no-local-model --router-config-path {config} --data-dir /d"
        );
        assert_eq!(
            stop_target(Some(42), &paths, |_| true, |_| Some(with_more_args.clone())),
            StopTarget::Ours(42)
        );
        assert_eq!(
            stop_target(Some(42), &paths, |_| false, |_| Some(ours.clone())),
            StopTarget::NotRunning
        );
        assert_eq!(
            stop_target(Some(1), &paths, |_| true, |_| Some(ours.clone())),
            StopTarget::NotRunning
        );
        assert_eq!(
            stop_target(None, &paths, |_| true, |_| Some(ours.clone())),
            StopTarget::NotRunning
        );
    }

    #[cfg(unix)]
    #[test]
    fn stop_never_kills_a_foreign_process() {
        let home = temp_home();
        let paths = AppPaths::new(&home);
        fs::create_dir_all(&paths.dir).expect("state dir");
        let mut foreign = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = foreign.id() as i32;
        fs::write(&paths.pid_file, format!("{pid}\n")).expect("pid file");

        let result = stop_instance(&paths);

        assert!(result.is_err(), "stop must refuse a foreign pid");
        assert!(process_alive(pid), "foreign process was signalled");
        assert!(paths.pid_file.exists(), "pid file kept for inspection");
        let _ = foreign.kill();
        let _ = foreign.wait();
        let _ = fs::remove_dir_all(&home);
    }

    #[cfg(unix)]
    #[test]
    fn stop_terminates_our_own_process() {
        let home = temp_home();
        let paths = AppPaths::new(&home);
        fs::create_dir_all(&paths.dir).expect("state dir");
        // A real child to stop. Which pid is ours is decided from its `ps`
        // command line (covered by `stop_target_only_claims_our_command_line`);
        // here that lookup answers like the real router's, so the test does
        // not depend on how each platform's `ps` prints an exec'd argv[0].
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn stand-in");
        let pid = child.id() as i32;
        // Owns the child for the whole test: dropping it (also on panic) kills
        // and reaps it through `Child`, which never signals a reaped pid.
        struct StandIn(std::sync::Arc<std::sync::Mutex<std::process::Child>>);
        impl Drop for StandIn {
            fn drop(&mut self) {
                let mut child = self.0.lock().unwrap_or_else(|error| error.into_inner());
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let ours = StandIn(std::sync::Arc::new(std::sync::Mutex::new(child)));
        fs::write(&paths.pid_file, format!("{pid}\n")).expect("pid file");
        let ours_line = format!(
            "{} serve --router-config-path {}",
            home.join("rld").display(),
            paths.router_config.display()
        );

        // Reap while stop waits for the pid to go: an unreaped child stays a
        // zombie that still counts as alive.
        let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reaper = {
            let child = std::sync::Arc::clone(&ours.0);
            let done = std::sync::Arc::clone(&done);
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let exited = child
                        .lock()
                        .map(|mut child| matches!(child.try_wait(), Ok(Some(_))))
                        .unwrap_or(true);
                    if exited {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            })
        };
        let result = stop_instance_with(&paths, |candidate| {
            (candidate == pid).then(|| ours_line.clone())
        });
        done.store(true, Ordering::Relaxed);
        let _ = reaper.join();
        // Whether stop itself ended the stand-in, read before the guard's
        // cleanup kills it.
        let exited = ours
            .0
            .lock()
            .map(|mut child| matches!(child.try_wait(), Ok(Some(_))))
            .unwrap_or(false);
        drop(ours);
        let result = result.expect("stop ours");
        assert!(exited, "stop left the stand-in running");

        assert!(result.contains(&pid.to_string()), "{result}");
        assert!(!paths.pid_file.exists());
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn launch_agent_plist_runs_serve_and_holds_no_secret() {
        let paths = AppPaths::new(Path::new("/home/u"));
        let plist = launch_agent_plist(
            Path::new("/home/u/.rayline/bin/rayline"),
            Path::new("/opt/rayline/rld"),
            Some("dev"),
            IdStyle::Bare,
            &paths,
        );
        assert!(plist.contains("<string>ai.rayline.claude-desktop</string>"));
        assert!(plist.contains(
            "<string>/home/u/.rayline/bin/rayline</string>\n\t\t<string>--env</string>\n\t\t<string>dev</string>\n\t\t<string>claude-desktop</string>\n\t\t<string>serve</string>"
        ));
        assert!(plist.contains(
            "<key>EnvironmentVariables</key>\n\t<dict>\n\t\t<key>RLD_BIN</key>\n\t\t<string>/opt/rayline/rld</string>"
        ));
        assert!(!plist.contains("rlk-"));
        assert!(!plist.contains(ROUTER_KEY_ENV));
        assert!(!plist.contains("--prefixed-ids"));
        let prod = launch_agent_plist(
            Path::new("/r"),
            Path::new("/rld"),
            None,
            IdStyle::Prefixed,
            &paths,
        );
        assert!(!prod.contains("--env"));
        assert!(prod.contains("<string>serve</string>\n\t\t<string>--prefixed-ids</string>"));
    }

    #[test]
    fn actions_parse() {
        for (text, action) in [
            ("start", AppAction::Start),
            ("serve", AppAction::Serve),
            ("status", AppAction::Status),
            ("validate", AppAction::Validate),
            ("stop", AppAction::Stop),
            ("install-service", AppAction::InstallService),
            ("uninstall-service", AppAction::UninstallService),
            ("off", AppAction::Off),
            ("uninstall", AppAction::Uninstall),
        ] {
            assert_eq!(AppAction::parse(text), Some(action));
        }
        assert_eq!(AppAction::parse("bogus"), None);
    }
}
