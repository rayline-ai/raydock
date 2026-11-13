//! `rayline run opencode|pi|omp|hermes|openclaw`: launch a third-party coding harness
//! against the hosted Rayline router.
//!
//! Each launcher resolves the environment's router URL and `rlk-` key the same
//! way `rayline run claude` does, writes a Rayline-owned config under
//! `~/.rayline/harness/<name>/`, points the harness at it through
//! the harness's own config env var, and execs the harness with the remaining
//! args. The key lives only in that config (0600, in a 0700 dir), never in the
//! harness's environment: every tool subprocess the harness spawns inherits its
//! environment, so a key there would reach a model-visible `env` call. For the
//! same reason the launch removes `RAYLINE_ROUTER_API_KEY` from the child's
//! environment. The config dir is per harness (it holds the harness's
//! sessions), so two concurrent launches of one harness with different keys
//! share the last-written key.
//!
//! Every config uses Anthropic Messages, except OpenClaw, whose default
//! provider speaks OpenAI Chat Completions. The models, their names and their
//! limits are the ones the router's `GET /v1/models` lists for the key, read
//! once per launch ([`fetch_hosted_models`]): the main agent defaults to the
//! listed ARC Sage (`product: arc-sage-1.0`) and, where the harness has a
//! separate small/background model role, that role to the listed ARC Swift
//! (`product: arc-swift-1.0`), so a contributor or ZDR key gets its own ids.
//! Under `rayline run <harness> --local` every renderer keeps `rayline-auto` (the execution
//! core's virtual model) and advertises the hybrid runtime's admitted limits
//! instead ([`RenderContext::capabilities`]): a harness that asks for more
//! output than the execution core admits has every turn refused.
//!
//! Episode keying: the router keys a routing episode on each harness's own
//! session id where it reads them ([`SESSION_AFFINITY_HEADER`]). opencode
//! sends it on its own; omp sends `X-Claude-Code-Session-Id` (apiKey mode from
//! omp 18.1.10); pi (on Messages) and OpenClaw (on Chat Completions) send it
//! once their config sets `compat.sendSessionAffinityHeaders`; Hermes (on
//! Messages) once its provider entry names the header in
//! `session_affinity_header` (from Hermes v2026.9.21). On Responses, pi's,
//! OpenClaw's and Hermes' `prompt_cache_key` keys the session instead. A harness session
//! is one episode, so `--resume` / `-c` / `--session-id` in a later launch
//! keeps the episode. These launchers talk to the hosted router directly; a
//! client that reaches it through `rld`'s local router keeps its session
//! headers too (`x-session-affinity`, `x-claude-code-session-id`,
//! `x-session-id`, `x-conversation-id`), which that router forwards only to
//! the hosted Rayline router. Where that router translates the request to
//! another protocol, a body-only `session_id` or Responses `prompt_cache_key`
//! reaches the hosted router as `x-session-affinity`.
//!
//! The per-launch conversation id ([`ConversationIdFallback`]) is off by
//! default on every wire: `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` turns it on,
//! and then each launch gets a fresh `RAYLINE_CONV_ID` (a UUID) sent as
//! `x-conversation-id`, so one launch is one episode and a resume in a new
//! launch starts a new one (for Hermes only when the launch starts its shared
//! gateway, see [`hermes`]). That also collapses a subagent's turns into its
//! parent's episode where the harness attaches the header to every request of
//! the process. The native compat flags and config keys stay on either way;
//! the gateway ranks `x-conversation-id` first when both arrive. The override
//! applies to `rayline run <harness> --local` too, so forcing it there accepts the same
//! episode collapse rather than silently doing nothing on that path; either
//! path exports [`CONV_ID_ENV`] only when a rendered config references it
//! ([`conversation_id_for`]).

pub(crate) mod hermes;
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

