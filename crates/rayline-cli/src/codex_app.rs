//! `rayline codex app` — launch the Codex **desktop app** routed through Rayline.
//!
//! Mirrors `rayline codex` (same `--config`/`--auth`/`--model` flags, same
//! router-daemon path) but targets the desktop app instead of the CLI. The
//! desktop app-server differs from the CLI in two ways that shape this module:
//!
//! 1. It **ignores `-c` overrides** — it only reads persistent config from its
//!    `CODEX_HOME`. So we materialize the Rayline provider into a `config.toml`
//!    inside an isolated `CODEX_HOME` (`~/.rayline/codex-app-home`) rather than
//!    passing `-c` flags.
//! 2. It is **single-instance per machine**, locked to the `CODEX_HOME` its
//!    app-server first spawned with. A second `codex app` launch just focuses
//!    the existing window. So if an app-server is already running on a different
//!    home, we must prompt the user to quit + relaunch under the Rayline home.

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use toml_edit::{DocumentMut, Item, Table, TomlError, Value};

use crate::codex::{
    CodexAuthMode, EffectiveCodexAuthMode, default_rayline_base_url, rayline_provider_config_toml,
};

const CODEX_APP_HOME_DIR: &str = "codex-app-home";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppRunRequest {
    pub model: Option<String>,
    pub config_path: Option<PathBuf>,
    pub auth_mode: CodexAuthMode,
    /// Extra args passed through to `codex app` (e.g. a workspace path).
    pub codex_args: Vec<OsString>,
    pub root_env_explicit: bool,
    /// Hosted environment override (`--env`) for router-key provisioning.
    pub env_name: Option<String>,
    /// Account bearer (`--auth-token`) for minting/reading the `rlk-` key.
    pub auth_token: Option<String>,
}

pub async fn run(mut request: AppRunRequest) -> ExitCode {
    // Default (no `--config`, auto auth) to Rc-Rc — route everything to the hosted
    // cloud RCR, mirroring `rayline claude` and `rayline codex`. Shared resolver
    // so the default is defined once.
    match crate::codex::resolve_codex_config_path_from_home(
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
    // 1. Start/ensure the Rayline router — identical path to `rayline codex`.
    let router_api_key_override = crate::codex::resolve_cloud_router_key(
        request.config_path.as_deref(),
        request.env_name.as_deref(),
        request.auth_token.as_deref(),
        request.root_env_explicit,
    )
    .await;
    let start_request = crate::router::RouterStartCliRequest {
        api_mode: crate::router::ROUTER_API_MODE_CODEX.to_owned(),
        proxy_routing_mode: crate::router::PROXY_ROUTING_MODE_ALL.to_owned(),
        config_path: request.config_path.clone(),
        codex_auth_mode: request.auth_mode,
        root_env_explicit: request.root_env_explicit,
        router_api_key_override,
    };
    if let Err(error) = crate::router::start_from_cli(&start_request).await {
        eprintln!("Error: failed to start Rayline Codex router: {error}");
        return ExitCode::from(1);
    }
    eprintln!(
        "Rayline Codex router ready at http://127.0.0.1:{}/v1",
        crate::router::DEFAULT_LOCAL_ROUTER_PORT
    );

    // 2. Compute the config we WOULD write — no filesystem changes yet, so a
    //    declined restart below leaves everything untouched.
    let home = match isolated_home_path() {
        Ok(home) => home,
        Err(error) => {
            eprintln!("Error: failed to resolve Codex app home: {error}");
            return ExitCode::from(1);
        }
    };
    let subscription_auth = request
        .auth_mode
        .effective_for_run(request.config_path.as_ref())
        == EffectiveCodexAuthMode::Subscription;
    // Present the Rayline provider to the desktop app as OpenAI-authed whenever we
    // can. That is what makes Codex use the provider's /models and show the clean
    // "Rayline Auto" entry in the picker instead of "Custom" + its built-in GPT
    // presets. This is presentation only — it does NOT change where the prompt
    // goes: the router config decides that. Under the Rc-Rc default the request
    // routes to the hosted RCR with your `rlk-` key (the endpoint is not
    // `client_bearer`, so the ChatGPT token Codex attaches is stripped and
    // replaced with the router key upstream). We enable it for subscription, and
    // for the Rc-Rc default only when a ChatGPT login already exists to attach —
    // `--auth none` (local) stays plain, and a user with no ChatGPT login keeps
    // the working-but-plain picker rather than being forced into a login.
    let openai_presentation =
        subscription_auth || (request.auth_mode == CodexAuthMode::Auto && chatgpt_auth_available());
    let config = generate_config(&request, openai_presentation);

    // 3. Reconcile the single-instance app-server. The comparison basis is the
    //    running app-server's OWN loaded config (its CODEX_HOME/config.toml),
    //    not our isolated file — that's what it actually loaded at startup.
    match reconcile_running_app(&home, &config) {
        ReconcileOutcome::AlreadyCurrent => {
            // Config already matches; don't rewrite. Still launch so `codex app`
            // focuses the window / opens the requested workspace.
            eprintln!("The Codex app is already using Rayline. Bringing it to the front.");
        }
        ReconcileOutcome::Cancelled => {
            eprintln!("Leaving the Codex app as it is.");
            return ExitCode::SUCCESS;
        }
        ReconcileOutcome::Proceed => {
            // No conflicting app, or the user agreed to restart: write config now.
            if let Err(error) = write_isolated_home(&home, &config, openai_presentation) {
                eprintln!("Error: failed to prepare Codex app home: {error}");
                return ExitCode::from(1);
            }
        }
    }

    // 4. Launch (or focus) the Codex app on the isolated home.
    let mut command = crate::codex::codex_command();
    command.env("CODEX_HOME", &home).arg("app");
    command.args(&request.codex_args);
    match command.status() {
        // A missing code means the child was killed by a signal — treat it as
        // failure (matching the Codex CLI path), not silent success.
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to launch codex app: {error}");
            ExitCode::from(127)
        }
    }
}

