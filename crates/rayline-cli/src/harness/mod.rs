//! `rayline opencode|pi|omp|hermes|openclaw`: launch a third-party coding harness
//! against the hosted Rayline router.
//!
//! Each launcher resolves the environment's router URL and `rlk-` key the same
//! way `rayline claude` does, writes a Rayline-owned config under
//! `~/.config/rayline/harness/<name>/<env>/`, points the harness at it through
//! the harness's own config env var, and execs the harness with the remaining
//! args. The key lives only in that config (0600, in a 0700 dir), never in the
//! harness's environment: every tool subprocess the harness spawns inherits its
//! environment, so a key there would reach a model-visible `env` call. For the
//! same reason the launch removes [`KEY_ENV`] and `RAYLINE_ROUTER_API_KEY`
//! from the child's environment. The config dir is per harness and env (it
//! holds the harness's sessions), so two concurrent launches of one harness on
//! one env with different explicit `RAYLINE_ROUTER_API_KEY` values share the
//! last-written key.
//!
//! Every config uses Anthropic Messages, model `rayline-router`,
//! `contextWindow` 200000 / `maxTokens` 32000 against the hosted router. Under
//! `rayline hybrid run` every renderer advertises the hybrid runtime's admitted
//! limits instead ([`RenderContext::capabilities`]): a harness that asks for
//! more output than the execution core admits has every turn refused.
//!
//! Episode keying: the router keys a routing episode on each harness's own
//! session id where it reads them: opencode and
//! Hermes (on Responses) send `x-session-affinity`, omp `X-Claude-Code-Session-Id` (apiKey
//! mode from omp 18.1.10), and pi and OpenClaw send `x-session-affinity` once
//! their config sets `compat.sendSessionAffinityHeaders`. On Responses, pi's
//! and OpenClaw's `prompt_cache_key` keys the session. A harness session is
//! one episode, so `--resume` / `-c` in a later launch keeps the episode.
//!
//! The per-launch conversation id ([`ConversationIdFallback`]) is the
//! fallback for a gateway that does not read native ids (prod today), and for
//! a wire that sends no native id (OpenClaw and Hermes on Messages): every
//! launch gets a fresh `RAYLINE_CONV_ID` (a UUID) sent as `x-conversation-id`,
//! so one launch is one episode and a resume in a new launch starts a new one.
//! The native compat flags stay on either way; the gateway ranks
//! `x-conversation-id` first when both arrive.

mod hermes;
mod omp;
mod openclaw;
mod opencode;
mod pi;
mod yaml_merge;

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::Value;

/// Env var that carried the router key to the harness before 0.2.11. Removed
/// from the child's environment, with [`ROUTER_KEY_ENV`], so neither an
/// exported key nor one from a parent launch reaches the harness's tools.
pub const KEY_ENV: &str = "RAYLINE_KEY";
/// The explicit router key the launcher reads ([`explicit_router_key`]).
const ROUTER_KEY_ENV: &str = "RAYLINE_ROUTER_API_KEY";
/// Env var that carries the per-launch conversation id (fallback only).
pub const CONV_ID_ENV: &str = "RAYLINE_CONV_ID";
/// Header the gateway reads as the conversation (episode) key.
pub const CONV_HEADER: &str = "x-conversation-id";
/// The virtual model every launcher selects.
pub const ROUTER_MODEL: &str = "rayline-router";
pub const CONTEXT_WINDOW: u64 = 200_000;
pub const MAX_OUTPUT_TOKENS: u64 = 32_000;
/// Runtime override for [`ConversationIdFallback`]: `1` or `0`.
pub const CONV_ID_FALLBACK_ENV: &str = "RAYLINE_HARNESS_CONV_ID_FALLBACK";
/// Envs whose router keys episodes on native harness session ids; the
/// per-launch `x-conversation-id` fallback is off there.
/// TODO: add "prod" (turning the fallback off there) once the prod router
/// reads native session ids.
const NATIVE_SESSION_KEY_ENVS: &[&str] = &["dev"];

/// When a config sends the per-launch `x-conversation-id`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConversationIdFallback {
    /// [`CONV_ID_FALLBACK_ENV`]: `Some(true)` always, `Some(false)` never.
    pub forced: Option<bool>,
    /// The env's gateway does not key on native session ids (prod today).
    pub env_needs_it: bool,
}