/// The router key a parent launch hands its child processes. Removed from the
/// harness's environment so it never reaches the harness's tools.
const ROUTER_KEY_ENV: &str = "RAYLINE_ROUTER_API_KEY";
/// Env var that carries the per-launch conversation id (fallback only).
pub const CONV_ID_ENV: &str = "RAYLINE_CONV_ID";
/// Header the gateway reads as the conversation (episode) key.
pub const CONV_HEADER: &str = "x-conversation-id";
/// Header a harness's own session id rides on, which the gateway keys the
/// episode on when no [`CONV_HEADER`] arrives. Most harnesses send it under
/// this name on their own; Hermes needs the name written into its config
/// ([`hermes::SESSION_AFFINITY_KEY`]).
pub const SESSION_AFFINITY_HEADER: &str = "x-session-affinity";
/// The execution core's virtual model under `rayline run <harness> --local`.
pub const ROUTER_MODEL: &str = crate::VIRTUAL_ROUTER_MODEL;

/// A model's price in USD per million tokens, from the router's
/// `GET /v1/arc/prices` card.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// A model a rendered config offers, at the limits it advertises.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LaunchModel {
    pub id: String,
    pub display_name: String,
    pub context_window: u64,
    pub max_output_tokens: u64,
    /// The ARC product the model runs (`product` in `GET /v1/models`): the
    /// same `arc-sage-1.0` for ARC Sage in every data mode. `None` for a
    /// model that is no ARC product (hybrid's virtual model).
    pub product: Option<String>,
    /// Its row on the price card ([`LaunchModels::apply_arc_prices`]), for a
    /// harness that shows costs (omp); `None` when not read or not on it.
    pub price: Option<ModelPrice>,
}

/// The models a rendered config selects and offers.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LaunchModels {
    /// The main agent's default.
    pub main: String,
    /// The default for a harness's small/background or subagent model role.
    pub background: String,
    /// Every model the provider entry lists, default first.
    pub offered: Vec<LaunchModel>,
}