/// Whether a ChatGPT `auth.json` exists in the user's real Codex home. Gates the
/// OpenAI-authed presentation for the Rc-Rc default: with a login we can attach a
/// token (so Codex shows the clean picker), without one we must not force a login.
fn chatgpt_auth_available() -> bool {
    user_codex_home()
        .map(|home| home.join("auth.json").exists())
        .unwrap_or(false)
}

/// Resolve the user's real Codex home (`$CODEX_HOME` or `~/.codex`), used as the
/// source of `auth.json`.
fn user_codex_home() -> io::Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))
}

/// The stable isolated home Rayline points the desktop app at.
fn isolated_home_path() -> io::Result<PathBuf> {
    let base = dirs::home_dir()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))?;
    Ok(base.join(crate::DOT_CONFIG_DIR).join(CODEX_APP_HOME_DIR))
}

/// Root keys the generated Rayline snippet owns. Together with the
/// `[model_providers.rayline]` table these are the only parts of the isolated
/// `config.toml` Rayline ever writes; everything else in the file belongs to the
/// Codex desktop app or the user.
const RAYLINE_ROOT_KEYS: [&str; 3] = ["model", "model_provider", "forced_login_method"];
const MODEL_PROVIDERS_TABLE: &str = "model_providers";
const RAYLINE_PROVIDER: &str = "rayline";

/// True when the existing `config.toml` does not already carry the Rayline
/// settings of the `generated` snippet (a changed `--model`/`--auth`/base-url,
/// a missing table, …). Compared on content, not text: the desktop app
/// re-serialises this file (line endings, trailing newline, spacing, key order)
/// and none of that is a setting. An unparsable file also differs — rewriting
/// is the only way to repair it.
fn generated_config_differs(existing: &str, generated: &str) -> bool {
    let (Ok(doc), Ok(generated)) = (
        parse_isolated_config(existing),
        generated.parse::<DocumentMut>(),
    ) else {
        return true;
    };
    RAYLINE_ROOT_KEYS
        .iter()
        .any(|key| !root_key_current(&doc, &generated, key))
        || !provider_table_current(&doc, &generated)
}

/// Render the Rayline `config.toml` snippet for the desktop app from the run
/// flags. Pure — no filesystem effects, so it can be compared against a running
/// app-server's config before deciding whether to write/restart.
fn generate_config(request: &AppRunRequest, subscription_auth: bool) -> String {
    let model = request
        .model
        .as_deref()
        .unwrap_or(crate::codex::CODEX_DEFAULT_SENTINEL_MODEL);
    let base_url = default_rayline_base_url();
    rayline_provider_config_toml(model, &base_url, subscription_auth)
}

/// Write the isolated `CODEX_HOME`: `config.toml` (the Rayline settings merged
/// into whatever the file already holds) plus an `auth.json` link. Called only
/// after we're committed to launching, so a declined restart never mutates the
/// home.
///
/// `auth.json` is linked only when auth resolves to subscription — a `--auth
/// none` / local-only run must not run the desktop app with the user's Codex
/// credentials, so any existing link is removed instead.
fn write_isolated_home(home: &Path, config: &str, subscription_auth: bool) -> io::Result<()> {
    fs::create_dir_all(home)?;
    let config_path = home.join("config.toml");
    // The document to merge into:
    //  - the isolated config.toml when it exists (the Codex desktop app persists
    //    its own settings there: [desktop], [projects], …),
    //  - on first creation, the tables of the user's main Codex config.toml,
    //  - otherwise nothing.
    let existing = match fs::read_to_string(&config_path) {
        Ok(read_value) => read_value,
        Err(_) => user_codex_home()
            .ok()
            .and_then(|h| fs::read_to_string(h.join("config.toml")).ok())
            .map(|seed| seed_tables(&seed))
            .unwrap_or_default(),
    };
    let merged = apply_rayline_settings(&existing, config).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{}: {error}", config_path.display()),
        )
    })?;
    fs::write(&config_path, merged)?;
    link_auth_json(home, subscription_auth)
}