impl ConversationIdFallback {
    /// `override_value` is [`CONV_ID_FALLBACK_ENV`] (`1`/`true`/`on` or
    /// `0`/`false`/`off`; anything else is ignored).
    pub(crate) fn new(env_name: &str, override_value: Option<&str>) -> Self {
        Self {
            forced: parse_switch(override_value),
            env_needs_it: !NATIVE_SESSION_KEY_ENVS.contains(&env_name),
        }
    }

    /// Whether a config sends the header. `native_session_id` is false for a
    /// wire on which the harness sends no session id the gateway can key on;
    /// the header is then on unless forced off.
    pub(crate) fn enabled(self, native_session_id: bool) -> bool {
        self.forced
            .unwrap_or(self.env_needs_it || !native_session_id)
    }
}

/// A `1`/`true`/`on` or `0`/`false`/`off` override; anything else is `None`.
pub(crate) fn parse_switch(value: Option<&str>) -> Option<bool> {
    match value.map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "1" | "true" | "on") => Some(true),
        Some(value) if matches!(value.as_str(), "0" | "false" | "off") => Some(false),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Harness {
    OpenCode,
    Pi,
    Omp,
    Hermes,
    OpenClaw,
}

impl Harness {
    pub const ALL: [Harness; 5] = [
        Harness::OpenCode,
        Harness::Pi,
        Harness::Omp,
        Harness::Hermes,
        Harness::OpenClaw,
    ];

    pub fn from_command(command: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|harness| harness.name() == command)
    }

    /// The `rayline <name>` subcommand and the config directory name.
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenCode => "opencode",
            Self::Pi => "pi",
            Self::Omp => "omp",
            Self::Hermes => "hermes",
            Self::OpenClaw => "openclaw",
        }
    }

    /// The executable looked up on PATH.
    pub fn binary(self) -> &'static str {
        self.name()
    }

    fn install_hint(self) -> &'static str {
        match self {
            Self::OpenCode => "Install opencode: https://opencode.ai/docs/",
            Self::Pi => "Install pi: npm install -g @earendil-works/pi-coding-agent",
            Self::Omp => "Install oh-my-pi (omp): https://github.com/can1357/oh-my-pi",
            Self::Hermes => "Install Hermes Agent: https://github.com/NousResearch/hermes-agent",
            Self::OpenClaw => "Install OpenClaw: npm install -g openclaw",
        }
    }

    pub(crate) fn plan(self, ctx: &RenderContext<'_>) -> Result<Plan, String> {
        Ok(match self {
            Self::OpenCode => opencode::plan(ctx),
            Self::Pi => pi::plan(ctx),
            Self::Omp => omp::plan(ctx),
            Self::Hermes => hermes::plan(ctx)?,
            Self::OpenClaw => openclaw::plan(ctx),
        })
    }

    pub(crate) fn hybrid_args(self, args: &[OsString]) -> io::Result<Vec<OsString>> {
        if self == Self::OpenClaw {
            let env_container =
                std::env::var_os("OPENCLAW_CONTAINER").is_some_and(|value| !value.is_empty());
            if env_container || openclaw::uses_container(args) {
                return Err(io::Error::other(
                    "OpenClaw container mode cannot use the hybrid gateway configuration; run an agent locally",
                ));
            }
            Ok(openclaw::launch_args(args))
        } else if self == Self::OpenCode {
            Ok(match find_on_path(self.binary()) {
                Some(binary) => opencode_args(&binary, args),
                None => args.to_vec(),
            })
        } else {
            Ok(args.to_vec())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub harness: Harness,
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub root_env_explicit: bool,
    /// Args passed through to the harness unchanged.
    pub args: Vec<OsString>,
}

/// What a harness renderer needs.
pub(crate) struct RenderContext<'a> {
    pub env_name: &'a str,
    /// The router key, written into the Rayline-owned config.
    pub key: &'a str,
    /// Router origin with no trailing `/` and no `/v1`
    /// (e.g. `https://api-dev.rayline.ai`).
    pub router_url: &'a str,
    pub config_dir: &'a Path,
    /// When configs carry the per-launch `x-conversation-id` header.
    pub conversation_id: ConversationIdFallback,
    /// pi only: whether to set `compat.allowEmptySignature`
    /// ([`pi::allow_empty_signature`]).
    pub allow_empty_signature: bool,
    /// The limits `rayline hybrid run` admits (context window the harness may
    /// budget against, the core's output cap, input modalities). `None`: the
    /// hosted router's defaults.
    pub capabilities: Option<&'a rayline_local_router::ModelCapabilities>,
}