impl LaunchModels {
    /// A hosted launch: every model the router's `GET /v1/models` lists for
    /// the key, which are the standard, contributor or ZDR ids by the
    /// account's data mode. The main agent runs the entry whose `product` is
    /// ARC Sage, the background role the one whose `product` is ARC Swift;
    /// a listing without either is an error.
    pub(crate) fn from_listing(body: &Value) -> Result<Self, String> {
        let data = body
            .get("data")
            .and_then(Value::as_array)
            .ok_or("the response has no `data` list")?;
        let mut offered = data
            .iter()
            .map(|entry| {
                let id = entry
                    .get("id")
                    .and_then(Value::as_str)
                    .ok_or("a listed model has no id")?;
                let limit = |field: &str| {
                    entry
                        .get(field)
                        .and_then(Value::as_u64)
                        .ok_or_else(|| format!("{id} has no {field}"))
                };
                Ok(LaunchModel {
                    id: id.to_owned(),
                    // An entry without a display name goes by its id.
                    display_name: entry
                        .get("display_name")
                        .and_then(Value::as_str)
                        .unwrap_or(id)
                        .to_owned(),
                    context_window: limit("context_length")?,
                    max_output_tokens: limit("max_output_tokens")?,
                    product: entry
                        .get("product")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    price: None,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let product_model = |product: &str| {
            offered
                .iter()
                .find(|model| model.product.as_deref() == Some(product))
                .map(|model| model.id.clone())
                .ok_or_else(|| format!("no listed model has \"product\": \"{product}\""))
        };
        let main = product_model(crate::ARC_SAGE_MODEL)?;
        let background = product_model(crate::ARC_SWIFT_MODEL)?;
        offered.sort_by_key(|model| model.id != main);
        Ok(Self {
            main,
            background,
            offered,
        })
    }

    /// `rayline run <harness> --local`: the execution core serves one virtual model, at
    /// the limits the hybrid runtime admits.
    pub(crate) fn hybrid(capabilities: &rayline_local_router::ModelCapabilities) -> Self {
        Self {
            main: ROUTER_MODEL.to_owned(),
            background: ROUTER_MODEL.to_owned(),
            offered: vec![LaunchModel {
                id: ROUTER_MODEL.to_owned(),
                display_name: "Raydock router".to_owned(),
                context_window: capabilities.context_window,
                max_output_tokens: capabilities.max_output_tokens,
                product: None,
                price: None,
            }],
        }
    }

    /// Read a `GET /v1/arc/prices` answer: each offered model's price by its
    /// own id, since every data mode has its own row. A model without a
    /// complete row gets no price.
    pub(crate) fn apply_arc_prices(&mut self, prices: &Value) {
        for model in &mut self.offered {
            let row = &prices["card"]["prices"][model.id.as_str()];
            let field = |name: &str| row.get(name).and_then(Value::as_f64);
            model.price = match (
                field("input"),
                field("output"),
                field("cache_read"),
                field("cache_write"),
            ) {
                (Some(input), Some(output), Some(cache_read), Some(cache_write)) => {
                    Some(ModelPrice {
                        input,
                        output,
                        cache_read,
                        cache_write,
                    })
                }
                _ => None,
            };
        }
    }

    /// The main agent's model.
    pub(crate) fn main_model(&self) -> &LaunchModel {
        &self.offered[0]
    }
}

/// `GET /v1/models` with the launch's key: the models a hosted launch offers
/// ([`LaunchModels::from_listing`]). Fails the launch when the router cannot
/// say, rather than guessing ids for a data mode it did not name.
pub(crate) async fn fetch_hosted_models(
    router_url: &str,
    key: &str,
) -> Result<LaunchModels, String> {
    let url = format!("{router_url}/v1/models");
    get_json(&url, Some(key))
        .await
        .and_then(|body| LaunchModels::from_listing(&body))
        .map_err(|error| format!("couldn't read this key's models from {url}: {error}"))
}

/// The router's public ARC price card, `GET /v1/arc/prices`.
pub(crate) async fn fetch_arc_prices(router_url: &str) -> Result<Value, String> {
    let url = format!("{router_url}/v1/arc/prices");
    get_json(&url, None)
        .await
        .map_err(|error| format!("couldn't read the ARC prices from {url}: {error}"))
}

async fn get_json(url: &str, bearer: Option<&str>) -> Result<Value, String> {
    let mut request = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?
        .get(url);
    if let Some(bearer) = bearer {
        request = request.bearer_auth(bearer);
    }
    let response = request.send().await.map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| format!("HTTP {status}: {error}"))?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {body}"));
    }
    Ok(body)
}

/// Output ceiling for models outside the hosted ARC tiers: the harness
/// default the router admits.
pub const MAX_OUTPUT_TOKENS: u64 = 32_000;
/// Runtime override for [`ConversationIdFallback`]: `1` or `0`.
pub const CONV_ID_FALLBACK_ENV: &str = "RAYLINE_HARNESS_CONV_ID_FALLBACK";

/// When a config sends the per-launch `x-conversation-id`: only when
/// [`CONV_ID_FALLBACK_ENV`] forces it. The router keys episodes on native
/// harness session ids, or derives one where a wire has none.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ConversationIdFallback {
    /// [`CONV_ID_FALLBACK_ENV`]: `Some(true)` always, `Some(false)` never.
    pub forced: Option<bool>,
}

impl ConversationIdFallback {
    /// `override_value` is [`CONV_ID_FALLBACK_ENV`] (`1`/`true`/`on` or
    /// `0`/`false`/`off`; anything else is ignored).
    pub(crate) fn new(override_value: Option<&str>) -> Self {
        Self {
            forced: parse_switch(override_value),
        }
    }