/// Merge the Rayline-owned settings of the `generated` snippet into `existing`
/// in place: each owned root key is set (or removed when the snippet omits it)
/// and `[model_providers.rayline]` is replaced wherever it sits in the file.
/// Everything else — the app's tables, root keys, comments and ordering — is
/// preserved byte-for-byte; only items whose content differs are touched.
///
/// The desktop app rewrites this file freely (it reorders tables and adds its
/// own), so the Rayline settings cannot be assumed to be a prefix or to be
/// anywhere in particular.
fn apply_rayline_settings(existing: &str, generated: &str) -> Result<String, TomlError> {
    let mut doc = parse_isolated_config(existing)?;
    let generated: DocumentMut = generated.parse()?;
    for key in RAYLINE_ROOT_KEYS {
        if root_key_current(&doc, &generated, key) {
            continue;
        }
        match generated.get(key).and_then(Item::as_str) {
            Some(wanted) => doc[key] = toml_edit::value(wanted),
            None => {
                doc.remove(key);
            }
        }
    }
    if !provider_table_current(&doc, &generated) {
        let mut table = rayline_provider_table(&generated)
            .expect("generated snippet always defines [model_providers.rayline]")
            .clone();
        // An existing table keeps its place; a new one goes ahead of every parsed
        // table (positions start at 1) so a first-run file reads Rayline-first.
        let position = rayline_provider_table(&doc).and_then(Table::position);
        table.set_position(position.or(Some(0)));
        let providers = doc
            .entry(MODEL_PROVIDERS_TABLE)
            .or_insert_with(toml_edit::table);
        if let Some(providers) = providers.as_table_mut() {
            // Render as `[model_providers.rayline]`, not an empty `[model_providers]`.
            providers.set_implicit(true);
        }
        providers[RAYLINE_PROVIDER] = Item::Table(table);
    }
    Ok(doc.to_string())
}

/// Whether `doc` already holds the generated snippet's value for an owned root
/// key — including "absent in both" (e.g. `forced_login_method` on `--auth none`).
fn root_key_current(doc: &DocumentMut, generated: &DocumentMut, key: &str) -> bool {
    doc.get(key).and_then(Item::as_str) == generated.get(key).and_then(Item::as_str)
}

/// Whether `doc`'s `[model_providers.rayline]` matches the generated one on content.
fn provider_table_current(doc: &DocumentMut, generated: &DocumentMut) -> bool {
    match (
        rayline_provider_table(doc),
        rayline_provider_table(generated),
    ) {
        (Some(current), Some(wanted)) => canonical(current) == canonical(wanted),
        _ => false,
    }
}

fn rayline_provider_table(doc: &DocumentMut) -> Option<&Table> {
    doc.get(MODEL_PROVIDERS_TABLE)
        .and_then(|providers| providers.get(RAYLINE_PROVIDER))
        .and_then(Item::as_table)
}

/// Parse an isolated `config.toml`. A file left with two
/// `[model_providers.rayline]` tables by an earlier Rayline release fails to
/// parse; drop every copy textually and retry, since the merge re-emits the
/// table anyway. Any other error is the caller's to surface.
fn parse_isolated_config(existing: &str) -> Result<DocumentMut, TomlError> {
    existing.parse().or_else(|error| {
        strip_rayline_provider_table(existing)
            .parse()
            .map_err(|_| error)
    })
}

/// A table's key/value pairs rendered with formatting reset and keys sorted, so
/// two tables compare equal on content even if the desktop app re-serialised
/// one of them with different spacing or key order.
fn canonical(table: &Table) -> String {
    let mut table = table.clone();
    table.sort_values();
    table.fmt();
    for (_, item) in table.iter_mut() {
        if let Some(inline) = item.as_value_mut().and_then(Value::as_inline_table_mut) {
            inline.sort_values();
            inline.fmt();
        }
    }
    table.to_string()
}

/// The tables of the user's main `config.toml`, used to seed the isolated
/// config on first creation (`[mcp_servers]`, `[projects]`, …). Root keys are
/// dropped: the ones we own are overridden anyway, and the rest (`profile`,
/// `notify`, …) can change routing or run commands and are not carried over
/// silently. An unparsable or empty seed contributes nothing.
fn seed_tables(seed: &str) -> String {
    let Ok(mut doc) = seed.parse::<DocumentMut>() else {
        return String::new();
    };
    doc.retain(|_, item| {
        item.is_array_of_tables() || item.as_table().is_some_and(|table| !table.is_dotted())
    });
    doc.to_string()
}

/// Remove every `[model_providers.rayline]` table (its header through the line
/// before the next table header / EOF) from `text`.
fn strip_rayline_provider_table(text: &str) -> String {
    let mut out = String::new();
    let mut skipping = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') {
            // A new table header ends any table we were skipping.
            skipping = trimmed.trim_end() == "[model_providers.rayline]";
        }
        if !skipping {
            out.push_str(line);
        }
    }
    out
}