impl RenderContext<'_> {
    /// Context window to advertise for the router model.
    pub fn context_window(&self) -> u64 {
        self.capabilities
            .map_or(CONTEXT_WINDOW, |caps| caps.context_window)
    }

    /// Output cap to advertise for the router model.
    pub fn max_output_tokens(&self) -> u64 {
        self.capabilities
            .map_or(MAX_OUTPUT_TOKENS, |caps| caps.max_output_tokens)
    }

    /// Input modalities to advertise for the router model.
    pub fn input_modalities(&self) -> Value {
        match self.capabilities {
            Some(caps) => serde_json::json!(caps.input_modalities),
            None => serde_json::json!(["text", "image"]),
        }
    }

    /// Base URL for clients that append `/v1/messages` themselves.
    pub fn messages_base(&self) -> String {
        self.router_url.to_owned()
    }

    /// Base URL for clients that expect the `/v1` prefix in the base.
    pub fn v1_base(&self) -> String {
        format!("{}/v1", self.router_url)
    }

    /// Conversation-id headers for a wire on which the harness sends its
    /// native session id: `{CONV_HEADER: value}` (`value` is the harness's
    /// reference to [`CONV_ID_ENV`]) when the fallback is on, else `{}`.
    pub fn conversation_headers(&self, value: &str) -> Value {
        self.conversation_headers_for(value, true)
    }

    /// As [`Self::conversation_headers`], for a wire with or without a native
    /// session id.
    pub fn conversation_headers_for(&self, value: &str, native_session_id: bool) -> Value {
        if self.conversation_id.enabled(native_session_id) {
            serde_json::json!({ CONV_HEADER: value })
        } else {
            serde_json::json!({})
        }
    }
}