    /// Whether a config sends the header.
    pub(crate) fn enabled(self) -> bool {
        self.forced == Some(true)
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

    /// The installed harness's major version, where a renderer or the launch
    /// args depend on it (OpenCode); None otherwise or when unreadable.
    pub(crate) fn major_version(self, binary: Option<&Path>) -> Option<u64> {
        match self {
            Self::OpenCode => opencode::major_version(binary?),
            _ => None,
        }
    }

    pub(crate) fn hybrid_args(
        self,
        args: &[OsString],
        major_version: Option<u64>,
    ) -> io::Result<Vec<OsString>> {
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
            Ok(opencode_args(args, major_version))
        } else {
            Ok(args.to_vec())
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub harness: Harness,
    pub env_name: Option<String>,
    pub root_env_explicit: bool,
    /// Args passed through to the harness unchanged.
    pub args: Vec<OsString>,
}

/// What a harness renderer needs.
pub(crate) struct RenderContext<'a> {
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
    /// The limits `rayline run <harness> --local` admits (context window the harness may
    /// budget against, the core's output cap, input modalities). `None`: the
    /// hosted router's defaults.
    pub capabilities: Option<&'a rayline_local_router::ModelCapabilities>,
    /// The installed harness's major version ([`Harness::major_version`]).
    pub harness_major_version: Option<u64>,
    /// The models this launch selects and offers: the router's listing for a
    /// hosted launch, [`LaunchModels::hybrid`] under `rayline run <harness> --local`.
    pub models: LaunchModels,
}

impl RenderContext<'_> {
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