/// Reconcile the isolated `<home>/auth.json` for the effective auth mode.
///
/// The isolated `auth.json` can be either a **symlink** we created (points at the
/// user's real one) or a **regular file** the desktop app wrote when the user
/// logged in inside the app. Those must be treated differently so we never delete
/// the user's only credentials:
///
///  - Subscription, source present → (re)link to the source. A prior symlink or
///    file is replaced; the source is the fresher truth.
///  - Subscription, source absent → keep a regular `auth.json` the app wrote
///    (the user's only credentials); only remove a *symlink* (it's stale, pointing
///    nowhere useful).
///  - Auth disabled (`--auth none` / local) → remove any isolated `auth.json`
///    (symlink or file) so the app doesn't run with the user's credentials.
fn link_auth_json(home: &Path, subscription_auth: bool) -> io::Result<()> {
    let link = home.join("auth.json");
    if !subscription_auth {
        return remove_if_present(&link);
    }
    let source = user_codex_home()?.join("auth.json");
    reconcile_auth_link(&source, &link)
}

/// Make `link` reflect `source` for the subscription path:
///  - source exists → replace `link` with a symlink to it;
///  - source absent → keep a regular-file `link` (app-written credentials), but
///    drop a dangling symlink.
fn reconcile_auth_link(source: &Path, link: &Path) -> io::Result<()> {
    let existing = fs::symlink_metadata(link).ok();
    let is_symlink = existing.as_ref().is_some_and(|meta| meta.is_symlink());

    if source.exists() {
        if existing.is_some() {
            fs::remove_file(link)?;
        }
        return symlink_file(source, link);
    }
    // No source: a symlink here is stale → remove it; a real file is the user's
    // desktop-app login → keep it.
    if is_symlink {
        fs::remove_file(link)?;
    }
    Ok(())
}