#[derive(Debug)]
pub(crate) struct ConfigFile {
    pub name: &'static str,
    pub contents: String,
    /// `true`: Rayline rewrites it on every launch (it holds the env's URL, so
    /// an env change must take effect), owner-only (it holds the key); a
    /// harness whose file also holds user settings merges them into
    /// `contents` first (Hermes). `false`: written once
    /// as a default; the harness or the user may change it afterwards.
    pub overwrite: bool,
    /// The file's text that `contents` was merged from. If the file no longer
    /// reads exactly this when it is replaced (the harness saved a setting
    /// meanwhile), the write is refused rather than losing that edit.
    pub merged_from: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Plan {
    pub files: Vec<ConfigFile>,
    pub env_set: Vec<(&'static str, OsString)>,
    pub env_remove: Vec<&'static str>,
}

pub async fn run(request: RunRequest) -> ExitCode {
    let harness = request.harness;
    let mut command = match prepare(request).await {
        Ok(command) => command,
        Err(error) => {
            eprintln!("Error: {error}");
            return ExitCode::from(1);
        }
    };
    exec_or_status(&mut command, harness)
}

async fn prepare(request: RunRequest) -> Result<Command, String> {
    let home = dirs::home_dir().ok_or("home directory not found")?;
    let harness = request.harness;
    let binary = find_on_path(harness.binary()).ok_or_else(|| {
        format!(
            "`{}` not found on PATH. {}",
            harness.binary(),
            harness.install_hint()
        )
    })?;

    // Same env -> router URL resolution as `rayline claude`: `--env` (default
    // prod), then settings.json `environments.<env>.router_url`.
    let env_name = crate::status::resolve_env(request.env_name.as_deref(), Some(&home));
    let hosted = crate::status::resolve_hosted_environment(&env_name, Some(&home))
        .map_err(|error| error.to_string())?;
    let router_url = normalize_router_url(&hosted.router_url);
    // An explicit RAYLINE_ROUTER_API_KEY wins, as on the codex path; otherwise
    // the stored `rlk-` key, minted (and signed in) on demand like `rayline claude`.
    let key = match explicit_router_key() {
        Some(key) => key,
        None => crate::claude::ensure_router_key(
            &env_name,
            &home,
            request.auth_token.as_deref(),
            request.root_env_explicit,
        )
        .await
        .map_err(|error| error.to_string())?,
    };

    if harness == Harness::Omp {
        omp::warn_if_old(&binary);
    }

    let config_dir = config_dir(&home, harness, &env_name);
    let ctx = RenderContext {
        env_name: &env_name,
        key: &key,
        router_url: &router_url,
        config_dir: &config_dir,
        conversation_id: ConversationIdFallback::new(
            &env_name,
            std::env::var(CONV_ID_FALLBACK_ENV).ok().as_deref(),
        ),
        allow_empty_signature: pi::allow_empty_signature(
            &env_name,
            std::env::var(pi::ALLOW_EMPTY_SIGNATURE_ENV).ok().as_deref(),
        ),
        capabilities: None,
    };
    let plan = harness.plan(&ctx)?;
    write_files(&config_dir, &plan.files).map_err(|error| {
        format!(
            "failed to write {} config in {}: {error}",
            harness.name(),
            config_dir.display()
        )
    })?;

    // Set the id only when a rendered config references it.
    let conv_id = plan
        .files
        .iter()
        .any(|file| file.contents.contains(CONV_ID_ENV))
        .then(|| conversation_id_for(harness, &request.args));
    let episode = match &conv_id {
        Some(id) => format!("conversation {id}"),
        None => "episode keyed on the harness session".to_owned(),
    };
    eprintln!(
        "{}: {} -> {router_url} (env {env_name}, {episode})\n  config: {}",
        crate::DISPLAY_NAME,
        harness.name(),
        config_dir.display()
    );
    let args = match harness {
        Harness::OpenClaw => {
            let env_container =
                std::env::var_os("OPENCLAW_CONTAINER").is_some_and(|value| !value.is_empty());
            if env_container || openclaw::uses_container(&request.args) {
                eprintln!(
                    "  warning: OpenClaw container mode runs inside the container, which does \
                     not see the Rayline config or key"
                );
            }
            let args = if env_container {
                request.args.clone()
            } else {
                openclaw::launch_args(&request.args)
            };
            if args.len() != request.args.len() {
                eprintln!(
                    "  added --local so this agent turn uses the Rayline config; with a \
                     Gateway running, stop it (openclaw gateway stop) or use `openclaw agent exec`"
                );
            }
            args
        }
        Harness::OpenCode => opencode_args(&binary, &request.args),
        _ => request.args,
    };
    Ok(build_command(&binary, &plan, conv_id.as_deref(), &args))
}

/// OpenCode's args with `--standalone` where OpenCode 2 would attach to its
/// background service ([`opencode::launch_args`]).
fn opencode_args(binary: &Path, args: &[OsString]) -> Vec<OsString> {
    let launched = opencode::launch_args(args, opencode::major_version(binary));
    if launched.len() != args.len() {
        eprintln!(
            "  added --standalone so OpenCode uses this config (its background service does not see it)"
        );
    }
    launched
}

fn explicit_router_key() -> Option<String> {
    std::env::var(ROUTER_KEY_ENV)
        .ok()
        .filter(|value| !value.is_empty())
}

/// `~/.config/rayline/harness/<name>/<env>/`
pub(crate) fn config_dir(home: &Path, harness: Harness, env_name: &str) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("harness")
        .join(harness.name())
        .join(env_name)
}

/// Strip trailing slashes and a trailing `/v1` so renderers can add exactly the
/// suffix their client expects.
pub(crate) fn normalize_router_url(url: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn write_files(dir: &Path, files: &[ConfigFile]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    for file in files {
        let path = dir.join(file.name);
        if !file.overwrite && path.exists() {
            continue;
        }
        let tmp = dir.join(format!(".{}.tmp-{}", file.name, std::process::id()));
        write_private(&tmp, &file.contents)?;
        if let Some(base) = &file.merged_from
            && fs::read_to_string(&path).ok().as_ref() != Some(base)
        {
            let _ = fs::remove_file(&tmp);
            return Err(io::Error::other(format!(
                "{} changed while Rayline was updating it; run the command again",
                path.display()
            )));
        }
        fs::rename(&tmp, &path)?;
    }
    Ok(())
}

/// Write `contents` to a new file at `path` that only the owner can read: the
/// Rayline-owned configs hold the router key. A stale file is removed first,
/// since an open of an existing file keeps its mode.
fn write_private(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write as _;
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)?.write_all(contents.as_bytes())
}

pub(crate) fn build_command(
    binary: &Path,
    plan: &Plan,
    conv_id: Option<&str>,
    args: &[OsString],
) -> Command {
    let mut command = Command::new(binary);
    command.args(args);
    // The key reaches the harness through its config only (see the module docs).
    command.env_remove(KEY_ENV);
    command.env_remove(ROUTER_KEY_ENV);
    for name in &plan.env_remove {
        command.env_remove(name);
    }
    for (name, value) in &plan.env_set {
        command.env(name, value);
    }
    match conv_id {
        Some(conv_id) => command.env(CONV_ID_ENV, conv_id),
        // An inherited id (e.g. from a parent launch) must not leak in.
        None => command.env_remove(CONV_ID_ENV),
    };
    command
}

/// The conversation id for this launch: stable for a harness session the
/// args name (OpenClaw `--session-id`), so relaunching it keeps the episode;
/// otherwise a fresh UUID.
pub(crate) fn conversation_id_for(harness: Harness, args: &[OsString]) -> String {
    match harness {
        Harness::OpenClaw => {
            openclaw::session_conversation_id(args).unwrap_or_else(new_conversation_id)
        }
        _ => new_conversation_id(),
    }
}

/// A random (v4) UUID. One per launch; see [`ConversationIdFallback`].
pub(crate) fn new_conversation_id() -> String {
    let mut bytes: [u8; 16] = rand::random();
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

pub(crate) fn json_file(name: &'static str, value: &Value, overwrite: bool) -> ConfigFile {
    let mut contents = serde_json::to_string_pretty(value).expect("config serializes");
    contents.push('\n');
    ConfigFile {
        name,
        contents,
        overwrite,
        merged_from: None,
    }
}

pub(crate) fn yaml_file(name: &'static str, value: &Value, overwrite: bool) -> ConfigFile {
    ConfigFile {
        name,
        contents: to_yaml(value),
        overwrite,
        merged_from: None,
    }
}

/// Minimal YAML writer for the configs above: block mappings and sequences,
/// scalars as JSON literals (a JSON string is a valid YAML double-quoted
/// scalar), and scalar-only sequences in flow style.
pub(crate) fn to_yaml(value: &Value) -> String {
    let mut out = String::new();
    write_yaml(value, 0, &mut out);
    out
}

fn write_yaml(value: &Value, indent: usize, out: &mut String) {
    let pad = " ".repeat(indent);
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                out.push_str(&pad);
                out.push_str(&yaml_key(key));
                out.push(':');
                write_yaml_child(child, indent, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                out.push_str(&pad);
                out.push('-');
                match item {
                    Value::Object(map) if !map.is_empty() => {
                        // `- key: value` with the remaining keys aligned under it.
                        let mut nested = String::new();
                        write_yaml(item, indent + 2, &mut nested);
                        out.push(' ');
                        out.push_str(nested.trim_start());
                    }
                    _ => write_yaml_child(item, indent, out),
                }
            }
        }
        scalar => {
            out.push_str(&pad);
            out.push_str(&scalar.to_string());
            out.push('\n');
        }
    }
}