    /// Conversation-id headers: `{CONV_HEADER: value}` (`value` is the
    /// harness's reference to [`CONV_ID_ENV`]) when the fallback is forced
    /// on, else `{}`.
    pub fn conversation_headers(&self, value: &str) -> Value {
        if self.conversation_id.enabled() {
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

    // Same env -> router URL resolution as `rayline run claude`: built-in prod, or
    // settings.json `environments.<env>.base_url` for `--env`.
    let hosted = crate::status::resolve_hosted_environment(request.env_name.as_deref(), &home)
        .map_err(|error| error.to_string())?;
    let env_name = hosted.name().to_owned();
    let router_url = normalize_router_url(&hosted.base_url);
    // The stored `rlk-` key, signing in first when signed out, like `rayline run claude`.
    let key = crate::status::router_key(&hosted, &home)
        .await
        .map_err(|error| error.to_string())?;

    if harness == Harness::Omp {
        omp::warn_if_old(&binary);
    }

    let mut models = fetch_hosted_models(&router_url, &key).await?;
    // omp shows each model's cost, from the price card.
    if harness == Harness::Omp {
        match fetch_arc_prices(&router_url).await {
            Ok(prices) => models.apply_arc_prices(&prices),
            Err(error) => eprintln!("warning: {error}; omp shows no model costs"),
        }
    }
    let config_dir = config_dir(&home, harness);
    let ctx = RenderContext {
        key: &key,
        router_url: &router_url,
        config_dir: &config_dir,
        conversation_id: ConversationIdFallback::new(
            std::env::var(CONV_ID_FALLBACK_ENV).ok().as_deref(),
        ),
        allow_empty_signature: pi::allow_empty_signature(
            &env_name,
            std::env::var(pi::ALLOW_EMPTY_SIGNATURE_ENV).ok().as_deref(),
        ),
        capabilities: None,
        harness_major_version: harness.major_version(Some(&binary)),
        models,
    };
    let plan = harness.plan(&ctx)?;
    write_files(&config_dir, &plan.files).map_err(|error| {
        format!(
            "failed to write {} config in {}: {error}",
            harness.name(),
            config_dir.display()
        )
    })?;

    let conv_id = conversation_id_for(&plan, new_conversation_id);
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
        Harness::OpenCode => opencode_args(&request.args, ctx.harness_major_version),
        _ => request.args,
    };
    let command = build_command(&binary, &plan, conv_id.as_deref(), &args);
    if harness == Harness::Hermes && cfg!(windows) {
        hermes::prepare_launch(&command)?;
    }
    Ok(command)
}

/// OpenCode's args with `--standalone` where OpenCode 2 would attach to its
/// background service ([`opencode::launch_args`]).
fn opencode_args(args: &[OsString], major_version: Option<u64>) -> Vec<OsString> {
    let launched = opencode::launch_args(args, major_version);
    if launched.len() != args.len() {
        eprintln!(
            "  added --standalone so OpenCode uses this config (its background service does not see it)"
        );
    }
    launched
}

/// `~/.rayline/harness/<name>/`
pub(crate) fn config_dir(home: &Path, harness: Harness) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR)
        .join("harness")
        .join(harness.name())
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
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(dir, true)?;
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

/// Write `contents` to a new file at `path` that only the owner can read (mode
/// 0600 on Unix, an owner-only DACL on Windows, set before anything is
/// written): the Rayline-owned configs hold the router key. A stale file is
/// removed first, since an open of an existing file keeps its mode.
pub(crate) fn write_private(path: &Path, contents: &str) -> io::Result<()> {
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
    let mut file = options.open(path)?;
    #[cfg(windows)]
    rayline_local_router::windows_state::restrict_to_owner(path, false)?;
    file.write_all(contents.as_bytes())
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

/// The per-launch id to export, minted by `mint` only when a rendered config
/// actually references [`CONV_ID_ENV`]. A harness that keys the episode on its
/// own session id references nothing, and then exporting an id would leave an
/// unused variable in the environment every tool subprocess inherits.
pub(crate) fn conversation_id_for(plan: &Plan, mint: impl FnOnce() -> String) -> Option<String> {
    plan.files
        .iter()
        .any(|file| file.contents.contains(CONV_ID_ENV))
        .then(mint)
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

pub(crate) fn find_on_path(binary_name: &str) -> Option<PathBuf> {
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

/// The program to launch for harness `name`, given the `PATH` to search.
/// Windows process creation does not apply PATHEXT, so npm's `.cmd` launchers
/// (codex, pi) are resolved the way a shell would; Unix keeps the bare name
/// and its usual PATH lookup.
pub(crate) fn launch_program(name: &str, path: Option<std::ffi::OsString>) -> PathBuf {
    if cfg!(windows) {
        let names = executable_names(name, true);
        if let Some(found) = path
            .and_then(|path| std::env::split_paths(&path).find_map(|dir| find_in_dir(&dir, &names)))
        {
            return found;
        }
    }
    PathBuf::from(name)
}

#[cfg(unix)]
fn exec_or_status(command: &mut Command, harness: Harness) -> ExitCode {
    use std::os::unix::process::CommandExt as _;

    let error = command.exec();
    eprintln!("raydock: failed to exec {}: {error}", harness.binary());
    ExitCode::from(127)
}

#[cfg(not(unix))]
fn exec_or_status(command: &mut Command, harness: Harness) -> ExitCode {
    match command.status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("raydock: failed to run {}: {error}", harness.binary());
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

    /// Asserts that Rayline wrote files under `home` and that each one is
    /// under `~/.rayline/harness/<name>/`, so none is under `~/.config/rayline`.
    /// `inputs` are paths under `home` the test or the harness itself owns.
    pub(crate) fn assert_only_harness_files(home: &Path, name: &str, inputs: &[&str]) {
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if fs::symlink_metadata(&path).unwrap().is_dir() {
                    walk(&path, files);
                } else {
                    files.push(path);
                }
            }
        }
        let mut files = Vec::new();
        walk(home, &mut files);
        files.retain(|file| {
            !inputs
                .iter()
                .any(|input| file.starts_with(home.join(input)))
        });
        assert!(!files.is_empty(), "{name} wrote nothing");
        let root = home.join(".rayline/harness").join(name);
        for file in files {
            assert!(
                file.starts_with(&root),
                "{name} wrote {} outside {}",
                file.display(),
                root.display()
            );
        }
    }

    /// A `GET /v1/models` answer for a key in `mode` (`None`: standard),
    /// listing Swift before Sage as the router does.
    pub(crate) fn listing(mode: Option<&str>) -> Value {
        let entry = |product: &str, name: &str| {
            let id = mode.map_or_else(
                || product.to_owned(),
                |mode| product.replace("-1.0", &format!("-{mode}-1.0")),
            );
            json!({
                "id": id, "object": "model", "owned_by": "rayline",
                "display_name": name, "product": product,
                "context_length": 1_000_000, "max_output_tokens": 128_000
            })
        };
        json!({
            "object": "list",
            "data": [
                entry(crate::ARC_SWIFT_MODEL, "ARC Swift"),
                entry(crate::ARC_SAGE_MODEL, "ARC Sage")
            ]
        })
    }

    /// The models a standard key's hosted launch offers.
    pub(crate) fn hosted_models() -> LaunchModels {
        LaunchModels::from_listing(&listing(None)).unwrap()
    }

    #[test]
    fn a_listing_picks_sage_and_swift_by_product_and_keeps_its_limits() {
        let mut body = listing(Some("contributor"));
        body["data"][0]["context_length"] = json!(200_000);
        body["data"].as_array_mut().unwrap().push(json!({
            "id": "other/model", "object": "model", "owned_by": "other",
            "context_length": 32_000, "max_output_tokens": 4_096
        }));
        let models = LaunchModels::from_listing(&body).unwrap();
        assert_eq!(models.main, "arc-sage-contributor-1.0");
        assert_eq!(models.background, "arc-swift-contributor-1.0");
        let ids: Vec<_> = models
            .offered
            .iter()
            .map(|model| model.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "arc-sage-contributor-1.0",
                "arc-swift-contributor-1.0",
                "other/model"
            ]
        );
        assert_eq!(models.offered[1].context_window, 200_000);
        assert_eq!(models.offered[2].display_name, "other/model");
        assert_eq!(models.offered[2].product, None);
    }

    #[test]
    fn a_listing_without_products_or_limits_is_an_error() {
        let mut body = listing(None);
        for entry in body["data"].as_array_mut().unwrap() {
            entry.as_object_mut().unwrap().remove("product");
        }
        assert_eq!(
            LaunchModels::from_listing(&body).unwrap_err(),
            "no listed model has \"product\": \"arc-sage-1.0\""
        );
        let mut body = listing(None);
        body["data"][1]
            .as_object_mut()
            .unwrap()
            .remove("max_output_tokens");
        assert_eq!(
            LaunchModels::from_listing(&body).unwrap_err(),
            "arc-sage-1.0 has no max_output_tokens"
        );
    }

    #[test]
    fn prices_come_from_each_ids_card_row() {
        let mut models = LaunchModels::from_listing(&listing(Some("contributor"))).unwrap();
        models.apply_arc_prices(&json!({"card": {"prices": {
            "arc-sage-contributor-1.0": {
                "input": 0.5, "output": 2.5, "cache_read": 0.025, "cache_write": 0.625
            },
            "arc-sage-1.0": {"input": 9.0, "output": 9.0, "cache_read": 9.0, "cache_write": 9.0}
        }}}));
        assert_eq!(
            models.main_model().price,
            Some(ModelPrice {
                input: 0.5,
                output: 2.5,
                cache_read: 0.025,
                cache_write: 0.625
            })
        );
        // No row for this id: no price, never another id's.
        assert_eq!(models.offered[1].price, None);
    }

    /// A hosted dev context writing into `dir`.
    pub(crate) fn render_context(dir: &Path) -> RenderContext<'_> {
        RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: dir,
            conversation_id: ConversationIdFallback::new(None),
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: None,
            models: hosted_models(),
        }
    }

    /// Render `harness` with no override into a temp dir and return
    /// (dir, plan).
    pub(crate) fn render(harness: Harness) -> (PathBuf, Plan) {
        render_with(harness, ConversationIdFallback::new(None))
    }

    /// Render `harness` with the given fallback policy.
    pub(crate) fn render_with(
        harness: Harness,
        conversation_id: ConversationIdFallback,
    ) -> (PathBuf, Plan) {
        let home = temp_dir(harness.name());
        let dir = config_dir(&home, harness);
        let ctx = RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id,
            allow_empty_signature: pi::allow_empty_signature("dev", None),
            capabilities: None,
            harness_major_version: None,
            models: hosted_models(),
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
    fn config_dir_is_per_harness() {
        let home = Path::new("/home/u");
        assert_eq!(
            config_dir(home, Harness::Pi),
            PathBuf::from("/home/u/.rayline/harness/pi")
        );
    }

    #[test]
    fn every_harness_writes_only_under_its_harness_folder() {
        for harness in Harness::ALL {
            let (dir, _) = render(harness);
            let home = dir.ancestors().nth(3).unwrap();
            assert_only_harness_files(home, harness.name(), &[]);
            fs::remove_dir_all(home).unwrap();
        }
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

    #[cfg(windows)]
    #[test]
    fn write_files_restricts_the_dir_and_files_to_the_owner() {
        use rayline_local_router::windows_state::is_owner_only;
        let dir = temp_dir("write-acl").join("cfg");
        let files = [ConfigFile {
            name: "config.toml",
            contents: "secret = 1\n".to_owned(),
            overwrite: true,
            merged_from: None,
        }];
        write_files(&dir, &files).unwrap();
        assert!(is_owner_only(&dir).unwrap());
        assert!(is_owner_only(&dir.join("config.toml")).unwrap());
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
    fn conversation_id_fallback_is_off_by_default() {
        assert!(!ConversationIdFallback::new(None).enabled());
    }

    #[test]
    fn conversation_id_fallback_override() {
        for on in ["1", "true", "ON", " on "] {
            assert!(ConversationIdFallback::new(Some(on)).enabled(), "{on:?}");
        }
        for off in ["0", "false", "Off"] {
            assert!(!ConversationIdFallback::new(Some(off)).enabled(), "{off:?}");
        }
        // Unrecognized or empty values keep the default (off).
        assert!(!ConversationIdFallback::new(Some("")).enabled());
        assert!(!ConversationIdFallback::new(Some("maybe")).enabled());
    }

    #[test]
    fn native_session_keys_are_the_default() {
        let command = build_command(Path::new("/bin/true"), &Plan::default(), None, &[]);
        let conv = command
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(CONV_ID_ENV));
        assert_eq!(conv, Some((std::ffi::OsStr::new(CONV_ID_ENV), None)));
        for harness in Harness::ALL {
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
    fn forced_fallback_sends_the_conversation_header() {
        for harness in Harness::ALL {
            let (dir, plan) = render_with(harness, ConversationIdFallback::new(Some("1")));
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
                ConversationIdFallback::new(None),
                ConversationIdFallback::new(Some("1")),
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
        let vars = [ROUTER_KEY_ENV];
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

    #[test]
    fn launch_program_resolves_windows_launchers_only_on_windows() {
        let dir = temp_dir("launch-program");
        // npm installs an extensionless sh shim beside the `.cmd` launcher.
        fs::write(dir.join("codex"), "#!/bin/sh\n").unwrap();
        fs::write(dir.join("codex.cmd"), "@echo off\r\n").unwrap();
        let path = Some(std::env::join_paths([&dir]).unwrap());
        let program = launch_program("codex", path.clone());
        if cfg!(windows) {
            assert_eq!(program, dir.join("codex.cmd"));
        } else {
            assert_eq!(program, PathBuf::from("codex"));
        }
        assert_eq!(launch_program("pi", path), PathBuf::from("pi"));
        assert_eq!(launch_program("codex", None), PathBuf::from("codex"));
        let _ = fs::remove_dir_all(&dir);
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