/// Remove `path` if it exists (file or symlink); no-op if absent.
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::remove_file(path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn symlink_file(source: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(source, link)
}

#[cfg(windows)]
fn symlink_file(source: &Path, link: &Path) -> io::Result<()> {
    // Windows symlinks need privilege; fall back to a copy if the link fails.
    match std::os::windows::fs::symlink_file(source, link) {
        Ok(()) => Ok(()),
        Err(_) => fs::copy(source, link).map(|_| ()),
    }
}

enum ReconcileOutcome {
    /// No conflicting app-server, or the user agreed to restart — safe to write
    /// config and launch.
    Proceed,
    /// An app-server is already running and already has our exact config — just
    /// focus it, write nothing.
    AlreadyCurrent,
    /// A conflicting app-server is running and the user declined to restart it.
    Cancelled,
}

/// Inspect the running Codex app-server (if any) and decide whether to launch.
///
/// A running single-instance app loaded its `config.toml` at startup, so new
/// settings only take effect on restart. The comparison basis is the app's OWN
/// loaded config — the `config.toml` in *its* `CODEX_HOME` (which the app wrote
/// to its home at launch), not our isolated file. Restart when the app is on a
/// different home, or on our home but its loaded config no longer matches what
/// we'd generate; otherwise a rerun with new `--model`/`--auth` would silently
/// keep the old config.
fn reconcile_running_app(home: &Path, config: &str) -> ReconcileOutcome {
    let Some(app) = find_running_app_server() else {
        return ReconcileOutcome::Proceed;
    };
    let same_home = app.codex_home.as_deref().map(Path::new) == Some(home);
    if same_home && running_app_config_matches(&app, config) {
        return ReconcileOutcome::AlreadyCurrent;
    }
    if same_home {
        eprintln!("The Codex app is running with an out-of-date Rayline configuration.");
    } else {
        eprintln!("The Codex app is already running with a different configuration.");
    }
    if !confirm_restart() {
        return ReconcileOutcome::Cancelled;
    }
    if let Err(error) = quit_running_app(&app) {
        eprintln!("Couldn't close the running Codex app: {error}");
        return ReconcileOutcome::Cancelled;
    }
    ReconcileOutcome::Proceed
}

/// Whether the running app-server's loaded config already carries our Rayline
/// settings. Reads the `config.toml` in the app's own
/// `CODEX_HOME` — what it actually loaded at startup. Unreadable ⇒ treat as a
/// mismatch (safer to restart than to leave it stale).
fn running_app_config_matches(app: &RunningApp, config: &str) -> bool {
    let Some(home) = app.codex_home.as_deref() else {
        return false;
    };
    match fs::read_to_string(Path::new(home).join("config.toml")) {
        Ok(existing) => !generated_config_differs(&existing, config),
        Err(_) => false,
    }
}

struct RunningApp {
    pid: u32,
    codex_home: Option<String>,
}

/// Find the Codex desktop app-server process and read its `CODEX_HOME`.
///
/// The app-server argv is `.../ChatGPT.app/Contents/Resources/codex app-server`
/// on macOS (`Codex.app` on installs predating the rename) and `codex
/// app-server` elsewhere — distinct from the GUI parent and the renderer/GPU
/// helpers. We key off the `app-server` token, so the bundle name is incidental.
fn find_running_app_server() -> Option<RunningApp> {
    let pid = pgrep_app_server()?;
    Some(RunningApp {
        pid,
        codex_home: read_codex_home_env(pid),
    })
}

/// Find the Codex desktop app-server pid.
///
/// We can't match a fixed `"codex app-server"` substring: Codex injects `-c`
/// flags between the binary and the subcommand (e.g. `codex -c features.x=1
/// app-server`). We also can't rely on `pgrep -l` for the argv — on Linux
/// (procps) `-l` prints only the process name even with `-f`, dropping the
/// `app-server` token. So `pgrep -f` gives us candidate pids, then we read each
/// one's full argv from a portable per-pid source and keep the app-server.
fn pgrep_app_server() -> Option<u32> {
    let output = Command::new("pgrep").arg("-f").arg("codex").output().ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .filter_map(|token| token.parse::<u32>().ok())
        .find(|pid| process_argv(*pid).is_some_and(|argv| is_app_server_argv(&argv)))
}

/// Read a process's full command line. Linux: `/proc/<pid>/cmdline` (NUL-joined
/// argv). macOS: `ps -p <pid> -o command=`. Mirrors `router::process_argv`.
fn process_argv(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let argv: Vec<u8> = raw
            .into_iter()
            .map(|byte| if byte == 0 { b' ' } else { byte })
            .collect();
        Some(String::from_utf8_lossy(&argv).trim().to_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = Command::new("ps")
            .args(["-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let argv = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (!argv.is_empty()).then_some(argv)
    }
}

/// True for the app-server process argv. The reliable signal is the
/// `app-server` subcommand token, which the CLI (`codex`, `codex resume`) and
/// the renderer/GPU helpers never carry. Robust to injected `-c` flags between
/// the binary and the subcommand.
fn is_app_server_argv(argv: &str) -> bool {
    argv.split_whitespace().any(|tok| tok == "app-server")
}

/// Read `CODEX_HOME` from a running process's environment.
///
/// Linux: `/proc/<pid>/environ`. macOS: `ps eww` (the `e` flag appends the
/// environment to the command line). Mirrors `claude_daemon::read_process_env`.
fn read_codex_home_env(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let raw = fs::read(format!("/proc/{pid}/environ")).ok()?;
        for entry in raw.split(|byte| *byte == 0) {
            let decoded = String::from_utf8_lossy(entry);
            if let Some(value) = decoded.strip_prefix("CODEX_HOME=") {
                return Some(value.to_owned());
            }
        }
        None
    }
    #[cfg(target_os = "macos")]
    {
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
        line.split_whitespace()
            .find_map(|token| token.strip_prefix("CODEX_HOME=").map(ToOwned::to_owned))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

/// Prompt `[y/N]` on the terminal. Defaults to "no" on non-interactive stdin or
/// any read error, so we never restart the user's app without explicit consent.
fn confirm_restart() -> bool {
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        eprintln!("Not an interactive terminal; leaving the running Codex app as-is.");
        return false;
    }
    eprint!("Would you like to restart it to use Rayline? [y/N] ");
    if io::stderr().flush().is_err() {
        return false;
    }
    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Quit the running Codex desktop app. macOS: `osascript ... quit` (clean app
/// teardown). Elsewhere: SIGTERM the app-server process.
fn quit_running_app(app: &RunningApp) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        // Ask the app to quit gracefully. If AppleScript can't reach it, fall
        // back to signalling the app-server process directly.
        //
        // Target the bundle id, not the app name: the desktop app ships as
        // `Codex.app` on older installs and `ChatGPT.app` since the rename,
        // but `com.openai.codex` is stable across both. Matching by name would
        // silently miss the renamed bundle and degrade every quit to SIGTERM.
        let quit = Command::new("osascript")
            .arg("-e")
            .arg("tell application id \"com.openai.codex\" to quit")
            .status();
        if !matches!(quit, Ok(status) if status.success()) {
            signal_term(app.pid)?;
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        signal_term(app.pid)?;
    }
    #[cfg(not(unix))]
    {
        let _ = app;
        return Err(io::Error::other(
            "quitting a running Codex app is not supported on this platform",
        ));
    }

    // Wait for the app-server to actually exit before we relaunch. Launching
    // while the single instance is still tearing down races its own app
    // management and can leave the app in a bad state.
    #[cfg(unix)]
    if wait_for_exit() {
        Ok(())
    } else {
        Err(io::Error::other(
            "the Codex app did not close in time; not relaunching",
        ))
    }
    #[cfg(not(unix))]
    Ok(())
}

/// Send `SIGTERM` to a pid.
#[cfg(unix)]
fn signal_term(pid: u32) -> io::Result<()> {
    // SAFETY: sending a signal to a pid is a simple libc call.
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Poll until the app-server is gone (up to ~6s). Returns whether it exited.
#[cfg(unix)]
fn wait_for_exit() -> bool {
    for _ in 0..30 {
        if pgrep_app_server().is_none() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_server_argv_matches_despite_injected_flags() {
        // Regression: Codex injects `-c` flags between the binary and the
        // subcommand, so a fixed "codex app-server" substring misses it.
        assert!(is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/Resources/codex -c features.code_mode_host=true app-server --analytics-default-enabled"
        ));
        assert!(is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
        // Installs predating the ChatGPT.app rename must keep matching.
        assert!(is_app_server_argv(
            "/Applications/Codex.app/Contents/Resources/codex app-server --analytics-default-enabled"
        ));
    }

    #[test]
    fn config_unchanged_when_settings_present_whatever_the_app_layout() {
        // Same settings, but the app moved [tools] above our table, added its own
        // root key and re-serialised our table (order, spacing): still "unchanged".
        let existing = "model = \"rayline-local\"\nmodel_provider = \"rayline\"\npersonality = \"pragmatic\"\n\n[tools]\nweb_search = true\n\n[model_providers.rayline]\nwire_api=\"responses\"\nname = \"Rayline Local\"\nbase_url =   \"http://127.0.0.1:20811/v1\"\n\n[desktop]\nx = 1\n";
        assert!(!generated_config_differs(
            existing,
            &snippet("rayline-local", false)
        ));
    }

    #[test]
    fn running_app_config_matches_reads_the_apps_own_home() {
        // The comparison basis is the running app-server's CODEX_HOME/config.toml
        // (what it loaded), with the app's own sections tolerated.
        let dir = unique_tmp_dir("running-match");
        let generated = snippet("rayline-local", false);
        fs::write(
            dir.join("config.toml"),
            format!("{generated}\n[desktop]\nx = 1\n"),
        )
        .unwrap();
        let app = RunningApp {
            pid: 1,
            codex_home: Some(dir.to_string_lossy().into_owned()),
        };
        assert!(running_app_config_matches(&app, &generated));

        // A changed model no longer matches.
        assert!(!running_app_config_matches(
            &app,
            &snippet("gpt-5.5", false)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn running_app_config_mismatch_when_home_unknown_or_no_file() {
        // No CODEX_HOME on the process, or no config.toml → treat as mismatch
        // (restart rather than leave stale).
        let no_home = RunningApp {
            pid: 1,
            codex_home: None,
        };
        assert!(!running_app_config_matches(&no_home, "anything"));

        let dir = unique_tmp_dir("running-nofile");
        let empty = RunningApp {
            pid: 1,
            codex_home: Some(dir.to_string_lossy().into_owned()),
        };
        assert!(!running_app_config_matches(&empty, "anything"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_changed_when_model_or_auth_differs() {
        let existing = format!(
            "{}\n[mcp_servers.x]\ny = 1\n",
            snippet("rayline-local", false)
        );
        assert!(generated_config_differs(
            &existing,
            &snippet("gpt-5.5", false)
        ));
        assert!(generated_config_differs(
            &existing,
            &snippet("rayline-local", true)
        ));
    }

    #[test]
    fn auth_link_removed_when_source_absent() {
        // A prior run linked auth.json; the current home has none. The stale link
        // must be cleared so the isolated app can't reuse old credentials.
        let dir = unique_tmp_dir("auth-absent");
        let link = dir.join("auth.json");
        let stale_source = dir.join("old-auth.json");
        fs::write(&stale_source, "OLD").unwrap();
        symlink_file(&stale_source, &link).unwrap();
        assert!(link.exists());

        let missing_source = dir.join("no-such-auth.json");
        reconcile_auth_link(&missing_source, &link).unwrap();

        assert!(!link.exists(), "stale link must be removed");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn auth_link_points_at_current_source() {
        let dir = unique_tmp_dir("auth-present");
        let link = dir.join("auth.json");
        let source = dir.join("real-auth.json");
        fs::write(&source, "TOKEN").unwrap();

        reconcile_auth_link(&source, &link).unwrap();

        assert_eq!(fs::read_to_string(&link).unwrap(), "TOKEN");
        assert_eq!(fs::read_link(&link).unwrap(), source);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn desktop_written_auth_file_kept_when_source_absent() {
        // The user logged in inside the desktop app, which wrote a REAL auth.json
        // in the isolated home. With no source auth (main home never logged in),
        // subscription reconcile must NOT delete it — that's their only credential.
        let dir = unique_tmp_dir("auth-desktop-file");
        let link = dir.join("auth.json");
        fs::write(&link, "DESKTOP_TOKEN").unwrap(); // regular file, not a symlink

        let missing_source = dir.join("no-such-auth.json");
        reconcile_auth_link(&missing_source, &link).unwrap();

        assert_eq!(
            fs::read_to_string(&link).unwrap(),
            "DESKTOP_TOKEN",
            "app-written auth file must be preserved"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_auth_mode_removes_desktop_written_auth_file() {
        // --auth none is an explicit opt-out: even a real app-written auth.json
        // is removed so the app doesn't run with the user's credentials.
        let dir = unique_tmp_dir("auth-none-file");
        let link = dir.join("auth.json");
        fs::write(&link, "DESKTOP_TOKEN").unwrap();

        link_auth_json(&dir, false).unwrap();

        assert!(
            fs::symlink_metadata(&link).is_err(),
            "no-auth must remove even a real auth file"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_auth_mode_removes_link_and_creates_none() {
        // --auth none must not expose the user's Codex credentials: an existing
        // link is removed and no new one is created.
        let dir = unique_tmp_dir("auth-none");
        let link = dir.join("auth.json");
        let stale = dir.join("stale-auth.json");
        fs::write(&stale, "OLD").unwrap();
        symlink_file(&stale, &link).unwrap();
        assert!(link.exists());

        link_auth_json(&dir, false).unwrap();

        assert!(
            fs::symlink_metadata(&link).is_err(),
            "auth.json must be absent under no-auth"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rayline-codex-app-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The snippet `generate_config` renders for a run: subscription on/off.
    fn snippet(model: &str, subscription: bool) -> String {
        rayline_provider_config_toml(model, &default_rayline_base_url(), subscription)
    }

    // An isolated config.toml as the Codex desktop app leaves it: it adds root
    // keys and tables of its own and has moved [tools] above our table.
    fn app_shaped_config(rayline_model: &str) -> String {
        format!(
            "model = \"{rayline_model}\"\nmodel_provider = \"rayline\"\npersonality = \"pragmatic\"\n\n[tools]\nweb_search = true\n\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = \"http://127.0.0.1:20811/v1\"\nwire_api = \"responses\"\n\n[desktop]\nfollowUpQueueMode = \"queue\"\n"
        )
    }

    #[test]
    fn rayline_table_replaced_in_place_below_app_table() {
        // A rerun with new settings must update our table where it is — one
        // copy, still below [tools] — and keep the app's root key and tables.
        let existing = app_shaped_config("rayline-local");
        let merged = apply_rayline_settings(&existing, &snippet("gpt-5.5", true)).unwrap();
        assert_eq!(merged.matches("[model_providers.rayline]").count(), 1);
        assert!(merged.find("[tools]") < merged.find("[model_providers.rayline]"));
        assert!(merged.starts_with("model = \"gpt-5.5\"\nmodel_provider = \"rayline\"\n"));
        assert!(merged.contains("forced_login_method = \"chatgpt\""));
        assert!(merged.contains("requires_openai_auth = true"));
        assert!(merged.contains("personality = \"pragmatic\""));
        assert!(merged.contains("[tools]\nweb_search = true"));
        assert!(merged.contains("[desktop]\nfollowUpQueueMode = \"queue\""));
    }

    #[test]
    fn missing_rayline_table_is_inserted_ahead_of_other_tables() {
        let existing = "[desktop]\nx = 1\n";
        let merged = apply_rayline_settings(existing, &snippet("rayline-local", false)).unwrap();
        assert!(merged.starts_with("model = \"rayline-local\"\nmodel_provider = \"rayline\"\n"));
        assert!(merged.find("[model_providers.rayline]") < merged.find("[desktop]"));
        assert!(!merged.contains("[model_providers]\n"));
    }

    #[test]
    fn auth_none_rerun_removes_subscription_keys() {
        let existing = snippet("rayline-local", true);
        let merged = apply_rayline_settings(&existing, &snippet("rayline-local", false)).unwrap();
        assert!(!merged.contains("forced_login_method"));
        assert!(!merged.contains("requires_openai_auth"));
        assert!(merged.contains("model_provider = \"rayline\""));
    }

    #[test]
    fn unchanged_file_is_returned_byte_for_byte() {
        let existing = format!(
            "# user comment\n{}\n[desktop] # trailing\nx = 1\n",
            snippet("rayline-local", true)
        );
        let merged = apply_rayline_settings(&existing, &snippet("rayline-local", true)).unwrap();
        assert_eq!(merged, existing);
    }

    #[test]
    fn app_reserialisation_is_not_a_change() {
        // The app rewrites the file with its own root keys, CRLF or no trailing
        // newline; none of that touches a Rayline setting.
        let generated = snippet("rayline-local", true);
        let with_app_key = format!("notify = [\"x\"]\n{generated}\n[desktop]\nx = 1");
        assert!(!generated_config_differs(&with_app_key, &generated));
        let crlf = with_app_key.replace('\n', "\r\n");
        assert!(!generated_config_differs(&crlf, &generated));
    }

    #[test]
    fn duplicate_rayline_tables_from_older_release_are_healed() {
        // An earlier release composed a fresh block over a suffix that still held
        // the previous table — invalid TOML the app refuses to load.
        let existing = format!(
            "{}\n[tools]\nweb_search = true\n\n[model_providers.rayline]\nname = \"Rayline Local\"\nbase_url = \"http://127.0.0.1:20811/v1\"\nwire_api = \"responses\"\n\n[desktop]\nx = 1\n",
            snippet("rayline-local", false)
        );
        let generated = snippet("rayline-local", false);
        assert!(generated_config_differs(&existing, &generated));
        let merged = apply_rayline_settings(&existing, &generated).unwrap();
        assert_eq!(merged.matches("[model_providers.rayline]").count(), 1);
        assert!(merged.contains("[tools]"));
        assert!(merged.contains("[desktop]"));
        assert!(merged.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn unrelated_parse_error_is_surfaced_not_clobbered() {
        let dir = unique_tmp_dir("broken");
        fs::write(dir.join("config.toml"), "[desktop\nx = 1\n").unwrap();
        let error = write_isolated_home(&dir, &snippet("rayline-local", false), false).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            fs::read_to_string(dir.join("config.toml")).unwrap(),
            "[desktop\nx = 1\n"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_keeps_tables_and_drops_root_keys() {
        // User's main config.toml: root keys (incl. a dotted one) are dropped,
        // tables and arrays of tables kept.
        let main = "model = \"gpt-5\"\nprofile = \"work\"\nfeatures.x = true\n\n[mcp_servers.foo]\ncommand = \"x\"\n\n[[hooks]]\nname = \"h\"\n";
        let seeded = seed_tables(main);
        assert!(!seeded.contains("model = "));
        assert!(!seeded.contains("profile"));
        assert!(!seeded.contains("features"));
        assert!(seeded.contains("[mcp_servers.foo]\ncommand = \"x\""));
        assert!(seeded.contains("[[hooks]]"));
        assert_eq!(seed_tables("model = \"gpt-5\"\n"), "");
        assert_eq!(seed_tables("not toml ="), "");
    }

    #[test]
    fn seed_with_existing_rayline_table_is_replaced_not_duplicated() {
        // A user who already configured Rayline in their main config.
        let main = "model = \"rayline-local\"\n\n[model_providers.rayline]\nname = \"User Rayline\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\n[mcp_servers.foo]\ncommand = \"x\"\n";
        let merged =
            apply_rayline_settings(&seed_tables(main), &snippet("rayline-local", false)).unwrap();
        assert_eq!(merged.matches("[model_providers.rayline]").count(), 1);
        assert!(!merged.contains("User Rayline"));
        assert!(merged.contains("[mcp_servers.foo]"));
    }

    #[test]
    fn strip_rayline_table_only_removes_that_table() {
        let text = "[a]\nx = 1\n\n[model_providers.rayline]\nname = \"y\"\nbase_url = \"z\"\n\n[b]\nq = 2\n";
        let stripped = strip_rayline_provider_table(text);
        assert!(stripped.contains("[a]"));
        assert!(stripped.contains("[b]"));
        assert!(!stripped.contains("[model_providers.rayline]"));
        assert!(!stripped.contains("base_url = \"z\""));
    }

    #[test]
    fn write_isolated_home_preserves_app_changes_across_reruns() {
        // Over the filesystem: first write, the app appends its own section, a
        // rerun with new settings updates ours and keeps the app's.
        let dir = unique_tmp_dir("merge-e2e");
        let iso = dir.join("config.toml");
        fs::write(
            &iso,
            format!(
                "{}\n[mcp_servers.foo]\ncommand = \"x\"\n",
                snippet("rayline-local", false)
            ),
        )
        .unwrap();
        // App persists a setting afterwards.
        fs::write(
            &iso,
            format!(
                "{}\n[desktop]\nqueue = true\n",
                fs::read_to_string(&iso).unwrap().trim_end()
            ),
        )
        .unwrap();

        write_isolated_home(&dir, &snippet("gpt-5.5", false), false).unwrap();
        let written = fs::read_to_string(&iso).unwrap();
        assert_eq!(written.matches("[model_providers.rayline]").count(), 1);
        assert!(written.contains("model = \"gpt-5.5\""));
        assert!(written.contains("[mcp_servers.foo]"));
        assert!(written.contains("[desktop]\nqueue = true"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn app_server_argv_rejects_cli_and_helpers() {
        assert!(!is_app_server_argv("codex resume 019f3c3e-fcc9"));
        assert!(!is_app_server_argv("codex"));
        assert!(!is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/Frameworks/Codex Helper --type=renderer"
        ));
        assert!(!is_app_server_argv(
            "/Applications/ChatGPT.app/Contents/MacOS/ChatGPT"
        ));
    }

    #[test]
    fn config_toml_points_at_rayline_router() {
        let toml =
            rayline_provider_config_toml("rayline-local", &default_rayline_base_url(), false);
        assert!(toml.contains("model_provider = \"rayline\""));
        assert!(toml.contains("wire_api = \"responses\""));
        assert!(toml.contains("127.0.0.1:20811/v1"));
        // No subscription auth fields when not subscription.
        assert!(!toml.contains("requires_openai_auth"));
    }

    #[test]
    fn config_toml_subscription_adds_auth_fields() {
        let toml = rayline_provider_config_toml("gpt-5.5", &default_rayline_base_url(), true);
        assert!(toml.contains("requires_openai_auth = true"));
        assert!(toml.contains("forced_login_method = \"chatgpt\""));
        assert!(toml.contains("model = \"gpt-5.5\""));
    }

    #[test]
    fn isolated_home_is_under_rayline_dir() {
        let home = isolated_home_path().unwrap();
        assert!(home.ends_with(format!("{}/{}", crate::DOT_CONFIG_DIR, CODEX_APP_HOME_DIR)));
    }
}
