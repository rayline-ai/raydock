//! `raydock codex app`: launch the Codex desktop app on ARC through the
//! Rayline router.
//!
//! Takes the same `--auth`/`--model` flags as `raydock codex` and points the
//! app at the same `rayline` provider. The desktop app-server differs from the
//! CLI in two ways that shape this module:
//!
//! 1. It reads persistent config only from its `CODEX_HOME`, so the provider
//!    goes into the `config.toml` of an app home of its own
//!    (`~/.rayline/harness/codex-app`).
//! 2. It is **single-instance per machine**, locked to the `CODEX_HOME` its
//!    app-server first spawned with. A second `codex app` launch just focuses
//!    the existing window. So if an app-server is already running on a different
//!    home, the user is asked to quit and relaunch it on the Rayline home.

use std::ffi::OsString;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use toml_edit::{DocumentMut, Item, Table, TomlError, Value};

use crate::codex::CodexAuthMode;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppRunRequest {
    pub model: Option<String>,
    pub auth_mode: CodexAuthMode,
    /// Extra args passed through to `codex app` (e.g. a workspace path).
    pub codex_args: Vec<OsString>,
    pub root_env_explicit: bool,
    /// Hosted environment (`--env`) whose router the app talks to.
    pub env_name: Option<String>,
    /// Account bearer (`--auth-token`) for minting/reading the `rlk-` key.
    pub auth_token: Option<String>,
}

pub async fn run(request: AppRunRequest) -> ExitCode {
    let provider = match crate::codex::resolve_provider(
        request.auth_mode,
        request.model.as_deref(),
        request.env_name.as_deref(),
        request.auth_token.as_deref(),
        request.root_env_explicit,
    )
    .await
    {
        Ok(provider) => provider,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };

    // 1. Compute the config to write. Nothing changes on disk yet, so a
    //    declined restart below leaves everything untouched.
    let Some(user_home) = dirs::home_dir() else {
        eprintln!("Error: failed to resolve Codex app home: home directory not found");
        return ExitCode::from(1);
    };
    let home = isolated_home_path(&user_home);
    let config = crate::codex::provider_config_toml(&provider);

    // 2. Reconcile the single-instance app-server. The comparison basis is the
    //    running app-server's OWN loaded config (its CODEX_HOME/config.toml),
    //    not our isolated file: that's what it actually loaded at startup.
    match reconcile_running_app(&home, &config) {
        ReconcileOutcome::AlreadyCurrent => {
            // Config already matches; don't rewrite. Still launch so `codex app`
            // focuses the window / opens the requested workspace.
            eprintln!("The Codex app is already using Raydock. Bringing it to the front.");
        }
        ReconcileOutcome::Cancelled => {
            eprintln!("Leaving the Codex app as it is.");
            return ExitCode::SUCCESS;
        }
        ReconcileOutcome::Proceed => {
            // No conflicting app, or the user agreed to restart: write config now.
            if let Err(error) = write_codex_home(&home, &config) {
                eprintln!("Error: failed to prepare Codex app home: {error}");
                return ExitCode::from(1);
            }
        }
    }

    // 3. Launch (or focus) the Codex app on the isolated home.
    let mut command = crate::codex::codex_command();
    command.env("CODEX_HOME", &home).arg("app");
    command.args(&request.codex_args);
    match command.status() {
        // A missing code means the child was killed by a signal: treat it as
        // failure (matching the Codex CLI path), not silent success.
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("raydock: failed to launch codex app: {error}");
            ExitCode::from(127)
        }
    }
}

/// Resolve the user's real Codex home (`$CODEX_HOME` or `~/.codex`), whose
/// `config.toml` tables seed a new Rayline Codex home.
fn user_codex_home() -> io::Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".codex")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "home directory not found"))
}

/// The stable isolated home Rayline points the desktop app at:
/// `~/.rayline/harness/codex-app`.
fn isolated_home_path(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR)
        .join("harness")
        .join("codex-app")
}

/// Root keys the generated Rayline snippet owns. Together with the
/// `[model_providers.rayline]` table these are the only parts of a Rayline
/// Codex home's `config.toml` Rayline ever writes; everything else in the file
/// belongs to Codex or the user.
const RAYLINE_ROOT_KEYS: [&str; 2] = ["model", "model_provider"];
const MODEL_PROVIDERS_TABLE: &str = "model_providers";
const RAYLINE_PROVIDER: &str = "rayline";