fn write_yaml_child(child: &Value, indent: usize, out: &mut String) {
    match child {
        Value::Object(map) if map.is_empty() => out.push_str(" {}\n"),
        Value::Object(_) => {
            out.push('\n');
            write_yaml(child, indent + 2, out);
        }
        Value::Array(items)
            if items
                .iter()
                .all(|item| !item.is_object() && !item.is_array()) =>
        {
            let rendered: Vec<String> = items.iter().map(Value::to_string).collect();
            out.push_str(&format!(" [{}]\n", rendered.join(", ")));
        }
        Value::Array(_) => {
            out.push('\n');
            write_yaml(child, indent + 2, out);
        }
        scalar => {
            out.push(' ');
            out.push_str(&scalar.to_string());
            out.push('\n');
        }
    }
}

fn yaml_key(key: &str) -> String {
    let plain = !key.is_empty()
        && key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
    if plain {
        key.to_owned()
    } else {
        Value::String(key.to_owned()).to_string()
    }
}

fn find_on_path(binary_name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let names = executable_names(binary_name, cfg!(windows));
    std::env::split_paths(&path).find_map(|dir| find_in_dir(&dir, &names))
}

/// The first of `names` in `dir` that is a runnable file. On Unix that means
/// an execute bit is set, as a shell's lookup requires.
pub(crate) fn find_in_dir(dir: &Path, names: &[String]) -> Option<PathBuf> {
    names
        .iter()
        .map(|name| dir.join(name))
        .find(|candidate| is_runnable(candidate))
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    fs::metadata(path).is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_runnable(path: &Path) -> bool {
    path.is_file()
}

/// File names to probe in each PATH dir, in order. On Windows only names with
/// a runnable extension count: npm puts an extensionless sh shim next to the
/// `.cmd` one, and CreateProcess cannot run the shim.
pub(crate) fn executable_names(binary_name: &str, windows: bool) -> Vec<String> {
    if windows {
        ["exe", "cmd", "bat", "com"]
            .iter()
            .map(|ext| format!("{binary_name}.{ext}"))
            .collect()
    } else {
        vec![binary_name.to_owned()]
    }
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command, harness: Harness) -> ExitCode {
    use std::os::unix::process::CommandExt as _;

    let error = command.exec();
    eprintln!("rayline: failed to exec {}: {error}", harness.binary());
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command, harness: Harness) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("rayline: failed to run {}: {error}", harness.binary());
            ExitCode::from(127)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;

    pub(crate) const TEST_URL: &str = "https://api-dev.rayline.ai";
    pub(crate) const TEST_KEY: &str = "rlk-test";

    pub(crate) fn temp_dir(label: &str) -> PathBuf {
        // A counter, not the clock: parallel tests can read the same time.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "rayline-harness-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Render `harness` for dev with no override into a temp dir and return
    /// (dir, plan).
    pub(crate) fn render(harness: Harness) -> (PathBuf, Plan) {
        render_with(harness, ConversationIdFallback::new("dev", None))
    }

    /// Render `harness` with the given fallback policy.
    pub(crate) fn render_with(
        harness: Harness,
        conversation_id: ConversationIdFallback,
    ) -> (PathBuf, Plan) {
        let home = temp_dir(harness.name());
        let dir = config_dir(&home, harness, "dev");
        let ctx = RenderContext {
            env_name: "dev",
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id,
            allow_empty_signature: pi::allow_empty_signature("dev", None),
            capabilities: None,
        };
        let plan = harness.plan(&ctx).unwrap();
        write_files(&dir, &plan.files).unwrap();
        (dir, plan)
    }

    pub(crate) fn env_value<'a>(plan: &'a Plan, name: &str) -> Option<&'a OsString> {
        plan.env_set
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value)
    }

    #[test]
    fn config_dir_is_per_harness_and_env() {
        let home = Path::new("/home/u");
        assert_eq!(
            config_dir(home, Harness::Pi, "dev"),
            PathBuf::from("/home/u/.config/rayline/harness/pi/dev")
        );
    }

    #[test]
    fn router_url_is_normalized() {
        for raw in [
            "https://api-dev.rayline.ai",
            "https://api-dev.rayline.ai/",
            "https://api-dev.rayline.ai/v1",
            "https://api-dev.rayline.ai/v1/",
        ] {
            assert_eq!(normalize_router_url(raw), "https://api-dev.rayline.ai");
        }
    }

    #[test]
    fn harness_names_round_trip() {
        for harness in Harness::ALL {
            assert_eq!(Harness::from_command(harness.name()), Some(harness));
        }
        assert_eq!(Harness::from_command("claude"), None);
    }

    #[test]
    fn conversation_id_is_a_v4_uuid() {
        let id = new_conversation_id();
        assert_eq!(id.len(), 36);
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(
            parts[3].chars().next(),
            Some('8' | '9' | 'a' | 'b')
        ));
        assert_ne!(id, new_conversation_id());
    }

    #[test]
    fn write_files_overwrites_owned_and_keeps_defaults() {
        let dir = temp_dir("write").join("cfg");
        let files = |text: &str| {
            vec![
                ConfigFile {
                    name: "owned.json",
                    contents: text.to_owned(),
                    overwrite: true,
                    merged_from: None,
                },
                ConfigFile {
                    name: "default.json",
                    contents: text.to_owned(),
                    overwrite: false,
                    merged_from: None,
                },
            ]
        };
        write_files(&dir, &files("one")).unwrap();
        write_files(&dir, &files("two")).unwrap();
        assert_eq!(fs::read_to_string(dir.join("owned.json")).unwrap(), "two");
        assert_eq!(fs::read_to_string(dir.join("default.json")).unwrap(), "one");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn command_carries_conversation_id_and_removes_keys() {
        let plan = Plan {
            files: Vec::new(),
            env_set: vec![("OPENCODE_CONFIG", OsString::from("/x/opencode.json"))],
            env_remove: vec!["CLAUDE_CONFIG_DIR"],
        };
        let command = build_command(
            Path::new("/bin/true"),
            &plan,
            Some("conv-1"),
            &[OsString::from("run"), OsString::from("hi")],
        );
        let envs: Vec<(String, Option<String>)> = command
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(envs.contains(&("RAYLINE_KEY".into(), None)));
        assert!(envs.contains(&("RAYLINE_ROUTER_API_KEY".into(), None)));
        assert!(
            !envs
                .iter()
                .any(|(_, value)| value.as_deref() == Some(TEST_KEY))
        );
        assert!(envs.contains(&("RAYLINE_CONV_ID".into(), Some("conv-1".into()))));
        assert!(envs.contains(&("OPENCODE_CONFIG".into(), Some("/x/opencode.json".into()))));
        assert!(envs.contains(&("CLAUDE_CONFIG_DIR".into(), None)));
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["run", "hi"]);
    }

    #[test]
    fn conversation_id_fallback_defaults_by_env() {
        let dev = ConversationIdFallback::new("dev", None);
        assert!(!dev.enabled(true));
        // A wire with no native id (OpenClaw Messages) keeps it even on dev.
        assert!(dev.enabled(false));
        for env in ["prod", "staging"] {
            let fallback = ConversationIdFallback::new(env, None);
            assert!(fallback.enabled(true) && fallback.enabled(false), "{env}");
        }
    }

    #[test]
    fn conversation_id_fallback_override() {
        for on in ["1", "true", "ON", " on "] {
            let fallback = ConversationIdFallback::new("dev", Some(on));
            assert!(fallback.enabled(true), "{on:?}");
        }
        for off in ["0", "false", "Off"] {
            let fallback = ConversationIdFallback::new("prod", Some(off));
            assert!(
                !fallback.enabled(true) && !fallback.enabled(false),
                "{off:?}"
            );
        }
        // Unrecognized or empty values keep the env default.
        assert!(ConversationIdFallback::new("prod", Some("")).enabled(true));
        assert!(!ConversationIdFallback::new("dev", Some("maybe")).enabled(true));
    }

    #[test]
    fn native_session_keys_are_the_dev_default() {
        let command = build_command(Path::new("/bin/true"), &Plan::default(), None, &[]);
        let conv = command
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(CONV_ID_ENV));
        assert_eq!(conv, Some((std::ffi::OsStr::new(CONV_ID_ENV), None)));
        // OpenClaw and Hermes are the exceptions: no native id on Messages.
        for harness in Harness::ALL
            .into_iter()
            .filter(|harness| !matches!(harness, Harness::OpenClaw | Harness::Hermes))
        {
            let (dir, plan) = render(harness);
            for file in &plan.files {
                let text = fs::read_to_string(dir.join(file.name)).unwrap();
                assert!(
                    !text.contains(CONV_HEADER) && !text.contains(CONV_ID_ENV),
                    "{}: {} still sends the per-launch id",
                    harness.name(),
                    file.name
                );
            }
        }
    }

    #[test]
    fn per_launch_fallback_sends_the_conversation_header() {
        for harness in Harness::ALL {
            let (dir, plan) = render_with(harness, ConversationIdFallback::new("prod", None));
            let sends = plan.files.iter().any(|file| {
                let text = fs::read_to_string(dir.join(file.name)).unwrap();
                text.contains(CONV_HEADER) && text.contains(CONV_ID_ENV)
            });
            assert!(sends, "{} must send {CONV_HEADER}", harness.name());
        }
    }

    #[test]
    fn create_once_files_never_reference_the_conversation_id() {
        // RAYLINE_CONV_ID is set from the rendered plan, so a file that is not
        // rewritten each launch must never reference it.
        for harness in Harness::ALL {
            for fallback in [
                ConversationIdFallback::new("dev", None),
                ConversationIdFallback::new("prod", None),
            ] {
                let (_dir, plan) = render_with(harness, fallback);
                for file in plan.files.iter().filter(|file| !file.overwrite) {
                    assert!(
                        !file.contents.contains(CONV_ID_ENV),
                        "{}: {}",
                        harness.name(),
                        file.name
                    );
                }
            }
        }
    }

    /// The key reaches each harness through an owner-only Rayline config file,
    /// never through an env var a tool subprocess would inherit.
    #[test]
    fn key_is_only_in_owner_only_config_files() {
        for harness in Harness::ALL {
            let (dir, plan) = render(harness);
            let mut holds_key = false;
            for file in &plan.files {
                let path = dir.join(file.name);
                let text = fs::read_to_string(&path).unwrap();
                holds_key |= text.contains(TEST_KEY);
                assert!(!text.contains(KEY_ENV), "{}: {}", harness.name(), file.name);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    let mode = fs::metadata(&path).unwrap().permissions().mode();
                    assert_eq!(mode & 0o777, 0o600, "{}: {}", harness.name(), file.name);
                }
            }
            assert!(holds_key, "{} config must carry the key", harness.name());
            assert!(
                plan.env_set
                    .iter()
                    .all(|(_, value)| !value.to_string_lossy().contains(TEST_KEY)),
                "{}",
                harness.name()
            );
        }
    }

    /// #1 (0.2.11): an exported key, or one a parent launch set, must not reach
    /// the harness, whose tool subprocesses inherit its environment.
    #[cfg(unix)]
    #[test]
    fn harness_child_never_inherits_a_router_key() {
        let vars = [KEY_ENV, ROUTER_KEY_ENV];
        let previous: Vec<_> = vars.iter().map(std::env::var_os).collect();
        for var in vars {
            // SAFETY: tests run with --test-threads=1 (AGENTS.md), so no other
            // thread reads the environment concurrently.
            unsafe { std::env::set_var(var, "rlk-test-not-a-real-key") };
        }
        let output = build_command(Path::new("/usr/bin/env"), &Plan::default(), None, &[])
            .output()
            .expect("run env");
        for (var, value) in vars.iter().zip(previous) {
            // SAFETY: as above.
            match value {
                Some(value) => unsafe { std::env::set_var(var, value) },
                None => unsafe { std::env::remove_var(var) },
            }
        }
        let printed = String::from_utf8_lossy(&output.stdout);
        assert!(output.status.success());
        assert!(
            !printed.contains("rlk-test-not-a-real-key"),
            "the harness environment still carries a router key"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_files_replaces_a_readable_file_with_an_owner_only_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = temp_dir("mode").join("cfg");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owned.json");
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let files = [ConfigFile {
            name: "owned.json",
            contents: "new".to_owned(),
            overwrite: true,
            merged_from: None,
        }];
        write_files(&dir, &files).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        let mode = fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn windows_lookup_skips_the_extensionless_npm_shim() {
        // npm's global bin dir holds `pi` (a sh script), `pi.cmd` and
        // `pi.ps1`; only `pi.cmd` runs under Windows' CreateProcess.
        let dir = temp_dir("pathext");
        for name in ["pi", "pi.cmd", "pi.ps1"] {
            fs::write(dir.join(name), "").unwrap();
        }
        let found = executable_names("pi", true)
            .into_iter()
            .map(|name| dir.join(name))
            .find(|candidate| candidate.is_file());
        assert_eq!(found, Some(dir.join("pi.cmd")));
        assert_eq!(executable_names("pi", false), ["pi"]);
    }

    #[cfg(unix)]
    #[test]
    fn path_lookup_skips_a_non_executable_file() {
        use std::os::unix::fs::PermissionsExt as _;
        let stale = temp_dir("noexec");
        let good = temp_dir("exec");
        fs::write(stale.join("pi"), "").unwrap();
        fs::set_permissions(stale.join("pi"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(good.join("pi"), "").unwrap();
        fs::set_permissions(good.join("pi"), fs::Permissions::from_mode(0o755)).unwrap();
        let names = executable_names("pi", false);
        assert_eq!(find_in_dir(&stale, &names), None);
        assert_eq!(find_in_dir(&good, &names), Some(good.join("pi")));
    }

    #[test]
    fn yaml_writer_shapes() {
        let value = json!({
            "a": {"b": "x", "list": ["text", "image"], "empty": {}},
            "seq": [{"id": "m", "n": 1}],
            "x-y": true
        });
        assert_eq!(
            to_yaml(&value),
            "a:\n  b: \"x\"\n  empty: {}\n  list: [\"text\", \"image\"]\nseq:\n  - id: \"m\"\n    n: 1\nx-y: true\n"
        );
    }
}