/// True when the existing `config.toml` does not already carry the Rayline
/// settings of the `generated` snippet (a changed `--model`, key or base URL,
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

/// Write a Rayline Codex home (`raydock codex`'s or the desktop app's): its
/// `config.toml` is the Rayline settings merged into whatever the file already
/// holds, written owner-only since it holds the router key. The app writes
/// this only after the launch is committed, so a declined restart never changes
/// the home.
pub(crate) fn write_codex_home(home: &Path, config: &str) -> io::Result<()> {
    let config_path = home.join("config.toml");
    // The document to merge into:
    //  - the home's config.toml when it exists (Codex persists its own settings
    //    there: [desktop], [projects], …),
    //  - on first creation, the tables of the user's main Codex config.toml,
    //  - otherwise nothing.
    // Only a missing file counts as absent; any other read or parse error fails
    // with the path.
    let current = read_if_present(&config_path)?;
    let existing = match &current {
        Some(text) => text.clone(),
        None => {
            let seed_path = user_codex_home()?.join("config.toml");
            match read_if_present(&seed_path)? {
                Some(seed) => seed_tables(&seed)
                    .map_err(|error| toml_parse_error(&seed_path, &seed, &error))?,
                None => String::new(),
            }
        }
    };
    let merged = apply_rayline_settings(&existing, config)
        .map_err(|error| toml_parse_error(&config_path, &existing, &error))?;
    crate::harness::write_files(
        home,
        &[crate::harness::ConfigFile {
            name: "config.toml",
            contents: merged,
            overwrite: true,
            merged_from: current,
            replaces_seed: None,
        }],
    )
}

/// An `InvalidData` error naming `path`, the line of `text` that fails to
/// parse and the parser's message. It never quotes `text`: a Codex config line
/// can hold a credential.
fn toml_parse_error(path: &Path, text: &str, error: &TomlError) -> io::Error {
    let line = error
        .span()
        .and_then(|span| text.get(..span.start))
        .map(|before| format!(" line {}", before.matches('\n').count() + 1))
        .unwrap_or_default();
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!(
            "{}{line}: invalid TOML: {}",
            path.display(),
            error.message().trim_end()
        ),
    )
}

/// The contents of `path`, or `None` when it does not exist. Any other read
/// error fails with the path.
fn read_if_present(path: &Path) -> io::Result<Option<String>> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io::Error::new(
            error.kind(),
            format!("{}: {error}", path.display()),
        )),
    }
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
/// key, including "absent in both".
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
fn seed_tables(seed: &str) -> Result<String, TomlError> {
    let mut doc = seed.parse::<DocumentMut>()?;
    doc.retain(|_, item| {
        item.is_array_of_tables() || item.as_table().is_some_and(|table| !table.is_dotted())
    });
    Ok(doc.to_string())
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
/// we'd generate; otherwise a rerun with a new `--model` would silently keep
/// the old config.
fn reconcile_running_app(home: &Path, config: &str) -> ReconcileOutcome {
    let Some(app) = find_running_app_server() else {
        return ReconcileOutcome::Proceed;
    };
    let same_home = app.codex_home.as_deref().map(Path::new) == Some(home);
    if same_home && running_app_config_matches(&app, config) {
        return ReconcileOutcome::AlreadyCurrent;
    }
    if same_home {
        eprintln!("The Codex app is running with an out-of-date Raydock configuration.");
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
/// argv). macOS: `ps -p <pid> -o command=`.
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
    eprint!("Would you like to restart it to use Raydock? [y/N] ");
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
        let existing = "model = \"arc-sage-1.0\"\nmodel_provider = \"rayline\"\npersonality = \"pragmatic\"\n\n[tools]\nweb_search = true\n\n[model_providers.rayline]\nwire_api=\"responses\"\nname = \"Rayline\"\nbase_url =   \"https://api.rayline.ai/v1\"\nexperimental_bearer_token = \"rlk-test-not-a-real-key\"\n\n[desktop]\nx = 1\n";
        assert!(!generated_config_differs(
            existing,
            &snippet("arc-sage-1.0")
        ));
    }

    #[test]
    fn running_app_config_matches_reads_the_apps_own_home() {
        // The comparison basis is the running app-server's CODEX_HOME/config.toml
        // (what it loaded), with the app's own sections tolerated.
        let dir = unique_tmp_dir("running-match");
        let generated = snippet("arc-sage-1.0");
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
        assert!(!running_app_config_matches(&app, &snippet("gpt-5.5")));
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
    fn config_changed_when_model_or_key_differs() {
        let existing = format!("{}\n[mcp_servers.x]\ny = 1\n", snippet("arc-sage-1.0"));
        assert!(generated_config_differs(&existing, &snippet("gpt-5.5")));
        let mut provider = crate::codex::tests::provider("arc-sage-1.0");
        provider.key = "rlk-test-another-key".to_owned();
        assert!(generated_config_differs(
            &existing,
            &crate::codex::provider_config_toml(&provider)
        ));
    }

    fn unique_tmp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rayline-codex-app-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The snippet a run generates for `model`.
    fn snippet(model: &str) -> String {
        crate::codex::provider_config_toml(&crate::codex::tests::provider(model))
    }

    // An isolated config.toml as the Codex desktop app leaves it: it adds root
    // keys and tables of its own and has moved [tools] above our table.
    fn app_shaped_config(rayline_model: &str) -> String {
        format!(
            "model = \"{rayline_model}\"\nmodel_provider = \"rayline\"\npersonality = \"pragmatic\"\n\n[tools]\nweb_search = true\n\n[model_providers.rayline]\nname = \"Raydock\"\nbase_url = \"https://api.rayline.ai/v1\"\nwire_api = \"responses\"\n\n[desktop]\nfollowUpQueueMode = \"queue\"\n"
        )
    }

    #[test]
    fn rayline_table_replaced_in_place_below_app_table() {
        // A rerun with new settings must update our table where it is — one
        // copy, still below [tools] — and keep the app's root key and tables.
        let existing = app_shaped_config("arc-sage-1.0");
        let merged = apply_rayline_settings(&existing, &snippet("gpt-5.5")).unwrap();
        assert_eq!(merged.matches("[model_providers.rayline]").count(), 1);
        assert!(merged.find("[tools]") < merged.find("[model_providers.rayline]"));
        assert!(merged.starts_with("model = \"gpt-5.5\"\nmodel_provider = \"rayline\"\n"));
        assert!(merged.contains("experimental_bearer_token = \"rlk-test-not-a-real-key\""));
        assert!(merged.contains("personality = \"pragmatic\""));
        assert!(merged.contains("[tools]\nweb_search = true"));
        assert!(merged.contains("[desktop]\nfollowUpQueueMode = \"queue\""));
    }

    #[test]
    fn missing_rayline_table_is_inserted_ahead_of_other_tables() {
        let existing = "[desktop]\nx = 1\n";
        let merged = apply_rayline_settings(existing, &snippet("arc-sage-1.0")).unwrap();
        assert!(merged.starts_with("model = \"arc-sage-1.0\"\nmodel_provider = \"rayline\"\n"));
        assert!(merged.find("[model_providers.rayline]") < merged.find("[desktop]"));
        assert!(!merged.contains("[model_providers]\n"));
    }

    #[test]
    fn unchanged_file_is_returned_byte_for_byte() {
        let existing = format!(
            "# user comment\n{}\n[desktop] # trailing\nx = 1\n",
            snippet("arc-sage-1.0")
        );
        let merged = apply_rayline_settings(&existing, &snippet("arc-sage-1.0")).unwrap();
        assert_eq!(merged, existing);
    }

    #[test]
    fn app_reserialisation_is_not_a_change() {
        // The app rewrites the file with its own root keys, CRLF or no trailing
        // newline; none of that touches a Rayline setting.
        let generated = snippet("arc-sage-1.0");
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
            "{}\n[tools]\nweb_search = true\n\n[model_providers.rayline]\nname = \"Raydock\"\nbase_url = \"https://api.rayline.ai/v1\"\nwire_api = \"responses\"\n\n[desktop]\nx = 1\n",
            snippet("arc-sage-1.0")
        );
        let generated = snippet("arc-sage-1.0");
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
        let error = write_codex_home(&dir, &snippet("arc-sage-1.0")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!error.to_string().contains("[desktop"), "{error}");
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
        let seeded = seed_tables(main).unwrap();
        assert!(!seeded.contains("model = "));
        assert!(!seeded.contains("profile"));
        assert!(!seeded.contains("features"));
        assert!(seeded.contains("[mcp_servers.foo]\ncommand = \"x\""));
        assert!(seeded.contains("[[hooks]]"));
        assert_eq!(seed_tables("model = \"gpt-5\"\n").unwrap(), "");
        assert!(seed_tables("not toml =").is_err());
    }

    #[test]
    fn unreadable_home_config_fails_with_its_path() {
        // config.toml exists but cannot be read as a file: not "absent".
        let dir = unique_tmp_dir("unreadable");
        fs::create_dir(dir.join("config.toml")).unwrap();
        let error = write_codex_home(&dir, &snippet("arc-sage-1.0")).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        let path = dir.join("config.toml");
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{error}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_config_is_absent_only_when_missing() {
        let dir = unique_tmp_dir("seed-errors");
        let user_home = dir.join("user-codex");
        fs::create_dir_all(&user_home).unwrap();
        let seed_path = user_home.join("config.toml");
        let saved = std::env::var_os("CODEX_HOME");
        // SAFETY: the workspace tests run with --test-threads=1; restored below.
        unsafe { std::env::set_var("CODEX_HOME", &user_home) };

        // Missing seed: the home is written from the snippet alone.
        let home = dir.join("missing");
        let missing = write_codex_home(&home, &snippet("arc-sage-1.0"));

        // Unreadable seed: fails with the seed path, writes nothing.
        fs::create_dir(&seed_path).unwrap();
        let unreadable_home = dir.join("unreadable");
        let unreadable = write_codex_home(&unreadable_home, &snippet("arc-sage-1.0"));
        fs::remove_dir(&seed_path).unwrap();

        // Invalid seed TOML: fails with the seed path and line, writes nothing
        // and never quotes the line (it can hold a credential).
        fs::write(
            &seed_path,
            "[mcp_servers.foo.env]\nTOKEN = \"seed-secret-value\n",
        )
        .unwrap();
        let invalid_home = dir.join("invalid");
        let invalid = write_codex_home(&invalid_home, &snippet("arc-sage-1.0"));

        // SAFETY: as above.
        match saved {
            Some(value) => unsafe { std::env::set_var("CODEX_HOME", value) },
            None => unsafe { std::env::remove_var("CODEX_HOME") },
        }

        missing.unwrap();
        assert!(
            fs::read_to_string(home.join("config.toml"))
                .unwrap()
                .contains("[model_providers.rayline]")
        );
        let seed_display = seed_path.display().to_string();
        let unreadable = unreadable.unwrap_err();
        assert_ne!(unreadable.kind(), io::ErrorKind::NotFound);
        assert!(
            unreadable.to_string().contains(&seed_display),
            "{unreadable}"
        );
        assert!(!unreadable_home.join("config.toml").exists());
        let invalid = invalid.unwrap_err();
        assert_eq!(invalid.kind(), io::ErrorKind::InvalidData);
        assert!(
            invalid
                .to_string()
                .starts_with(&format!("{seed_display} line 2: invalid TOML: ")),
            "{invalid}"
        );
        assert!(
            !invalid.to_string().contains("seed-secret-value"),
            "{invalid}"
        );
        assert!(!invalid_home.join("config.toml").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_with_existing_rayline_table_is_replaced_not_duplicated() {
        // A user who already configured Rayline in their main config.
        let main = "model = \"arc-sage-1.0\"\n\n[model_providers.rayline]\nname = \"User Rayline\"\nbase_url = \"https://router.example.test/v1\"\n\n[mcp_servers.foo]\ncommand = \"x\"\n";
        let merged =
            apply_rayline_settings(&seed_tables(main).unwrap(), &snippet("arc-sage-1.0")).unwrap();
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
    fn write_codex_home_preserves_app_changes_across_reruns() {
        // Over the filesystem: first write, the app appends its own section, a
        // rerun with new settings updates ours and keeps the app's.
        let dir = unique_tmp_dir("merge-e2e");
        let iso = dir.join("config.toml");
        fs::write(
            &iso,
            format!(
                "{}\n[mcp_servers.foo]\ncommand = \"x\"\n",
                snippet("arc-sage-1.0")
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

        write_codex_home(&dir, &snippet("gpt-5.5")).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn codex_home_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = unique_tmp_dir("owner-only");
        write_codex_home(&dir, &snippet("arc-sage-1.0")).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("config.toml")), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_app_writes_only_under_its_harness_folder() {
        let home = crate::harness::tests::temp_dir("codex-app-home");
        write_codex_home(&isolated_home_path(&home), &snippet("arc-sage-1.0")).unwrap();
        crate::harness::tests::assert_only_harness_files(&home, "codex-app", &[]);
        fs::remove_dir_all(home).unwrap();
    }
}
