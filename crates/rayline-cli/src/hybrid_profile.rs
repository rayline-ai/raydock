//! Raydock's hybrid defaults, applied to a qualified execution-core
//! profile before `rld` starts the Go core.
//!
//! With no override, Raydock sends a policy explicitly through the core's
//! `execution_context_v2` Decisions contract; it never relies on an absent
//! policy. A session with a usable local deployment gets prefer-local
//! (`routing.policy: {"preset": "arc-policy.prefer-local.v1"}`, the service's
//! 0.65 threshold); one without gets the hybrid default
//! (`arc-policy.hybrid-default.v1`). The optional `hybrid.routing` block in settings.json maps
//! onto `routing.policy`:
//!
//! ```json
//! {"hybrid": {"routing": {"policy": "utility" | "prefer_local", "min_p_pass": 0.65,
//!                         "schedule": {"model": ["cache_boundaries", "user_messages"]}},
//!             "local_context_window": 98304,
//!             "local_registry_model": "qwen/qwen3.8-27b"}}
//! ```
//!
//! `hybrid.local_context_window` is the local model's window in tokens. When
//! absent, the window the runtime reports is used, else
//! [`DEFAULT_LOCAL_CONTEXT_WINDOW`]. A turn whose prompt does not fit hands off
//! to cloud.
//!
//! `hybrid.local_registry_model` declares which model the default profile's
//! local deployment serves, as the core's registry names it
//! ([`LOCAL_REGISTRY_MODEL`] when absent). The core refuses a model its
//! embedded registry does not know when it starts.
//!
//! A profile deployment marked `"from_local_model": true` takes its endpoint
//! and served model from the `local_model` entry (`raydock local custom …`).
//! Its runtime identity, protocol and thinking-control qualification still come
//! from the profile; a configured prompt counter verifies the served model.
//!
//! A profile that explicitly selects the older `current` or
//! `execution_context_v1` Decisions mode is left unchanged; an override block
//! cannot apply to it and is refused.

use std::path::Path;

use serde_json::{Map, Value, json};

pub(crate) const HYBRID_DEFAULT_PRESET: &str = "arc-policy.hybrid-default.v1";
pub(crate) const PREFER_LOCAL_PRESET: &str = "arc-policy.prefer-local.v1";
const API_MODE_V2: &str = "execution_context_v2";
const ROUTER_KEY_ENV: &str = "RAYLINE_ROUTER_API_KEY";
/// The ARC model the default profile asks for decisions. Swift decides at its
/// own fixed V (no `routing.v_value`), which picks cheap and so uses the local
/// model most.
pub(crate) const DECISIONS_MODEL: &str = "arc-swift-1.0";
pub(crate) const LOCAL_REGISTRY_MODEL: &str = "qwen/qwen3.8-27b";
/// The local model's default window: 96k tokens keeps Qwen3.8-27B's KV cache
/// within a laptop's memory, and a longer turn hands off to cloud.
pub(crate) const DEFAULT_LOCAL_CONTEXT_WINDOW: u64 = 98_304;
/// The bounds of `hybrid.local_context_window` (the cloud targets' window).
const LOCAL_CONTEXT_WINDOW_RANGE: std::ops::RangeInclusive<u64> = 2048..=1_000_000;
/// Context window of the default cloud targets, Claude Opus 5.5, Sonnet 5.5
/// and Haiku 5.5 (pricing snapshot 2026-10-16, `context_window_tokens`; Haiku
/// read from the dev and prod catalogs 2026-11-09, `context_length`).
const CLOUD_CONTEXT_WINDOW: u64 = 1_000_000;
/// Output ceiling of every hybrid deployment: the harness default
/// (`harness::MAX_OUTPUT_TOKENS`), not the larger hosted ARC ceiling. The core
/// advertises the smallest deployment cap as the session's, and enforces each
/// cap against the deployment the turn lands on; a 4096 cap cut long local
/// turns short before any edit landed (Mac hybrid test, 2026-10-27).
const MAX_OUTPUT_TOKENS: u64 = crate::harness::MAX_OUTPUT_TOKENS;
/// The default profile's cloud deployments, as `(id, wire_model,
/// registry_model, max_output_tokens)`.
///
/// `wire_model` is the router's own catalog entry, served by Anthropic
/// directly; `registry_model` is the same model as the core's thinking
/// registry knows it, whose measured cell supplies the action's controls.
///
/// The ceiling is each pinned model's own, not the harness default: a single
/// constant for all of them capped every cloud answer at a fraction of what
/// the model allows. These are the models this profile pins, so their
/// ceilings are declared here beside them (catalog figures, 2026-11-06 for
/// Opus and Sonnet and 2026-11-09 for Haiku, matching dev and prod), as
/// [`CLOUD_CONTEXT_WINDOW`] is. Override them per model with
/// `hybrid.cloud_output_tokens`.
///
/// Haiku 5.5 is the cheapest Claude tier, so a turn that leaves the local
/// model (too large for its window, or one Swift expects it to fail) has a
/// cloud target priced well below Sonnet. The core's registry qualifies it on
/// the same cell as Sonnet (`openrouter`, Messages, `default` base).
pub(crate) const CLOUD_MODELS: &[(&str, &str, &str, u64)] = &[
    (
        "cloud-opus",
        "claude-opus-5-5",
        "anthropic/claude-opus-5.5",
        128_000,
    ),
    (
        "cloud-sonnet",
        "claude-sonnet-5-5",
        "anthropic/claude-sonnet-5.5",
        128_000,
    ),
    (
        "cloud-haiku",
        "claude-haiku-5-5",
        "anthropic/claude-haiku-5.5",
        128_000,
    ),
];
/// Sanity bounds on a declared cloud output ceiling, not a check that the
/// provider allows it: the lower bound leaves room for output beside
/// Anthropic's 1024-token minimum thinking budget, and a larger budget needs
/// more than the bound alone guarantees; the upper is a policy ceiling well
/// above anything the catalog publishes, not proof any model serves it. The
/// core refuses a budget that reaches the admitted limit either way.
const CLOUD_OUTPUT_RANGE: std::ops::RangeInclusive<u64> = 1025..=1_000_000;

/// `hybrid.local_context_window` from settings, if the user set one.
pub(crate) fn configured_local_context_window(
    settings: Option<&Value>,
) -> Result<Option<u64>, String> {
    let value = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("local_context_window"));
    match value {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|window| LOCAL_CONTEXT_WINDOW_RANGE.contains(window))
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "settings hybrid.local_context_window must be a whole number of tokens from {} to {}",
                    LOCAL_CONTEXT_WINDOW_RANGE.start(),
                    LOCAL_CONTEXT_WINDOW_RANGE.end()
                )
            }),
    }
}

/// The local deployment's window and a note for the user.
///
/// `configured` is the user's window (`--local-context-window`, else settings
/// `hybrid.local_context_window`); `loaded` is the window the runtime reports
/// (llama.cpp `/props`, or `/v1/models` from vLLM or TensorFold). A runtime
/// cannot hold more than it was loaded with, so a smaller loaded window always
/// wins. With no configured window, a reported one is used as is, up to the
/// cloud targets' window: the default exists to bound memory for a runtime
/// whose window Raydock cannot see, and a runtime that reports one has
/// already allocated it. Before 0.4.3 the default applied even then, so
/// Claude Code on a non-Qwen model barely fit: its opening request (about
/// 73,000 tokens) plus the answer allowance nearly fills the default.
pub(crate) fn resolve_local_window(
    configured: Option<u64>,
    loaded: Option<u64>,
) -> (u64, Option<String>) {
    let max = *LOCAL_CONTEXT_WINDOW_RANGE.end();
    match (configured, loaded) {
        (Some(configured), Some(loaded)) if loaded < configured => (
            loaded,
            Some(format!(
                "Note: the local runtime is loaded with a {loaded}-token window, below the configured {configured}; using {loaded}. Load it with a {configured}-token window (llama.cpp `--ctx-size {configured}`, vLLM `--max-model-len {configured}`) to keep longer turns local."
            )),
        ),
        (Some(configured), Some(loaded)) if loaded > configured => (
            configured,
            Some(format!(
                "Note: the local runtime is loaded with a {loaded}-token window; Raydock uses the configured {configured}. Remove --local-context-window and settings hybrid.local_context_window to use the runtime's window."
            )),
        ),
        (Some(configured), Some(_)) => (configured, None),
        (None, Some(loaded)) if loaded > max => (
            max,
            Some(format!(
                "Note: the local runtime is loaded with a {loaded}-token window; Raydock uses {max}, the cloud targets' window."
            )),
        ),
        (None, Some(loaded)) => (loaded, None),
        (configured, None) => {
            let window = configured.unwrap_or(DEFAULT_LOCAL_CONTEXT_WINDOW);
            (
                window,
                Some(format!(
                    "Warning: Raydock cannot read this runtime's context window, so it assumes {window} tokens. Load the local model with at least that window (llama.cpp `--ctx-size {window}`, Ollama `OLLAMA_CONTEXT_LENGTH={window}`): a runtime loaded with less (Ollama's default is a few thousand tokens) may silently truncate longer prompts. Set a smaller window with --local-context-window if needed."
                )),
            )
        }
    }
}

/// The local deployment's output ceiling for a `window`-token window.
fn local_max_output(window: u64) -> u64 {
    (window / 4).clamp(1024, MAX_OUTPUT_TOKENS)
}

/// The smallest Claude Code opening request, as the core bounds it for a
/// model whose tokenizer it does not have (bytes-bounded), with its capacity
/// margin and without the output allowance: about 73,000 tokens with no MCP
/// servers or project instructions (prod, 2026-11-10: 105,061 required at a
/// 131,072 window, of which 32,000 is the answer: 73,061). Tools and
/// instructions add to it, so below this every task start goes to cloud and
/// a little above it most do. Qwen3.8-27B's is measured with its tokenizer and
/// is far smaller (about 39,000).
const CLAUDE_CODE_OPENING_TOKENS: u64 = 73_000;

/// A warning when `window` cannot hold even the smallest Claude Code opening
/// request plus the answer the core reserves for it (`capacity.go`: prompt +
/// output allowance + margin), so every Claude Code task start goes to
/// cloud. None for Qwen3.8-27B, whose prompts the core measures exactly.
pub(crate) fn small_window_warning(registry_model: &str, window: u64) -> Option<String> {
    if registry_model == LOCAL_REGISTRY_MODEL
        || window.saturating_sub(local_max_output(window)) >= CLAUDE_CODE_OPENING_TOKENS
    {
        return None;
    }
    let allowance = local_max_output(window);
    Some(format!(
        "Warning: the local window is {window} tokens. Claude Code's opening request needs about {CLAUDE_CODE_OPENING_TOKENS} tokens or more for {registry_model} (more with MCP servers or project instructions), plus {allowance} for the answer, so Claude Code tasks are likely to start on cloud. Load the local model with a 262144-token window (llama.cpp `--ctx-size 262144`, vLLM `--max-model-len 262144`) and set --local-context-window or settings hybrid.local_context_window to 262144 if you set one."
    ))
}

/// `hybrid.local_registry_model` from settings, or [`LOCAL_REGISTRY_MODEL`].
/// Raydock has no copy of the core's registry, so it checks only the shape
/// (`provider/model`); the core refuses a model its registry does not know.
pub(crate) fn local_registry_model(settings: Option<&Value>) -> Result<String, String> {
    let value = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("local_registry_model"));
    let Some(value) = value else {
        return Ok(LOCAL_REGISTRY_MODEL.to_owned());
    };
    value
        .as_str()
        .filter(|id| is_model_id(id))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            "settings hybrid.local_registry_model must be a registry model id \"provider/model\", such as \"z-ai/glm-5.3-flash\"".to_owned()
        })
}

/// The model Claude Code's auto-mode classifier goes to: `--classifier-model`,
/// else `hybrid.classifier_model` from settings, else Claude Haiku 5.5. It is
/// sent through the profile's cloud router, so any model that router serves
/// on Messages works; Raydock checks only that it is a router model id. Applies
/// with a profile file too, since the classifier is not a profile target.
pub(crate) fn classifier_model(
    flag: Option<&str>,
    settings: Option<&Value>,
) -> Result<String, String> {
    if let Some(flag) = flag {
        return if is_router_model_id(flag) {
            Ok(flag.to_owned())
        } else {
            Err(format!(
                "--classifier-model must be a router model id, such as \"{CLASSIFIER_MODEL}\""
            ))
        };
    }
    let value = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("classifier_model"));
    let Some(value) = value else {
        return Ok(CLASSIFIER_MODEL.to_owned());
    };
    value
        .as_str()
        .filter(|id| is_router_model_id(id))
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            format!("settings hybrid.classifier_model must be a router model id, such as \"{CLASSIFIER_MODEL}\"")
        })
}

/// Claude Code's auto-mode classifier model when the user names none.
pub(crate) const CLASSIFIER_MODEL: &str = "claude-haiku-5-5";

/// `provider/model`: one slash, both parts non-empty, no whitespace.
fn is_model_id(id: &str) -> bool {
    id.split_once('/').is_some_and(|(provider, model)| {
        !provider.is_empty() && !model.is_empty() && !model.contains('/')
    }) && !id.chars().any(char::is_whitespace)
}

/// A router catalog id: `provider/model`, or a bare id such as
/// `claude-haiku-5-5`. The router refuses one it does not serve.
fn is_router_model_id(id: &str) -> bool {
    is_model_id(id) || (!id.is_empty() && !id.contains('/') && !id.chars().any(char::is_whitespace))
}

/// The derived action's model and worker name for a registry id: the part
/// after the provider (`qwen/qwen3.8-27b` -> `qwen3.8-27b`).
fn registry_action_model(registry_model: &str) -> &str {
    registry_model
        .split_once('/')
        .map_or(registry_model, |(_, model)| model)
}

/// `hybrid.routing.local_price`: USD per million tokens for every leg of the
/// local model, 0 or within [1e-6, 1e6]; the service prices every other
/// candidate from its catalog.
fn local_price(settings: Option<&Value>) -> Result<f64, String> {
    let value = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("routing"))
        .and_then(|routing| routing.get("local_price"));
    match value {
        None => Ok(0.0),
        Some(value) => value
            .as_f64()
            .filter(|price| *price == 0.0 || (1e-6..=1e6).contains(price))
            .ok_or_else(|| "settings hybrid.routing.local_price must be 0 or a price in USD per million tokens (1e-6 to 1e6)".to_owned()),
    }
}

/// Each [`CLOUD_MODELS`] pin with the output ceiling to declare for it:
/// `hybrid.cloud_output_tokens` when settings set it, else the ceiling
/// declared beside the pin.
///
/// Nothing is read over the network for this. The profile pins these exact
/// models, so their ceilings are ours to state; discovering a number we chose
/// ourselves would only make a launch depend on a request that can fail.
///
/// The settings block is keyed by `wire_model` and is all-or-nothing: a
/// partial block is refused rather than mixed with the declared figures, so a
/// profile's ceilings have one source.
fn cloud_pins(
    settings: Option<&Value>,
) -> Result<Vec<(&'static str, &'static str, &'static str, u64)>, String> {
    let declared = || CLOUD_MODELS.to_vec();
    let Some(value) = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("cloud_output_tokens"))
    else {
        return Ok(declared());
    };
    let block = value.as_object().ok_or_else(|| {
        "settings hybrid.cloud_output_tokens must map each cloud model to its output ceiling"
            .to_owned()
    })?;
    let pinned = || {
        CLOUD_MODELS
            .iter()
            .map(|(_, wire, _, _)| *wire)
            .collect::<Vec<_>>()
            .join(", ")
    };
    // A key naming another model, such as an id an earlier release pinned,
    // is named in the error rather than reported as a missing ceiling.
    if let Some(other) = block
        .keys()
        .find(|key| !CLOUD_MODELS.iter().any(|(_, wire, _, _)| wire == key))
    {
        return Err(format!(
            "settings hybrid.cloud_output_tokens names {other}, which the default profile does not pin; key each output ceiling by a pinned model: {}",
            pinned()
        ));
    }
    if block.len() != CLOUD_MODELS.len() {
        return Err(format!(
            "settings hybrid.cloud_output_tokens needs an output ceiling for each model the default profile pins: {}",
            pinned()
        ));
    }
    CLOUD_MODELS
        .iter()
        .map(|(id, wire, registry, _)| {
            let ceiling = block
                .get(*wire)
                .and_then(Value::as_u64)
                .filter(|ceiling| CLOUD_OUTPUT_RANGE.contains(ceiling))
                .ok_or_else(|| {
                    format!(
                        "settings hybrid.cloud_output_tokens needs {wire} as a whole number of tokens from {} to {}",
                        CLOUD_OUTPUT_RANGE.start(),
                        CLOUD_OUTPUT_RANGE.end()
                    )
                })?;
            Ok((*id, *wire, *registry, ceiling))
        })
        .collect()
}

const SCHEDULE_CLASSES: &[&str] = &["cache_boundaries", "user_messages", "all_turns"];

/// Where the applied policy came from, for the start message.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PolicySource {
    /// Raydock's default: no override block and no profile policy.
    Default,
    /// The user's `hybrid.routing` block.
    Settings,
    /// The qualified profile's own `decisions.routing.policy`.
    Profile,
    /// The profile pins an older Decisions mode; no policy is sent.
    LegacyMode(String),
}

#[derive(Debug)]
pub(crate) struct Composed {
    pub config: Value,
    pub policy: PolicySource,
    pub preset: Option<String>,
    /// `(deployment id, url, model)` taken from `local_model`.
    pub local_model: Option<(String, String, String)>,
}

/// Which profile [`compose`] is given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProfileOrigin {
    /// Built by [`default_profile`]; `local_price` applies.
    Default,
    /// A qualified profile file (`--config`), which sets its own prices.
    File,
}

/// Compose the core configuration from a profile and the user's settings.
/// `base_dir` is the profile's directory: relative package and registry paths
/// are made absolute because the composed file lives in the state directory.
pub(crate) fn compose(
    mut profile: Value,
    settings: Option<&Value>,
    base_dir: &Path,
    origin: ProfileOrigin,
) -> Result<Composed, String> {
    if let Some(hybrid) = settings.and_then(|settings| settings.get("hybrid"))
        && !hybrid.is_object()
    {
        return Err("settings hybrid must be an object".to_owned());
    }
    let root = profile
        .as_object_mut()
        .ok_or("hybrid configuration must be a JSON object")?;
    for key in ["package_path", "registry_path"] {
        if let Some(Value::String(path)) = root.get(key)
            && !Path::new(path).is_absolute()
        {
            let absolute = base_dir.join(path);
            root.insert(key.to_owned(), json!(absolute.to_string_lossy()));
        }
    }
    let local_model = apply_local_model(root, settings)?;
    let priced = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("routing"))
        .and_then(|routing| routing.get("local_price"))
        .is_some();
    match origin {
        ProfileOrigin::File if priced => {
            return Err("settings hybrid.routing.local_price applies only to the default profile; a profile file (--config) sets its own prices, so remove local_price or start without --config".to_owned());
        }
        ProfileOrigin::File
            if settings
                .and_then(|settings| settings.get("hybrid"))
                .and_then(|hybrid| hybrid.get("local_registry_model"))
                .is_some() =>
        {
            return Err("settings hybrid.local_registry_model applies only to the default profile; a profile file (--config) sets its own registry_model, so remove local_registry_model or start without --config".to_owned());
        }
        ProfileOrigin::File => {}
        ProfileOrigin::Default => {
            local_price(settings)?;
        }
    }
    let block = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("routing"));
    if let Some(block) = block
        && !block.is_object()
    {
        return Err("settings hybrid.routing must be an object".to_owned());
    }
    // Prefer-local excludes a local target whose capacity is unknown. A
    // local deployment's capacity is counted by its llama.cpp
    // prompt_counter, or else sized by the core itself: it counts
    // Qwen3.8-27B with its embedded tokenizer and bounds any other model
    // by bytes (`caller_bounded`). Only a profile with no local
    // deployment, or a malformed counter, can never select local.
    let locals: Vec<&Value> = root
        .get("deployments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|deployment| deployment.get("kind").and_then(Value::as_str) == Some("local"))
        .collect();
    let no_local = locals.is_empty();
    let malformed_counter = locals.iter().any(|deployment| {
        deployment
            .get("prompt_counter")
            .is_some_and(|counter| !counter.is_object())
    });
    // With no policy chosen, a session that can use its local model prefers it.
    let default_preset = if !no_local && !malformed_counter {
        PREFER_LOCAL_PRESET
    } else {
        HYBRID_DEFAULT_PRESET
    };
    // A profile without api_mode keeps the core's own default (`current`), as
    // it did before v2, unless it is a package-less v2 profile.
    let derived = root.get("derived_actions").is_some();
    let decisions = root
        .get_mut("decisions")
        .and_then(Value::as_object_mut)
        .ok_or("hybrid configuration requires a decisions object")?;
    let mode = match decisions.get("api_mode").and_then(Value::as_str) {
        None if derived => None,
        None => Some("current".to_owned()),
        Some(API_MODE_V2) => None,
        Some(mode) => Some(mode.to_owned()),
    };
    match mode {
        None => {}
        Some(mode) => {
            if block.is_some() {
                return Err(format!(
                    "settings hybrid.routing cannot apply to a profile in Decisions api_mode {mode:?}; set decisions.api_mode to \"execution_context_v2\" in the profile"
                ));
            }
            return Ok(Composed {
                config: profile,
                policy: PolicySource::LegacyMode(mode),
                preset: None,
                local_model,
            });
        }
    }
    decisions.insert("api_mode".to_owned(), json!(API_MODE_V2));
    let routing = decisions
        .entry("routing")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or("decisions.routing must be an object")?;
    let existing = routing.get("policy").cloned();
    let (policy, source) = map_block(block, existing.as_ref(), default_preset)?;
    let preset = policy
        .get("preset")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    routing.insert("policy".to_owned(), Value::Object(policy));
    if preset.as_deref() == Some(PREFER_LOCAL_PRESET) {
        if no_local {
            return Err(
                "settings hybrid.routing.policy \"prefer_local\" needs a local deployment, and this session has none (local routing is off); use \"utility\" or fix the local model"
                    .to_owned(),
            );
        }
        if malformed_counter {
            return Err(
                "settings hybrid.routing.policy \"prefer_local\" needs each local deployment's prompt_counter to be an object; fix or remove it in the profile"
                    .to_owned(),
            );
        }
    }
    Ok(Composed {
        config: profile,
        policy: source,
        preset,
        local_model,
    })
}

/// The mapped `routing.policy` and its source.
type MappedRouting = (Map<String, Value>, PolicySource);

/// Map the override block onto `routing.policy`; `default_preset` applies
/// when neither the block nor the profile names a preset.
fn map_block(
    block: Option<&Value>,
    existing: Option<&Value>,
    default_preset: &str,
) -> Result<MappedRouting, String> {
    let block = block.and_then(Value::as_object);
    if let Some(block) = block {
        for key in block.keys() {
            if !matches!(
                key.as_str(),
                "policy" | "schedule" | "min_p_pass" | "local_price"
            ) {
                return Err(format!(
                    "settings hybrid.routing.{key} is not supported (policy, schedule, min_p_pass, local_price)"
                ));
            }
        }
    }
    let mut policy = match existing {
        Some(Value::Object(policy)) => policy.clone(),
        Some(_) => return Err("profile decisions.routing.policy must be an object".to_owned()),
        None => Map::new(),
    };
    let mut source = if existing.is_some() {
        PolicySource::Profile
    } else {
        PolicySource::Default
    };
    if let Some(block) = block
        && !block.is_empty()
    {
        source = PolicySource::Settings;
    }
    if let Some(selection) = block.and_then(|block| block.get("policy")) {
        let preset = match selection.as_str() {
            Some("utility") => HYBRID_DEFAULT_PRESET,
            Some("prefer_local") => PREFER_LOCAL_PRESET,
            _ => {
                return Err(
                    "settings hybrid.routing.policy must be \"utility\" or \"prefer_local\""
                        .to_owned(),
                );
            }
        };
        if preset != PREFER_LOCAL_PRESET
            && let Some(Value::Object(overrides)) = policy.get_mut("overrides")
        {
            // A selection threshold applies only to prefer-local; keep schedules.
            overrides.remove("selection");
            if overrides.is_empty() {
                policy.remove("overrides");
            }
        }
        policy.insert("preset".to_owned(), json!(preset));
    }
    if !policy.contains_key("preset") {
        policy.insert("preset".to_owned(), json!(default_preset));
    }
    if let Some(schedule) = block.and_then(|block| block.get("schedule")) {
        validate_schedule(schedule)?;
        let overrides = policy
            .entry("overrides")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("profile routing.policy.overrides must be an object")?;
        overrides.insert("schedule".to_owned(), schedule.clone());
    }
    if let Some(threshold) = block.and_then(|block| block.get("min_p_pass")) {
        let threshold = threshold
            .as_f64()
            .filter(|value| *value > 0.0 && *value < 1.0)
            .ok_or("settings hybrid.routing.min_p_pass must be a number in (0, 1)")?;
        if policy.get("preset").and_then(Value::as_str) != Some(PREFER_LOCAL_PRESET) {
            return Err(
                "settings hybrid.routing.min_p_pass applies only with policy \"prefer_local\""
                    .to_owned(),
            );
        }
        let overrides = policy
            .entry("overrides")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("profile routing.policy.overrides must be an object")?;
        overrides.insert("selection".to_owned(), json!({"min_p_pass": threshold}));
    }
    Ok((policy, source))
}

fn validate_schedule(schedule: &Value) -> Result<(), String> {
    let schedule = schedule
        .as_object()
        .ok_or("settings hybrid.routing.schedule must be an object")?;
    if schedule.is_empty() {
        return Err("settings hybrid.routing.schedule must set model or thinking".to_owned());
    }
    for (dimension, classes) in schedule {
        if dimension != "model" && dimension != "thinking" {
            return Err(format!(
                "settings hybrid.routing.schedule.{dimension} is not a dimension (model, thinking)"
            ));
        }
        let classes = classes
            .as_array()
            .filter(|classes| !classes.is_empty())
            .ok_or_else(|| {
                format!(
                    "settings hybrid.routing.schedule.{dimension} must list switch-point classes"
                )
            })?;
        let mut seen = Vec::new();
        for class in classes {
            let class = class
                .as_str()
                .filter(|class| SCHEDULE_CLASSES.contains(class));
            match class {
                Some(class) if !seen.contains(&class) => seen.push(class),
                _ => {
                    return Err(format!(
                        "settings hybrid.routing.schedule.{dimension} has an unknown or repeated class; use {}",
                        SCHEDULE_CLASSES.join(", ")
                    ));
                }
            }
        }
    }
    Ok(())
}

/// Fill the opted-in local deployment from `local_model` (custom mode).
fn apply_local_model(
    root: &mut Map<String, Value>,
    settings: Option<&Value>,
) -> Result<Option<(String, String, String)>, String> {
    let Some(deployments) = root.get_mut("deployments").and_then(Value::as_array_mut) else {
        return Ok(None);
    };
    let mut applied = None;
    for deployment in deployments.iter_mut() {
        let Some(deployment) = deployment.as_object_mut() else {
            continue;
        };
        match deployment.remove("from_local_model") {
            None | Some(Value::Bool(false)) => continue,
            Some(Value::Bool(true)) => {}
            Some(_) => return Err("deployment from_local_model must be a boolean".to_owned()),
        }
        if applied.is_some() {
            return Err("only one deployment can take its endpoint from local_model".to_owned());
        }
        if deployment.get("kind").and_then(Value::as_str) != Some("local") {
            return Err("from_local_model applies only to a local deployment".to_owned());
        }
        let (base_url, model) = custom_local_model(settings).ok_or(
            "this profile takes its local endpoint from local_model; run `raydock local custom --url <URL> --model <NAME>` first",
        )?;
        let path = match deployment.get("protocol").and_then(Value::as_str) {
            Some("openai.chat.v1") => "/v1/chat/completions",
            Some("anthropic.messages.v1") => "/v1/messages",
            Some("openai.responses.v1") => "/v1/responses",
            _ => return Err("local deployment protocol is missing or unsupported".to_owned()),
        };
        let url = format!(
            "{}{path}",
            crate::local_model::normalize_base_url(&base_url)
        );
        let id = deployment
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        deployment.insert("url".to_owned(), json!(url));
        deployment.insert("wire_model".to_owned(), json!(model));
        applied = Some((id, url, model));
    }
    Ok(applied)
}

pub(crate) fn custom_local_model(settings: Option<&Value>) -> Option<(String, String)> {
    let entry = settings?.get("local_model")?;
    let field = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    let base_url = field("base_url")?;
    let mode = entry.get("mode").and_then(Value::as_str);
    if mode.is_some_and(|mode| mode != "custom") {
        return None;
    }
    Some((base_url, field("model")?))
}

/// The default package-less profile: the local model from `local_model`
/// (Qwen3.8-27B unless `hybrid.local_registry_model` declares another), Opus,
/// Sonnet and Haiku through the signed-in router (exact model pins), decisions
/// from the
/// router's [`DECISIONS_MODEL`] with the hybrid default preset applied by
/// [`compose`]. The core derives each action's controls from its embedded
/// registry.
///
/// One cloud deployment and one action per [`CLOUD_MODELS`] pin, the
/// deployment carrying the ceiling [`cloud_pins`] resolves for it.
pub(crate) fn default_profile(
    router_url: &str,
    base_url: &str,
    model: &str,
    context_window: u64,
    cell: Option<&str>,
    settings: Option<&Value>,
) -> Result<Value, String> {
    // Each pin's ceiling: `hybrid.cloud_output_tokens` when set, else the one
    // declared beside the pin.
    let cloud_pins = cloud_pins(settings)?;
    // On-device inference costs the user about nothing per token: price the
    // local model at `local_price` (default 0, every leg, USD per million
    // tokens) and leave cloud models at their market prices.
    let local_price = local_price(settings)?;
    let registry_model = local_registry_model(settings)?;
    let action_model = registry_action_model(&registry_model);
    if context_window < 2048 {
        return Err(format!(
            "local context window {context_window} is too small"
        ));
    }
    let router = router_url.trim_end_matches('/');
    let local_url = format!(
        "{}/v1/chat/completions",
        crate::local_model::normalize_base_url(base_url)
    );
    let max_output = local_max_output(context_window);
    let cloud = |id: &str, wire: &str, registry: &str, max_output_tokens: u64| {
        json!({"id": id, "kind": "cloud", "cloud_transport": "model_pin", "url": format!("{router}/v1/messages"),
               "protocol": "anthropic.messages.v1", "wire_model": wire, "registry_model": registry,
               "registry_provider": "openrouter", "runtime_identity": "rayline-catalog-model-pin-v1",
               "max_output_tokens": max_output_tokens, "context_window": CLOUD_CONTEXT_WINDOW, "input_modalities": ["text"],
               // As on the local deployment: the core serves harnesses over
               // loopback, and its callers are mostly harnesses that emit
               // whatever their own defaults produce for a model their
               // catalog does not describe: Hermes sends a fixed 128,000 on
               // the Messages wire with no setting to lower it. Lowering such
               // a number serves the turn where refusing it only loses the
               // answer.
               //
               // The trade is real, not free. These ceilings are a catalog
               // snapshot or a user's `hybrid.cloud_output_tokens`, not a
               // figure the provider confirmed, so one that has drifted low
               // now shortens an answer instead of failing loudly. An SDK
               // caller stating its own allowance (`docs/hybrid-routing.md`)
               // is clamped the same way. The core logs the stated and sent
               // figures so a short answer can be traced to this.
               "clamp_output_allowance": true,
               "token_units": "", "auth_env": ROUTER_KEY_ENV})
    };
    let legs = json!({"input": local_price, "output": local_price, "cache_read": local_price, "cache_write": local_price});
    let mut prices = Map::new();
    prices.insert(registry_model.clone(), legs);
    // Routes API V1: the caller's V is `v_value`; the caller prices only the
    // local model and the service fills every other candidate from its
    // catalog, so there is no `exclusive` member and no digest to precompute
    // (the core verifies the service's digest from the echoed table).
    let mut decisions = json!({"api_mode": API_MODE_V2, "url": format!("{router}/v1/routes"),
                               "auth_env": ROUTER_KEY_ENV, "model": DECISIONS_MODEL, "result": "swift",
                               "routing": {"prices": prices}});
    if let Some(cell) = cell {
        decisions["cell_tag"] = json!(cell);
    }
    let mut deployments = vec![json!(
        {"id": "local", "kind": "local", "url": local_url, "protocol": "openai.chat.v1", "wire_model": model,
         "registry_model": registry_model, "registry_provider": "openai",
         "runtime_identity": format!("local_model:{model}"), "max_output_tokens": max_output,
         "context_window": context_window, "input_modalities": ["text"], "token_units": "",
         // The smallest cap of the four, so a caller asking for more is
         // lowered to it here most often. Every deployment clamps; see the
         // cloud closure above for the trade that makes.
         "clamp_output_allowance": true})];
    // One deployment and one derived action per pin, from the same list: the
    // core only checks that an action names a known deployment, so a pin with
    // a deployment and no action would be unroutable without any error.
    let mut derived_actions =
        vec![json!({"model": action_model, "worker": action_model, "deployment_id": "local"})];
    for (id, wire, registry, ceiling) in &cloud_pins {
        deployments.push(cloud(id, wire, registry, *ceiling));
        // The action names the model as the core's registry knows it, without
        // the registry's provider prefix.
        let action_model = registry_action_model(registry);
        derived_actions.push(json!({"model": action_model, "worker": action_model,
                                    "deployment_id": id, "bases": ["default"]}));
    }
    Ok(json!({
        "contract_version": 1,
        "allow_experimental_controls": true,
        "decisions": decisions,
        "deployments": deployments,
        "derived_actions": derived_actions
    }))
}

/// What the local runtime says it has loaded, read once when hybrid routing
/// starts: its per-slot window and which model it serves.
#[derive(Debug, PartialEq)]
pub(crate) struct LocalRuntime {
    /// The loaded context window: llama.cpp's per-slot `n_ctx`, or the
    /// served model's `max_model_len` / `context_length` from an
    /// OpenAI-compatible `/v1/models` (vLLM, TensorFold); None when the
    /// runtime does not report one (it is then the user's to state).
    pub window: Option<u64>,
    pub identity: LocalIdentity,
    /// The runtime refuses a prompt over its window rather than cutting it:
    /// a server whose `/v1/models` states its window (vLLM, TensorFold).
    /// Ollama and `mlx_lm.server` truncate and state none. llama.cpp is left
    /// out: run with `--context-shift` it cuts the prompt, and `/props` does
    /// not say whether it is on.
    pub refuses_over_window: bool,
}

/// Whether the loaded model is the one the default profile is built for:
/// its capacity counts use Qwen3.8-27B's own tokenizer and chat template.
#[derive(Debug, PartialEq)]
pub(crate) enum LocalIdentity {
    /// The runtime names Qwen3.8-27B.
    Match,
    /// The runtime names another model; local routing is refused.
    Mismatch(String),
    /// The runtime does not say what it serves (not llama.cpp or Ollama).
    Unknown,
}

impl LocalIdentity {
    /// A wrong model never serves a turn: its window and token counts would
    /// be wrong, and nothing on the chat path would show it.
    pub(crate) fn refuses_local(&self) -> bool {
        matches!(self, Self::Mismatch(_))
    }

    /// What `raydock hybrid start` tells the user, if anything.
    pub(crate) fn start_message(&self, base_url: &str) -> Option<String> {
        match self {
            Self::Match => None,
            Self::Mismatch(reported) => Some(format!(
                "Error: the local runtime at {base_url} serves {reported}, not {LOCAL_MODEL_NAME}. Local routing is off for this session: every turn goes to cloud. Serve {LOCAL_MODEL_NAME} (Ollama `qwen3.8:27b`, or a Qwen3.8-27B GGUF in llama.cpp), then run `raydock hybrid start` again."
            )),
            Self::Unknown => Some(format!(
                "Warning: Rayline cannot read which model the runtime at {base_url} serves (llama.cpp and Ollama report it); it assumes {LOCAL_MODEL_NAME}."
            )),
        }
    }
}

/// Whether local routing is refused for this session, and what to tell the
/// user. The identity check knows only Qwen3.8-27B: a model declared in
/// `hybrid.local_registry_model` is the user's statement and is not checked.
pub(crate) fn local_verdict(
    registry_model: &str,
    identity: &LocalIdentity,
    base_url: &str,
) -> (bool, Option<String>) {
    if registry_model == LOCAL_REGISTRY_MODEL {
        (identity.refuses_local(), identity.start_message(base_url))
    } else {
        (
            false,
            Some(format!(
                "Local model: {registry_model} (declared in settings hybrid.local_registry_model; Raydock does not check what {base_url} serves)"
            )),
        )
    }
}

/// Reads what the runtime serves: llama.cpp's `/props` (its loaded file and
/// window), else `/v1/models` for the window and Ollama's `/api/show` for the
/// configured model (the GGUF's `general.*` metadata). A user-chosen name such as a llama.cpp alias or the
/// request's own model name is not evidence: the runtimes echo it back on
/// every chat response, so the response cannot confirm the model either.
pub(crate) async fn read_local_runtime(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> LocalRuntime {
    let base = crate::local_model::normalize_base_url(base_url);
    async fn answer(request: reqwest::RequestBuilder) -> Option<Value> {
        request
            .send()
            .await
            .ok()?
            .error_for_status()
            .ok()?
            .json()
            .await
            .ok()
    }
    if let Some(props) = answer(client.get(format!("{base}/props"))).await {
        let window = props
            .get("default_generation_settings")
            .and_then(|settings| settings.get("n_ctx"))
            .or_else(|| props.get("n_ctx"))
            .and_then(Value::as_u64)
            .filter(|window| *window > 0);
        return LocalRuntime {
            window,
            identity: llamacpp_identity(&props),
            refuses_over_window: false,
        };
    }
    let window = answer(client.get(format!("{base}/v1/models")))
        .await
        .and_then(|models| served_window(&models, model));
    let show = answer(
        client
            .post(format!("{base}/api/show"))
            .json(&json!({"model": model})),
    )
    .await;
    LocalRuntime {
        window,
        identity: show
            .as_ref()
            .map_or(LocalIdentity::Unknown, ollama_identity),
        refuses_over_window: window.is_some(),
    }
}

/// Marks the local deployment `context_overflow: "refuses"`: when every
/// target falls short on an estimate, the core then sends the turn to this
/// runtime and lets its own context refusal decide, instead of answering the
/// compaction 400 for a prompt that may well fit.
pub(crate) fn mark_local_refuses_over_window(profile: &mut Value) {
    let deployments = profile.get_mut("deployments").and_then(Value::as_array_mut);
    for deployment in deployments.into_iter().flatten() {
        if deployment.get("kind").and_then(Value::as_str) == Some("local") {
            deployment["context_overflow"] = json!("refuses");
        }
    }
}

/// The served model's window from an OpenAI-compatible `/v1/models` list:
/// the entry whose `id` is the configured model, else the only entry. vLLM
/// reports `max_model_len`; TensorFold reports it and `context_length`.
fn served_window(models: &Value, model: &str) -> Option<u64> {
    let entries = models.get("data")?.as_array()?;
    let entry = entries
        .iter()
        .find(|entry| entry.get("id").and_then(Value::as_str) == Some(model))
        .or(match entries.as_slice() {
            [only] => Some(only),
            _ => None,
        })?;
    ["max_model_len", "context_length"].iter().find_map(|key| {
        entry
            .get(*key)
            .and_then(Value::as_u64)
            .filter(|window| *window > 0)
    })
}

/// A reported name, lower case, letters and digits only.
fn normalized(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|char| char.to_ascii_lowercase())
        .collect()
}

/// [`LOCAL_MODEL_NAME`] as [`normalized`] spells it, and the `28B` a
/// converter may compute from its 27.8B parameters (no Qwen3.8-28B exists).
const LOCAL_MODEL_KEYS: &[&str] = &["qwen3827b", "qwen3828b"];
/// Model families whose name in a file or GGUF identifies *a* model: such a
/// name without [`LOCAL_MODEL_KEYS`] is another model. A name with none of
/// them (`model.gguf`, `ggml-model-Q4_K_M.gguf`) identifies nothing.
const MODEL_FAMILIES: &[&str] = &[
    "qwen",
    "llama",
    "gemma",
    "mistral",
    "mixtral",
    "ministral",
    "devstral",
    "codestral",
    "phi",
    "deepseek",
    "glm",
    "gpt",
    "granite",
    "kimi",
    "minimax",
    "olmo",
    "smollm",
    "falcon",
    "nemotron",
    "hermes",
    "starcoder",
    "exaone",
    "internlm",
    "baichuan",
];
/// The model the default profile is built for (Hugging Face `Qwen/Qwen3.8-27B`,
/// 27.8B parameters with its vision encoder; Ollama `qwen3.8:27b`).
pub(crate) const LOCAL_MODEL_NAME: &str = "Qwen3.8-27B";

/// What one reported name says: Qwen3.8-27B, another model, or nothing.
fn name_identity(name: &str, reported: &str) -> LocalIdentity {
    let name = normalized(name);
    if LOCAL_MODEL_KEYS.iter().any(|key| name.contains(key)) {
        LocalIdentity::Match
    } else if MODEL_FAMILIES.iter().any(|family| name.contains(family)) {
        LocalIdentity::Mismatch(reported.to_owned())
    } else {
        LocalIdentity::Unknown
    }
}

/// llama.cpp names its loaded model by file: `/props` `model_path`, whose
/// file name carries the model's name for every published GGUF
/// (`Qwen3.8-27B-UD-Q4_K_M.gguf`, `unsloth_Qwen3.8-27B-GGUF_…`). The file
/// name decides when it names a model; else its folder's name does. The
/// `--alias` llama.cpp may report instead is the user's label, not evidence.
fn llamacpp_identity(props: &Value) -> LocalIdentity {
    let Some(reported) = props
        .get("model_path")
        .and_then(Value::as_str)
        .filter(|path| !path.trim().is_empty())
    else {
        return LocalIdentity::Unknown;
    };
    let path = Path::new(reported);
    [path.file_name(), path.parent().and_then(Path::file_name)]
        .into_iter()
        .flatten()
        .map(|name| name_identity(&name.to_string_lossy(), reported))
        .find(|identity| *identity != LocalIdentity::Unknown)
        .unwrap_or(LocalIdentity::Unknown)
}

/// Ollama reports the loaded GGUF's own metadata: `general.basename` with
/// `general.size_label` (`Qwen3.8` + `27B` in `qwen3.8:27b`), or
/// `general.name`. Either naming Qwen3.8-27B is a match.
fn ollama_identity(show: &Value) -> LocalIdentity {
    let Some(info) = show.get("model_info").and_then(Value::as_object) else {
        return LocalIdentity::Unknown;
    };
    let field = |key: &str| info.get(key).and_then(Value::as_str).unwrap_or_default();
    let basename = format!(
        "{} {}",
        field("general.basename"),
        field("general.size_label")
    );
    let name = field("general.name");
    // Each verdict reports the name it was read from.
    let verdicts = [
        name_identity(&basename, basename.trim()),
        name_identity(name, name),
    ];
    if verdicts.contains(&LocalIdentity::Match) {
        LocalIdentity::Match
    } else {
        verdicts
            .into_iter()
            .find(|identity| *identity != LocalIdentity::Unknown)
            .unwrap_or(LocalIdentity::Unknown)
    }
}

/// Removes the local deployment and its action from the default profile:
/// every turn then goes to cloud. Caller prices keep the local entry; a
/// price for an unoffered model selects nothing.
pub(crate) fn without_local(profile: &mut Value) {
    let mut local_ids = Vec::new();
    if let Some(deployments) = profile.get_mut("deployments").and_then(Value::as_array_mut) {
        deployments.retain(|deployment| {
            let local = deployment.get("kind").and_then(Value::as_str) == Some("local");
            if local && let Some(id) = deployment.get("id").and_then(Value::as_str) {
                local_ids.push(id.to_owned());
            }
            !local
        });
    }
    if let Some(actions) = profile
        .get_mut("derived_actions")
        .and_then(Value::as_array_mut)
    {
        actions.retain(|action| {
            action
                .get("deployment_id")
                .and_then(Value::as_str)
                .is_none_or(|id| !local_ids.iter().any(|local| local == id))
        });
    }
}

/// Saved Rayline API keys for credential references whose endpoint is the
/// same host as a signed-in environment's router. A key is only ever offered
/// to the host it was issued for; an explicit launch environment wins.
pub(crate) fn saved_credentials(
    config: &Value,
    home: &Path,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut references = Vec::new();
    // Every endpoint naming a variable, local ones included: a supplied key
    // must reach no other host through a shared variable name.
    let mut all_references = Vec::new();
    if let Some(decisions) = config.get("decisions") {
        references.push((decisions.get("auth_env"), decisions.get("url")));
        all_references.push((decisions.get("auth_env"), decisions.get("url")));
    }
    for deployment in config
        .get("deployments")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if deployment.get("kind").and_then(Value::as_str) == Some("cloud") {
            references.push((deployment.get("auth_env"), deployment.get("url")));
        }
        all_references.push((deployment.get("auth_env"), deployment.get("url")));
    }
    let mut environments = vec![crate::status::PROD_ENV.to_owned()];
    if let Some(configured) = crate::status::read_settings(home)
        .as_ref()
        .and_then(|settings| settings.get("environments"))
        .and_then(Value::as_object)
    {
        environments.extend(configured.keys().cloned());
    }
    let mut result: Vec<(String, String)> = Vec::new();
    for &(name, url) in &references {
        let (Some(name), Some(url)) = (name.and_then(Value::as_str), url.and_then(Value::as_str))
        else {
            continue;
        };
        if environment(name).is_some_and(|value| !value.is_empty()) {
            continue;
        }
        let Some(host) = https_host(url) else {
            continue;
        };
        for env_name in &environments {
            let Ok(hosted) = crate::status::resolve_hosted_environment(env_name, Some(home)) else {
                continue;
            };
            if https_host(&hosted.router_url).as_deref() != Some(host.as_str()) {
                continue;
            }
            let Some(key) = crate::status::load_claude_key_from_home(env_name, home) else {
                continue;
            };
            match result.iter().find(|(existing, _)| existing == name) {
                Some((_, existing)) if *existing != key => {
                    return Err(format!(
                        "credential reference {name} matches more than one saved Rayline key; set it explicitly"
                    ));
                }
                Some(_) => {}
                None => result.push((name.to_owned(), key)),
            }
        }
    }
    // The core reads one variable per name: every endpoint that names it must
    // be the host the saved key was issued for, or the key is not supplied.
    for (name, _) in &result {
        let hosts: Vec<_> = all_references
            .iter()
            .filter(|(reference, _)| reference.and_then(Value::as_str) == Some(name.as_str()))
            .map(|(_, url)| url.and_then(Value::as_str).and_then(https_host))
            .collect();
        if hosts.iter().any(|host| host.is_none() || *host != hosts[0]) {
            return Err(format!(
                "credential reference {name} is shared by endpoints on different hosts; set it explicitly"
            ));
        }
    }
    Ok(result)
}

/// The HTTPS origin authority (host and effective port) of an endpoint.
fn https_host(url: &str) -> Option<String> {
    rayline_local_router::decisions_url::https_authority(url)
}

/// Router URLs of every configured Rayline environment: the built-in `prod`
/// plus each valid `environments` entry in settings.json.
pub(crate) fn configured_router_urls(home: &Path) -> Vec<String> {
    let mut names = vec![crate::status::PROD_ENV.to_owned()];
    if let Some(configured) = crate::status::read_settings(home)
        .as_ref()
        .and_then(|settings| settings.get("environments"))
        .and_then(Value::as_object)
    {
        names.extend(configured.keys().cloned());
    }
    names
        .iter()
        .filter_map(|name| crate::status::resolve_hosted_environment(name, Some(home)).ok())
        // Only the origin: a router URL's path, query or userinfo never
        // reaches `rld`'s arguments, and one with credentials is dropped.
        .filter_map(|hosted| https_host(&hosted.router_url))
        .map(|authority| format!("https://{authority}"))
        .collect()
}

/// The configured environment whose router serves `url` (same HTTPS host
/// and port), e.g. to name the right `raydock --env <env> auth login`.
pub(crate) fn environment_for_url(home: &Path, url: &str) -> Option<String> {
    let target = https_host(url)?;
    let mut names = vec![crate::status::PROD_ENV.to_owned()];
    if let Some(configured) = crate::status::read_settings(home)
        .as_ref()
        .and_then(|settings| settings.get("environments"))
        .and_then(Value::as_object)
    {
        names.extend(configured.keys().cloned());
    }
    names.into_iter().find(|name| {
        crate::status::resolve_hosted_environment(name, Some(home))
            .ok()
            .and_then(|hosted| https_host(&hosted.router_url))
            .is_some_and(|authority| authority == target)
    })
}

/// Refuse a configuration whose `decisions.url` is not a Rayline router of a
/// configured environment (HTTPS, exact host and port) or a loopback address.
/// `rld` repeats this check on the file it hands the core.
pub(crate) fn check_decisions_url(config: &Value, router_urls: &[String]) -> Result<(), String> {
    rayline_local_router::decisions_url::check_config(
        config,
        router_urls.iter().map(String::as_str),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Value {
        json!({
            "contract_version": 1,
            "package_path": "package.json",
            "decisions": {"api_mode": "execution_context_v2", "url": "https://api-dev.rayline.ai/v1/routes", "auth_env": "RAYLINE_ROUTER_API_KEY",
                          "model": "arc-1.0", "result": "top", "routing": {}},
            "deployments": [
                {"id": "local-qwen", "kind": "local", "protocol": "openai.chat.v1", "from_local_model": true,
                 "url": "http://127.0.0.1:1/v1/chat/completions", "wire_model": "placeholder"},
                {"id": "cloud", "kind": "cloud", "url": "https://api-dev.rayline.ai/v1/messages", "auth_env": "RAYLINE_ROUTER_API_KEY"}
            ]
        })
    }

    fn settings(routing: Option<Value>) -> Value {
        let mut settings = json!({"local_model": {"mode": "custom", "base_url": "http://127.0.0.1:11434/v1", "model": "qwen3.8:27b"}});
        if let Some(routing) = routing {
            settings["hybrid"] = json!({"routing": routing});
        }
        settings
    }

    #[test]
    fn no_override_prefers_the_local_model_explicitly() {
        let composed = compose(
            profile(),
            Some(&settings(None)),
            Path::new("/profiles"),
            ProfileOrigin::File,
        )
        .unwrap();
        let decisions = &composed.config["decisions"];
        assert_eq!(decisions["api_mode"], "execution_context_v2");
        assert_eq!(
            decisions["routing"]["policy"],
            json!({"preset": PREFER_LOCAL_PRESET})
        );
        assert_eq!(decisions["result"], "top");
        assert!(decisions["routing"].get("v_value").is_none());
        assert_eq!(composed.policy, PolicySource::Default);
        // The package path is joined onto the profile folder with the
        // platform's separator.
        assert_eq!(
            composed.config["package_path"],
            Path::new("/profiles")
                .join("package.json")
                .to_string_lossy()
                .as_ref()
        );
    }

    #[test]
    fn override_block_maps_to_policy_and_schedule() {
        let block = json!({"policy": "utility", "schedule": {"model": ["cache_boundaries", "user_messages"], "thinking": ["all_turns"]}});
        let composed = compose(
            profile(),
            Some(&settings(Some(block))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        let decisions = &composed.config["decisions"];
        assert_eq!(
            decisions["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET, "overrides": {"schedule": {"model": ["cache_boundaries", "user_messages"], "thinking": ["all_turns"]}}})
        );
        assert_eq!(decisions["result"], "top");
        assert!(decisions["routing"].get("v_value").is_none());
        assert_eq!(composed.policy, PolicySource::Settings);
    }

    #[test]
    fn local_context_window_is_unset_unless_configured() {
        assert_eq!(configured_local_context_window(None).unwrap(), None);
        assert_eq!(
            configured_local_context_window(Some(&json!({"hybrid": {}}))).unwrap(),
            None
        );
        let raised = json!({"hybrid": {"local_context_window": 131_072}});
        assert_eq!(
            configured_local_context_window(Some(&raised)).unwrap(),
            Some(131_072)
        );
        for bad in [json!(1024), json!(2_000_000), json!("96k"), json!(-1)] {
            let settings = json!({"hybrid": {"local_context_window": bad}});
            assert!(
                configured_local_context_window(Some(&settings)).is_err(),
                "{bad}"
            );
        }
        // The cloud targets carry their 1M window.
        let profile = default_profile(
            "https://api-dev.rayline.ai",
            "http://127.0.0.1:11434/v1",
            "qwen3.8:27b",
            98_304,
            None,
            None,
        )
        .unwrap();
        let windows: Vec<_> = profile["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|deployment| {
                (
                    deployment["id"].clone(),
                    deployment["context_window"].clone(),
                )
            })
            .collect();
        assert_eq!(
            windows,
            [
                (json!("local"), json!(98_304)),
                (json!("cloud-opus"), json!(1_000_000)),
                (json!("cloud-sonnet"), json!(1_000_000)),
                (json!("cloud-haiku"), json!(1_000_000))
            ]
        );
    }

    #[test]
    fn an_unconfigured_window_follows_the_runtime_and_a_configured_one_is_capped_by_it() {
        // Unconfigured: the runtime's reported window, so Claude Code's
        // opening request (about 73,000 tokens plus the answer allowance)
        // fits a 262k GLM server with room to grow.
        assert_eq!(resolve_local_window(None, Some(262_144)), (262_144, None));
        assert_eq!(resolve_local_window(None, Some(32_768)), (32_768, None));
        let (window, note) = resolve_local_window(None, Some(1_048_576));
        assert_eq!(window, 1_000_000);
        assert!(note.is_some());
        // Unconfigured and unreadable (Ollama, MLX): the default, with a warning.
        let (window, note) = resolve_local_window(None, None);
        assert_eq!(window, DEFAULT_LOCAL_CONTEXT_WINDOW);
        assert!(note.is_some_and(|note| note.starts_with("Warning")));
        // Configured: kept, but never above what the runtime holds.
        assert_eq!(
            resolve_local_window(Some(131_072), Some(131_072)),
            (131_072, None)
        );
        assert_eq!(
            resolve_local_window(Some(131_072), Some(262_144)).0,
            131_072
        );
        assert_eq!(resolve_local_window(Some(131_072), Some(65_536)).0, 65_536);
        assert_eq!(resolve_local_window(Some(131_072), None).0, 131_072);
    }

    #[test]
    fn a_window_too_small_for_claude_codes_opening_request_is_warned() {
        let glm = "z-ai/glm-5.3-flash";
        let warning =
            small_window_warning(glm, 65_536).expect("64k cannot hold the opening request");
        assert!(warning.contains("65536"), "{warning}");
        assert!(warning.contains("plus 16384 for the answer"), "{warning}");
        assert!(warning.contains("--ctx-size 262144"), "{warning}");
        // The edge: 97,333 reserves 24,333 for the answer, leaving 73,000.
        assert!(small_window_warning(glm, 97_332).is_some());
        assert_eq!(small_window_warning(glm, 97_333), None);
        // 98,304 holds the measured opening request (97,637 required), and
        // 131,072 ran Claude Code locally on prod (105,061 required).
        assert_eq!(small_window_warning(glm, 98_304), None);
        assert_eq!(small_window_warning(glm, 131_072), None);
        assert_eq!(small_window_warning(glm, 262_144), None);
        // Qwen's prompts are measured with its tokenizer.
        assert_eq!(small_window_warning(LOCAL_REGISTRY_MODEL, 65_536), None);
    }

    #[test]
    fn default_profile_needs_only_the_router_and_local_model() {
        let profile = default_profile(
            "https://api-dev.rayline.ai/",
            "http://127.0.0.1:11434/v1",
            "qwen3.8:27b",
            16384,
            None,
            None,
        )
        .unwrap();
        let composed = compose(
            profile,
            Some(&settings(None)),
            Path::new("/state"),
            ProfileOrigin::Default,
        )
        .unwrap();
        let config = &composed.config;
        assert_eq!(
            config["decisions"]["url"],
            "https://api-dev.rayline.ai/v1/routes"
        );
        assert_eq!(config["decisions"]["model"], "arc-swift-1.0");
        assert_eq!(
            config["decisions"]["routing"]["policy"],
            json!({"preset": PREFER_LOCAL_PRESET})
        );
        assert!(config.get("package_path").is_none() && config.get("bindings").is_none());
        let local = &config["deployments"][0];
        assert_eq!(local["url"], "http://127.0.0.1:11434/v1/chat/completions");
        assert_eq!(local["wire_model"], "qwen3.8:27b");
        assert_eq!(local["context_window"], 16384);
        assert_eq!(local["max_output_tokens"], 4096);
        // Every deployment lowers an over-cap allowance to its own ceiling:
        // the harness that sent it could not state a smaller one.
        assert_eq!(local["clamp_output_allowance"], true);
        // Each cloud deployment declares the ceiling it was given, its own
        // model's, not one constant for all of them.
        for (cloud, (_, _, _, ceiling)) in config["deployments"].as_array().unwrap()[1..]
            .iter()
            .zip(CLOUD_MODELS)
        {
            assert_eq!(cloud["max_output_tokens"], *ceiling);
            assert_eq!(cloud["clamp_output_allowance"], true);
        }
        // One action for the local model and one per cloud pin.
        assert_eq!(
            config["derived_actions"].as_array().unwrap().len(),
            1 + CLOUD_MODELS.len()
        );
        assert_eq!(
            config["deployments"][1]["url"],
            "https://api-dev.rayline.ai/v1/messages"
        );
        // The saved key reaches only the router host every endpoint names.
        let home =
            std::env::temp_dir().join(format!("rayline-hybrid-default-{}", std::process::id()));
        let dir = home.join(".config").join(crate::CONFIG_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"environments":{"dev":{"router_url":"https://api-dev.rayline.ai","cli_auth_url":"https://dev.example/cli-auth"}}}"#).unwrap();
        std::fs::write(
            dir.join("credentials.json"),
            r#"{"router_keys":{"dev":{"apiKey":"rlk-dev-synthetic"}}}"#,
        )
        .unwrap();
        let found = saved_credentials(config, &home, |_| None).unwrap();
        assert_eq!(
            found,
            vec![(
                "RAYLINE_ROUTER_API_KEY".to_owned(),
                "rlk-dev-synthetic".to_owned()
            )]
        );
        let _ = std::fs::remove_dir_all(&home);
        assert!(default_profile("https://r", "http://l", "m", 1000, None, None).is_err());
    }

    /// Each cloud deployment declares its own model's ceiling, and settings
    /// may replace them per model. One constant for all of them capped every
    /// cloud answer at the smallest arm's figure.
    #[test]
    fn each_cloud_deployment_declares_its_own_output_ceiling() {
        let built = |settings: Option<&Value>| {
            let profile = default_profile(
                "https://r.example",
                "http://127.0.0.1:11434/v1",
                "q",
                98304,
                None,
                settings,
            )
            .unwrap();
            let ceilings: Vec<_> = profile["deployments"].as_array().unwrap()[1..]
                .iter()
                .map(|cloud| cloud["max_output_tokens"].as_u64().unwrap())
                .collect();
            (
                ceilings,
                profile["deployments"][0]["max_output_tokens"].clone(),
            )
        };
        // Each pin's declared ceiling.
        let declared: Vec<_> = CLOUD_MODELS.iter().map(|(_, _, _, c)| *c).collect();
        let (ceilings, local) = built(None);
        assert_eq!(ceilings, declared);
        // The local cap keeps its own rule, a quarter of the local window.
        assert_eq!(local, 24_576);
        // Settings replace them per model.
        let mut overridden = settings(None);
        overridden["hybrid"]["cloud_output_tokens"] = json!({
            "claude-opus-5-5": 96_000,
            "claude-sonnet-5-5": 64_000,
            "claude-haiku-5-5": 32_000
        });
        assert_eq!(built(Some(&overridden)).0, vec![96_000, 64_000, 32_000]);
    }

    /// `hybrid.cloud_output_tokens` overrides the declared ceilings, keyed by
    /// the pinned `wire_model`, and is refused unless it names every pinned
    /// model with a usable figure.
    #[test]
    fn settings_may_declare_every_cloud_ceiling() {
        let with = |block: Value| {
            let mut settings = settings(None);
            settings["hybrid"]["cloud_output_tokens"] = block;
            settings
        };
        // The range's own bounds are accepted, so the error names a figure
        // genuinely outside it.
        let bounds = with(json!({
            "claude-opus-5-5": CLOUD_OUTPUT_RANGE.start(),
            "claude-sonnet-5-5": CLOUD_OUTPUT_RANGE.end(),
            "claude-haiku-5-5": CLOUD_OUTPUT_RANGE.start()
        }));
        assert_eq!(
            cloud_pins(Some(&bounds)).map(|pins| pins.iter().map(|(_, _, _, c)| *c).collect()),
            Ok(vec![
                *CLOUD_OUTPUT_RANGE.start(),
                *CLOUD_OUTPUT_RANGE.end(),
                *CLOUD_OUTPUT_RANGE.start()
            ])
        );
        let each = with(json!({
            "claude-opus-5-5": 96_000,
            "claude-sonnet-5-5": 64_000,
            "claude-haiku-5-5": 32_000
        }));
        assert_eq!(
            cloud_pins(Some(&each)).map(|pins| pins.iter().map(|(_, _, _, c)| *c).collect()),
            Ok(vec![96_000, 64_000, 32_000])
        );
        // Absent: each pin's declared ceiling.
        let declared = CLOUD_MODELS
            .iter()
            .map(|(_, _, _, c)| *c)
            .collect::<Vec<_>>();
        for absent in [Some(&settings(None)), None] {
            assert_eq!(
                cloud_pins(absent)
                    .map(|pins| pins.iter().map(|(_, _, _, c)| *c).collect::<Vec<_>>()),
                Ok(declared.clone())
            );
        }
        for refused in [
            // One model named, so the others would have to come from elsewhere.
            json!({"claude-opus-5-5": 96_000}),
            // The two pins of an earlier release: a block written before Haiku
            // was pinned needs its ceiling too, rather than mixing one
            // declared figure with two of the user's.
            json!({
                "claude-opus-5-5": 96_000,
                "claude-sonnet-5-5": 64_000
            }),
            // A model this profile does not pin.
            json!({
                "claude-opus-5-5": 96_000,
                "claude-sonnet-5-5": 64_000,
                "claude-haiku-5-5": 32_000,
                "claude-fable-5-1": 64_000
            }),
            // The right number of models, one of them not pinned: the count
            // alone cannot catch this, so each pin is looked up by name.
            json!({
                "claude-opus-5-5": 96_000,
                "claude-sonnet-5-5": 64_000,
                "claude-fable-5-1": 64_000
            }),
            // Out of range, and not a whole number of tokens.
            json!({"claude-opus-5-5": 1024, "claude-sonnet-5-5": 64_000, "claude-haiku-5-5": 32_000}),
            json!({"claude-opus-5-5": 96_000, "claude-sonnet-5-5": 0, "claude-haiku-5-5": 32_000}),
            json!({"claude-opus-5-5": 96_000, "claude-sonnet-5-5": 64_000, "claude-haiku-5-5": "32000"}),
            json!({"claude-opus-5-5": 96_000, "claude-sonnet-5-5": 64_000, "claude-haiku-5-5": 1_000_001}),
            json!([96_000, 64_000, 32_000]),
        ] {
            assert!(
                cloud_pins(Some(&with(refused.clone()))).is_err(),
                "{refused} was accepted"
            );
        }
    }

    /// Every cloud deployment has a derived action naming it. The core only
    /// checks that an action names a known deployment, so a pin that gained a
    /// deployment without an action would be unroutable and silent.
    #[test]
    fn every_cloud_deployment_has_a_derived_action() {
        let profile = default_profile(
            "https://r.example",
            "http://127.0.0.1:11434/v1",
            "q",
            98304,
            None,
            None,
        )
        .unwrap();
        let routed: Vec<_> = profile["derived_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|action| action["deployment_id"].as_str().unwrap())
            .collect();
        for deployment in profile["deployments"].as_array().unwrap() {
            let id = deployment["id"].as_str().unwrap();
            assert!(routed.contains(&id), "{id} has no derived action");
        }
        // Each cloud action names the model the core's registry knows, not
        // the wire ref's full pin. Found by deployment, not by position.
        for (id, _, registry, _) in CLOUD_MODELS {
            let action = profile["derived_actions"]
                .as_array()
                .unwrap()
                .iter()
                .find(|action| action["deployment_id"] == *id)
                .unwrap_or_else(|| panic!("{id} has no derived action"));
            assert_eq!(action["model"], registry.rsplit('/').next().unwrap());
            assert_eq!(action["worker"], action["model"]);
        }
    }

    /// Every pin declares a positive ceiling within the range settings are
    /// held to, so the built-in figures and an override are the same contract.
    #[test]
    fn every_pinned_model_declares_a_usable_ceiling() {
        for (id, wire, registry, ceiling) in CLOUD_MODELS {
            assert!(
                CLOUD_OUTPUT_RANGE.contains(ceiling),
                "{id} ({wire}) declares {ceiling}, outside {CLOUD_OUTPUT_RANGE:?}"
            );
            assert!(
                *ceiling <= CLOUD_CONTEXT_WINDOW,
                "{id} ({wire}) declares an output ceiling over its context window"
            );
            // The router pin is a catalog id; the core needs the registry's
            // `provider/model` for the cell, and the action's model name is
            // its last component.
            assert!(is_router_model_id(wire), "{id} pins {wire:?}");
            assert!(
                is_model_id(registry),
                "{id} ({registry}) is not a registry model id"
            );
        }
    }

    /// The core advertises the smallest deployment cap, so a long local turn
    /// gets a quarter of the local window up to the harness default.
    #[test]
    fn default_profile_output_cap_scales_with_the_local_window() {
        let local_cap = |window| {
            let profile =
                default_profile("https://r", "http://l", "m", window, None, None).unwrap();
            profile["deployments"][0]["max_output_tokens"]
                .as_u64()
                .unwrap()
        };
        assert_eq!(local_cap(2048), 1024);
        assert_eq!(local_cap(DEFAULT_LOCAL_CONTEXT_WINDOW), 24_576);
        assert_eq!(local_cap(1_000_000), 32_000);
    }

    #[test]
    fn default_profile_prices_the_local_model_at_its_marginal_cost() {
        let profile = default_profile(
            "https://api-dev.rayline.ai",
            "http://127.0.0.1:8080/v1",
            "q",
            16384,
            None,
            None,
        )
        .unwrap();
        let config = compose(
            profile,
            Some(&settings(None)),
            Path::new("/state"),
            ProfileOrigin::Default,
        )
        .unwrap()
        .config;
        let decisions = &config["decisions"];
        assert_eq!(
            decisions["routing"]["prices"],
            json!({"qwen/qwen3.8-27b": {"input": 0.0, "output": 0.0, "cache_read": 0.0, "cache_write": 0.0}})
        );
        // Routes API V1: no `exclusive` member and no precomputed digest.
        assert!(decisions["routing"].get("exclusive").is_none());
        assert!(decisions["routing"].get("v_value").is_none());
        assert_eq!(decisions["result"], "swift");
        assert!(decisions.get("price_sha256").is_none());
        // local_price is configurable and validated; it is refused for a profile file.
        let priced = settings(Some(json!({"local_price": 0.05})));
        let profile = default_profile(
            "https://r.example",
            "http://l",
            "q",
            16384,
            None,
            Some(&priced),
        )
        .unwrap();
        assert_eq!(
            profile["decisions"]["routing"]["prices"]["qwen/qwen3.8-27b"]["input"],
            0.05
        );
        let negative = settings(Some(json!({"local_price": -1})));
        assert!(
            default_profile(
                "https://r.example",
                "http://l",
                "q",
                16384,
                None,
                Some(&negative),
            )
            .is_err()
        );
        assert!(
            compose(
                profile_value(),
                Some(&priced),
                Path::new("/p"),
                ProfileOrigin::File
            )
            .is_err()
        );
    }

    #[test]
    fn local_price_is_refused_for_every_profile_file() {
        // Even a package-less profile with derived actions sets its own prices.
        let mut derived = profile();
        derived["derived_actions"] =
            json!([{"model": "m", "worker": "m", "deployment_id": "local-qwen"}]);
        for price in [json!(0.05), json!(0), json!(-1), json!("cheap")] {
            let priced = settings(Some(json!({"local_price": price})));
            for file in [profile(), derived.clone()] {
                let error = compose(file, Some(&priced), Path::new("/p"), ProfileOrigin::File)
                    .expect_err("local_price with a profile file");
                assert!(
                    error.contains("local_price applies only to the default profile"),
                    "{error}"
                );
            }
        }
    }

    #[test]
    fn local_price_is_validated_for_the_default_profile() {
        let base =
            || default_profile("https://r.example", "http://l", "q", 16384, None, None).unwrap();
        for price in [
            json!(-1),
            json!(1e7),
            json!(1e-9),
            json!("0.1"),
            json!(null),
        ] {
            let priced = settings(Some(json!({"local_price": price})));
            let error = compose(
                base(),
                Some(&priced),
                Path::new("/state"),
                ProfileOrigin::Default,
            )
            .expect_err("invalid local_price");
            assert!(
                error.contains("local_price must be 0 or a price"),
                "{error}"
            );
        }
        let priced = settings(Some(json!({"local_price": 0.05})));
        assert!(
            compose(
                base(),
                Some(&priced),
                Path::new("/state"),
                ProfileOrigin::Default
            )
            .is_ok()
        );
    }

    #[test]
    fn a_non_object_hybrid_block_is_refused() {
        for hybrid in [json!(5), json!("utility"), json!([]), json!(null)] {
            let mut bad = settings(None);
            bad["hybrid"] = hybrid.clone();
            assert!(
                compose(profile(), Some(&bad), Path::new("/p"), ProfileOrigin::File).is_err(),
                "{hybrid}"
            );
        }
        let mut bad = settings(None);
        bad["hybrid"] = json!({"routing": null});
        assert!(compose(profile(), Some(&bad), Path::new("/p"), ProfileOrigin::File).is_err());
    }

    fn profile_value() -> Value {
        profile()
    }

    #[test]
    fn prefer_local_maps_to_its_preset_and_threshold() {
        let block = json!({"policy": "prefer_local", "min_p_pass": 0.65});
        // A local deployment without a counter is sized by the core itself.
        let sized = compose(
            profile(),
            Some(&settings(Some(block.clone()))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(sized.preset.as_deref(), Some(PREFER_LOCAL_PRESET));
        // A malformed counter is refused.
        let mut malformed = profile();
        malformed["deployments"][0]["prompt_counter"] = json!("/m.gguf");
        assert!(
            compose(
                malformed,
                Some(&settings(Some(block.clone()))),
                Path::new("/p"),
                ProfileOrigin::File
            )
            .is_err()
        );
        let mut counted = profile();
        counted["deployments"][0]["prompt_counter"] = json!({"model_path": "/m.gguf"});
        let composed = compose(
            counted,
            Some(&settings(Some(block))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": PREFER_LOCAL_PRESET, "overrides": {"selection": {"min_p_pass": 0.65}}})
        );
    }

    #[test]
    fn a_profile_without_api_mode_keeps_the_core_default() {
        let mut legacy = profile();
        legacy["decisions"]
            .as_object_mut()
            .unwrap()
            .remove("api_mode");
        let composed = compose(
            legacy.clone(),
            Some(&settings(None)),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.policy,
            PolicySource::LegacyMode("current".to_owned())
        );
        assert!(composed.config["decisions"].get("api_mode").is_none());
        assert!(
            composed.config["decisions"]["routing"]
                .get("policy")
                .is_none()
        );
        assert!(
            compose(
                legacy,
                Some(&settings(Some(json!({"policy": "utility"})))),
                Path::new("/p"),
                ProfileOrigin::File
            )
            .is_err()
        );
    }

    #[test]
    fn a_saved_key_never_reaches_a_local_endpoint_sharing_its_variable() {
        let home =
            std::env::temp_dir().join(format!("rayline-hybrid-localvar-{}", std::process::id()));
        let dir = home.join(".config").join(crate::CONFIG_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"environments":{"dev":{"router_url":"https://api-dev.rayline.ai","cli_auth_url":"https://dev.example/cli-auth"}}}"#).unwrap();
        std::fs::write(
            dir.join("credentials.json"),
            r#"{"router_keys":{"dev":{"apiKey":"rlk-dev-synthetic"}}}"#,
        )
        .unwrap();
        let mut shared = profile();
        shared["deployments"][0]["auth_env"] = json!("RAYLINE_ROUTER_API_KEY");
        shared["deployments"][0]["url"] = json!("https://remote.example/v1/chat/completions");
        assert!(saved_credentials(&shared, &home, |_| None).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn invalid_or_unserved_overrides_are_refused() {
        for block in [
            json!({"policy": "utility", "min_p_pass": 0.5}),
            json!({"policy": "prefer_local", "min_p_pass": 1.0}),
            json!({"policy": "fastest"}),
            json!({"schedule": {"model": ["every_5"]}}),
            json!({"schedule": {"model": []}}),
            json!({"schedule": {"model": ["user_messages", "user_messages"]}}),
            json!({"schedule": {"tools": ["all_turns"]}}),
            json!({"v_value": 0.5}),
            json!({"value_usd": 0.5}),
            json!({"selection": {"min_p_pass": 0.5}}),
        ] {
            assert!(
                compose(
                    profile(),
                    Some(&settings(Some(block.clone()))),
                    Path::new("/p"),
                    ProfileOrigin::File
                )
                .is_err(),
                "{block}"
            );
        }
    }

    #[test]
    fn legacy_mode_profiles_are_unchanged_and_refuse_overrides() {
        let mut legacy = profile();
        legacy["decisions"]["api_mode"] = json!("current");
        let composed = compose(
            legacy.clone(),
            Some(&settings(None)),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(composed.config["decisions"]["routing"], json!({}));
        assert_eq!(composed.config["decisions"]["api_mode"], "current");
        assert!(
            compose(
                legacy,
                Some(&settings(Some(json!({"policy": "utility"})))),
                Path::new("/p"),
                ProfileOrigin::File
            )
            .is_err()
        );
    }

    #[test]
    fn profile_policy_is_kept_unless_the_user_overrides_it() {
        let mut pinned = profile();
        pinned["decisions"]["routing"]["policy"] =
            json!({"preset": "arc-policy.package-default.v1"});
        let composed = compose(
            pinned.clone(),
            Some(&settings(None)),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.preset.as_deref(),
            Some("arc-policy.package-default.v1")
        );
        assert_eq!(composed.policy, PolicySource::Profile);
        let composed = compose(
            pinned,
            Some(&settings(Some(json!({"policy": "utility"})))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(composed.preset.as_deref(), Some(HYBRID_DEFAULT_PRESET));
        // Switching a prefer-local profile to utility drops its threshold only.
        let mut local = profile();
        local["decisions"]["routing"]["policy"] = json!({"preset": PREFER_LOCAL_PRESET,
            "overrides": {"selection": {"min_p_pass": 0.7}, "schedule": {"model": ["all_turns"]}}});
        let composed = compose(
            local.clone(),
            Some(&settings(Some(json!({"policy": "utility"})))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET, "overrides": {"schedule": {"model": ["all_turns"]}}})
        );
        local["decisions"]["routing"]["policy"]["overrides"]
            .as_object_mut()
            .unwrap()
            .remove("schedule");
        let composed = compose(
            local,
            Some(&settings(Some(json!({"policy": "utility"})))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET})
        );
    }

    #[test]
    fn local_model_fills_only_the_opted_in_local_deployment() {
        let composed = compose(
            profile(),
            Some(&settings(None)),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        let local = &composed.config["deployments"][0];
        assert_eq!(local["url"], "http://127.0.0.1:11434/v1/chat/completions");
        assert_eq!(local["wire_model"], "qwen3.8:27b");
        assert!(local.get("from_local_model").is_none());
        assert_eq!(
            composed.config["deployments"][1]["url"],
            "https://api-dev.rayline.ai/v1/messages"
        );
        let missing = json!({"local_model": {"mode": "recommended"}});
        assert!(
            compose(
                profile(),
                Some(&missing),
                Path::new("/p"),
                ProfileOrigin::File
            )
            .is_err()
        );
        let mut cloud_opt_in = profile();
        cloud_opt_in["deployments"][1]["from_local_model"] = json!(true);
        assert!(
            compose(
                cloud_opt_in,
                Some(&settings(None)),
                Path::new("/p"),
                ProfileOrigin::File
            )
            .is_err()
        );
    }

    #[test]
    fn saved_keys_only_reach_their_own_router_host() {
        let home = std::env::temp_dir().join(format!("rayline-hybrid-keys-{}", std::process::id()));
        let config_dir = home.join(".config").join(crate::CONFIG_DIR);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{"environments":{"dev":{"router_url":"https://api-dev.rayline.ai","cli_auth_url":"https://dev.example/cli-auth"}}}"#,
        )
        .unwrap();
        std::fs::write(
            config_dir.join("credentials.json"),
            r#"{"router_keys":{"dev":{"apiKey":"rlk-dev-synthetic"},"prod":{"apiKey":"rlk-prod-synthetic"}}}"#,
        )
        .unwrap();
        let unset = |_: &str| None;
        let found = saved_credentials(&profile(), &home, unset).unwrap();
        assert_eq!(
            found,
            vec![(
                "RAYLINE_ROUTER_API_KEY".to_owned(),
                "rlk-dev-synthetic".to_owned()
            )]
        );
        let explicit = |_: &str| Some("from-launch-env".to_owned());
        assert!(
            saved_credentials(&profile(), &home, explicit)
                .unwrap()
                .is_empty()
        );
        let mut elsewhere = profile();
        elsewhere["decisions"]["url"] = json!("https://decisions.example.com/v1/routes");
        elsewhere["deployments"][1]["url"] = json!("http://api-dev.rayline.ai/v1/messages");
        assert!(
            saved_credentials(&elsewhere, &home, unset)
                .unwrap()
                .is_empty()
        );
        // One variable shared with an endpoint on another host or port is refused.
        let mut shared = profile();
        shared["deployments"][1]["url"] = json!("https://gateway.example.com/v1/messages");
        assert!(saved_credentials(&shared, &home, unset).is_err());
        let mut port = profile();
        port["deployments"][1]["url"] = json!("https://api-dev.rayline.ai:8443/v1/messages");
        assert!(saved_credentials(&port, &home, unset).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn decisions_go_only_to_a_configured_router_or_loopback() {
        let home =
            std::env::temp_dir().join(format!("rayline-hybrid-decisions-{}", std::process::id()));
        let config_dir = home.join(".config").join(crate::CONFIG_DIR);
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("settings.json"),
            r#"{"environments":{"stage":{"router_url":"https://router.stage.example:8443","cli_auth_url":"https://stage.example/cli-auth"}}}"#,
        )
        .unwrap();
        let routers = configured_router_urls(&home);
        assert_eq!(
            routers,
            vec![
                "https://api.rayline.ai:443".to_owned(),
                "https://router.stage.example:8443".to_owned()
            ]
        );
        // The default profile is built on the selected environment's router.
        for router in [
            "https://api.rayline.ai",
            "https://router.stage.example:8443",
        ] {
            let profile =
                default_profile(router, "http://127.0.0.1:1", "q", 16384, None, None).unwrap();
            check_decisions_url(&profile, &routers).unwrap();
        }
        // A profile file is held to the same allowlist.
        let mut custom = profile();
        check_decisions_url(&custom, &routers).unwrap();
        custom["decisions"]["url"] = json!("http://127.0.0.1:9/decisions");
        check_decisions_url(&custom, &routers).unwrap();
        for url in [
            json!("https://rayline--decisions.modal.run/v1/routes"),
            json!("https://router.stage.example/v1/routes"),
            json!("https://user@api.rayline.ai/v1/routes"),
            json!(42),
        ] {
            custom["decisions"]["url"] = url.clone();
            assert!(check_decisions_url(&custom, &routers).is_err(), "{url}");
        }
        custom["decisions"].as_object_mut().unwrap().remove("url");
        assert!(check_decisions_url(&custom, &routers).is_err());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn the_default_profile_prefers_local_without_a_prompt_counter() {
        for local_settings in [settings(Some(json!({"policy": "prefer_local"}))), {
            let mut glm = settings(Some(json!({"policy": "prefer_local"})));
            glm["hybrid"]["local_registry_model"] = json!("z-ai/glm-5.3-flash");
            glm
        }] {
            let profile = default_profile(
                "https://r.example",
                "http://127.0.0.1:1/v1",
                "local-model",
                98304,
                None,
                Some(&local_settings),
            )
            .unwrap();
            assert!(profile["deployments"][0].get("prompt_counter").is_none());
            let composed = compose(
                profile,
                Some(&local_settings),
                Path::new("/state"),
                ProfileOrigin::Default,
            )
            .unwrap();
            assert_eq!(composed.preset.as_deref(), Some(PREFER_LOCAL_PRESET));
        }
    }

    #[test]
    fn classifier_model_is_haiku_unless_settings_or_the_flag_name_another() {
        assert_eq!(classifier_model(None, None).unwrap(), "claude-haiku-5-5");
        let settings = json!({"hybrid": {"classifier_model": "claude-sonnet-5-5"}});
        assert_eq!(
            classifier_model(None, Some(&settings)).unwrap(),
            "claude-sonnet-5-5"
        );
        // A `provider/model` router id is accepted too.
        assert_eq!(
            classifier_model(Some("anthropic/claude-opus-5.5"), Some(&settings)).unwrap(),
            "anthropic/claude-opus-5.5"
        );
        for bad in ["", "claude haiku", "a/b/c", "/haiku", "anthropic/"] {
            let settings = json!({"hybrid": {"classifier_model": bad}});
            assert!(
                classifier_model(None, Some(&settings))
                    .unwrap_err()
                    .starts_with("settings hybrid.classifier_model"),
                "{bad:?} was accepted"
            );
            assert!(
                classifier_model(Some(bad), None)
                    .unwrap_err()
                    .starts_with("--classifier-model"),
                "{bad:?} was accepted"
            );
        }
    }

    /// Settings keyed by a model the profile no longer pins (the OpenRouter
    /// ids earlier releases pinned) are refused, naming the key and the pins.
    #[test]
    fn cloud_output_tokens_keyed_by_an_unpinned_model_names_it() {
        let mut settings = settings(None);
        settings["hybrid"]["cloud_output_tokens"] = json!({
            "anthropic/claude-opus-5.5": 96_000,
            "anthropic/claude-sonnet-5.5": 64_000,
            "anthropic/claude-haiku-5.5": 32_000
        });
        let error = cloud_pins(Some(&settings)).unwrap_err();
        assert!(
            error.starts_with("settings hybrid.cloud_output_tokens names anthropic/claude-")
                && error.ends_with("claude-opus-5-5, claude-sonnet-5-5, claude-haiku-5-5"),
            "{error}"
        );
    }

    #[test]
    fn local_registry_model_defaults_to_qwen() {
        assert_eq!(local_registry_model(None).unwrap(), "qwen/qwen3.8-27b");
        let profile = default_profile(
            "https://r.example",
            "http://127.0.0.1:1/v1",
            "qwen3.8:27b",
            98304,
            None,
            Some(&settings(None)),
        )
        .unwrap();
        assert_eq!(
            profile["deployments"][0]["registry_model"],
            "qwen/qwen3.8-27b"
        );
        assert_eq!(
            profile["derived_actions"][0],
            json!({"model": "qwen3.8-27b", "worker": "qwen3.8-27b", "deployment_id": "local"})
        );
        assert!(profile["decisions"]["routing"]["prices"]["qwen/qwen3.8-27b"].is_object());
    }

    #[test]
    fn a_declared_local_model_names_the_deployment_action_and_price() {
        let mut glm = settings(None);
        glm["hybrid"] = json!({"local_registry_model": "z-ai/glm-5.3-flash",
                               "routing": {"local_price": 0.5}});
        let profile = default_profile(
            "https://r.example",
            "http://127.0.0.1:8000/v1",
            "GLM-5.3-Flash-EXL3",
            98304,
            None,
            Some(&glm),
        )
        .unwrap();
        let local = &profile["deployments"][0];
        assert_eq!(local["registry_model"], "z-ai/glm-5.3-flash");
        assert_eq!(local["wire_model"], "GLM-5.3-Flash-EXL3");
        assert_eq!(
            profile["derived_actions"][0],
            json!({"model": "glm-5.3-flash", "worker": "glm-5.3-flash", "deployment_id": "local"})
        );
        let prices = profile["decisions"]["routing"]["prices"]
            .as_object()
            .unwrap();
        assert_eq!(prices.keys().collect::<Vec<_>>(), ["z-ai/glm-5.3-flash"]);
        assert_eq!(prices["z-ai/glm-5.3-flash"]["input"], 0.5);
        // It applies to the default profile only, like local_price.
        let mut file_settings = settings(None);
        file_settings["hybrid"] = json!({"local_registry_model": "z-ai/glm-5.3-flash"});
        let error = compose(
            profile_value(),
            Some(&file_settings),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap_err();
        assert!(error.starts_with("settings hybrid.local_registry_model"));
    }

    #[test]
    fn a_declared_local_model_skips_the_qwen_identity_check() {
        let url = "http://127.0.0.1:8000/v1";
        let glm = LocalIdentity::Mismatch("GLM-5.3-Flash-EXL3".to_owned());
        // The default model is checked: another model is refused.
        let (refused, message) = local_verdict(LOCAL_REGISTRY_MODEL, &glm, url);
        assert!(refused);
        assert!(message.unwrap().starts_with("Error:"));
        let (refused, message) = local_verdict(LOCAL_REGISTRY_MODEL, &LocalIdentity::Unknown, url);
        assert!(!refused);
        assert!(message.unwrap().contains("assumes Qwen3.8-27B"));
        // A declared model is neither refused nor assumed to be Qwen.
        for identity in [glm, LocalIdentity::Unknown, LocalIdentity::Match] {
            let (refused, message) = local_verdict("z-ai/glm-5.3-flash", &identity, url);
            let message = message.unwrap();
            assert!(!refused);
            assert!(
                message.starts_with("Local model: z-ai/glm-5.3-flash") && !message.contains("Qwen"),
                "{message}"
            );
        }
    }

    #[test]
    fn a_malformed_local_registry_model_is_refused() {
        for bad in [
            json!("glm-5.3-flash"),
            json!("/glm-5.3-flash"),
            json!("z-ai/"),
            json!("z-ai/glm/5.3"),
            json!("z-ai/glm 5.3"),
            json!(""),
            json!(42),
            json!(null),
        ] {
            let settings = json!({"hybrid": {"local_registry_model": bad}});
            let error = default_profile(
                "https://r.example",
                "http://127.0.0.1:1/v1",
                "m",
                98304,
                None,
                Some(&settings),
            )
            .unwrap_err();
            // `settings ` errors are prefixed with the settings file's path.
            assert!(
                error.starts_with("settings hybrid.local_registry_model"),
                "{bad}: {error}"
            );
        }
    }

    #[test]
    fn the_served_window_comes_from_the_matching_or_only_model() {
        let tensorfold = json!({"object": "list", "data": [{"id": "GLM-5.3-Flash-EXL3",
            "object": "model", "context_length": 1_048_576, "max_model_len": 1_048_576}]});
        assert_eq!(
            served_window(&tensorfold, "GLM-5.3-Flash-EXL3"),
            Some(1_048_576)
        );
        // The only entry is the served model whatever it is called.
        assert_eq!(served_window(&tensorfold, "glm"), Some(1_048_576));
        let vllm = json!({"data": [{"id": "a", "max_model_len": 32768},
                                   {"id": "b", "max_model_len": 65536}]});
        assert_eq!(served_window(&vllm, "b"), Some(65536));
        // Several entries and none named: unknown.
        assert_eq!(served_window(&vllm, "c"), None);
        let context_only = json!({"data": [{"id": "m", "context_length": 8192}]});
        assert_eq!(served_window(&context_only, "m"), Some(8192));
        let zero_max = json!({"data": [{"id": "m", "max_model_len": 0, "context_length": 8192}]});
        assert_eq!(served_window(&zero_max, "m"), Some(8192));
        for unknown in [
            json!({"data": [{"id": "m"}]}),
            json!({"data": [{"id": "m", "max_model_len": 0}]}),
            json!({"data": []}),
            json!({"models": []}),
        ] {
            assert_eq!(served_window(&unknown, "m"), None, "{unknown}");
        }
    }

    #[test]
    fn only_the_local_deployment_is_marked_as_refusing_an_over_window_prompt() {
        let mut profile =
            default_profile("https://r", "http://l", "m", 98_304, None, None).unwrap();
        mark_local_refuses_over_window(&mut profile);
        for deployment in profile["deployments"].as_array().unwrap() {
            let expected = (deployment["kind"] == "local").then(|| json!("refuses"));
            assert_eq!(
                deployment.get("context_overflow").cloned(),
                expected,
                "{deployment}"
            );
        }
    }

    #[tokio::test]
    async fn an_openai_compatible_runtime_reports_its_window_through_v1_models() {
        let client = reqwest::Client::new();
        let models = json!({"object": "list", "data": [{"id": "GLM-5.3-Flash-EXL3",
            "context_length": 1_048_576, "max_model_len": 1_048_576}]});
        let url = runtime_stub_with_models(None, None, Some(models)).await;
        let runtime = read_local_runtime(&client, &url, "GLM-5.3-Flash-EXL3").await;
        assert_eq!(
            runtime,
            LocalRuntime {
                window: Some(1_048_576),
                identity: LocalIdentity::Unknown,
                refuses_over_window: true,
            }
        );
    }

    /// A stub runtime answering `GET /props` and `POST /api/show` with the
    /// given bodies (None: 404), for as many requests as arrive.
    async fn runtime_stub(props: Option<Value>, show: Option<Value>) -> String {
        runtime_stub_with_models(props, show, None).await
    }

    /// [`runtime_stub`] that also answers `GET /v1/models`.
    async fn runtime_stub_with_models(
        props: Option<Value>,
        show: Option<Value>,
        models: Option<Value>,
    ) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let read = stream.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..read]);
                let body = if request.starts_with("GET /props ") {
                    props.clone()
                } else if request.starts_with("POST /api/show ") {
                    show.clone()
                } else if request.starts_with("GET /v1/models ") {
                    models.clone()
                } else {
                    None
                };
                let (status, body) = match body {
                    Some(body) => ("200 OK", body.to_string()),
                    None => ("404 Not Found", String::new()),
                };
                let head = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes()).await;
                let _ = stream.write_all(body.as_bytes()).await;
                let _ = stream.flush().await;
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    #[tokio::test]
    async fn llamacpp_runtime_is_identified_by_its_loaded_file() {
        let client = reqwest::Client::new();
        let props = |path: &str| {
            Some(json!({"default_generation_settings": {"n_ctx": 98304},
                        "model_alias": "qwen3.8-27b", "model_path": path}))
        };
        // Unsloth's file, and llama.cpp's -hf cache name for it.
        for path in [
            "/models/Qwen3.8-27B-UD-Q4_K_M.gguf",
            "/cache/llama.cpp/unsloth_Qwen3.8-27B-GGUF_Qwen3.8-27B-UD-Q4_K_M.gguf",
            "/models/qwen3.8-27b/model-q4.gguf",
        ] {
            let url = runtime_stub(props(path), None).await;
            let runtime = read_local_runtime(&client, &url, "qwen3.8-27b").await;
            assert_eq!(runtime.window, Some(98304));
            assert_eq!(runtime.identity, LocalIdentity::Match, "{path}");
            assert!(!runtime.identity.refuses_local());
        }
        // The alias names Qwen3.8 but the file is Qwen3.6: the alias is the
        // user's label, so this is a mismatch.
        let url = runtime_stub(props("/models/Qwen3.6-27B-Q4_K_M.gguf"), None).await;
        let runtime = read_local_runtime(&client, &url, "qwen3.8-27b").await;
        assert_eq!(
            runtime.identity,
            LocalIdentity::Mismatch("/models/Qwen3.6-27B-Q4_K_M.gguf".to_owned())
        );
        assert!(runtime.identity.refuses_local());
        let message = runtime.identity.start_message(&url).unwrap();
        assert!(
            message.contains("Qwen3.6-27B-Q4_K_M.gguf")
                && message.contains("every turn goes to cloud")
        );
        // The file name decides over its folder's name.
        let url = runtime_stub(props("/models/qwen3.8-27b/Qwen3.6-27B-Q4_K_M.gguf"), None).await;
        let runtime = read_local_runtime(&client, &url, "qwen3.8-27b").await;
        assert!(runtime.identity.refuses_local());
        // A name that identifies no model is unknown, not a mismatch.
        for path in [
            "/models/ggml-model-Q4_K_M.gguf",
            "/hf/snapshots/abc123/model.gguf",
        ] {
            let url = runtime_stub(props(path), None).await;
            let runtime = read_local_runtime(&client, &url, "qwen3.8-27b").await;
            assert_eq!(runtime.identity, LocalIdentity::Unknown, "{path}");
        }
        // No path reported: unknown.
        let url = runtime_stub(Some(json!({"n_ctx": 4096})), None).await;
        let runtime = read_local_runtime(&client, &url, "m").await;
        assert_eq!(
            runtime,
            LocalRuntime {
                window: Some(4096),
                identity: LocalIdentity::Unknown,
                refuses_over_window: false,
            }
        );
    }

    #[tokio::test]
    async fn ollama_runtime_is_identified_by_its_gguf_metadata() {
        let client = reqwest::Client::new();
        // `qwen3.8:27b` as the Ollama library publishes it, and unsloth's GGUF.
        for info in [
            json!({"general.basename": "Qwen3.8", "general.size_label": "27B", "general.name": "Qwen3.8 27B 0814"}),
            json!({"general.basename": "Qwen3.8-27B", "general.size_label": "27B", "general.name": "Qwen3.8-27B"}),
            // A local conversion whose size label rounds 27.8B up.
            json!({"general.basename": "Qwen3.8", "general.size_label": "28B", "general.name": "Model"}),
        ] {
            let url = runtime_stub(None, Some(json!({"model_info": info}))).await;
            let runtime = read_local_runtime(&client, &url, "qwen3.8:27b").await;
            assert_eq!(runtime.window, None);
            assert!(runtime.identity == LocalIdentity::Match, "{runtime:?}");
            // Ollama cuts an over-window prompt rather than refusing it.
            assert!(!runtime.refuses_over_window);
        }
        let qwen36 = json!({"model_info": {"general.basename": "Qwen3.6", "general.size_label": "27B", "general.name": "Qwen3.6 27B"}});
        let url = runtime_stub(None, Some(qwen36)).await;
        let runtime = read_local_runtime(&client, &url, "qwen3.8:27b").await;
        assert_eq!(
            runtime.identity,
            LocalIdentity::Mismatch("Qwen3.6 27B".to_owned())
        );
        // The error names the field that identified the model.
        let llama = json!({"model_info": {"general.name": "Model", "general.basename": "Llama-3.3", "general.size_label": "70B"}});
        let url = runtime_stub(None, Some(llama)).await;
        let runtime = read_local_runtime(&client, &url, "my-model").await;
        assert_eq!(
            runtime.identity,
            LocalIdentity::Mismatch("Llama-3.3 70B".to_owned())
        );
        // Metadata that names no model family is unknown.
        let unnamed = json!({"model_info": {"general.name": "Model", "general.basename": "Model"}});
        let url = runtime_stub(None, Some(unnamed)).await;
        let runtime = read_local_runtime(&client, &url, "my-model").await;
        assert_eq!(runtime.identity, LocalIdentity::Unknown);
        // A runtime that reports neither (LM Studio, MLX, vLLM): unknown, and
        // local routing stays on with a warning.
        let url = runtime_stub(None, None).await;
        let runtime = read_local_runtime(&client, &url, "qwen3.8-27b").await;
        assert_eq!(runtime.identity, LocalIdentity::Unknown);
        assert!(!runtime.identity.refuses_local());
        assert!(
            runtime
                .identity
                .start_message(&url)
                .unwrap()
                .starts_with("Warning:")
        );
    }

    #[test]
    fn reported_names_match_mismatch_or_say_nothing() {
        for name in [
            "Qwen3.8-27B",
            "qwen3.8:27b",
            "Qwen3.8-27B-FP8",
            "qwen38-27b",
        ] {
            assert_eq!(name_identity(name, name), LocalIdentity::Match, "{name}");
        }
        for name in [
            "Qwen3.6-27B",
            "qwen3.8-270b",
            "Qwen3.8-2.4T-A95B",
            "Llama-3.3-70B",
            "gemma-4-31b",
        ] {
            assert!(name_identity(name, name).refuses_local(), "{name}");
        }
        for name in ["model.gguf", "ggml-model-Q4_K_M.gguf", "local", ""] {
            assert_eq!(name_identity(name, name), LocalIdentity::Unknown, "{name}");
        }
    }

    #[test]
    fn a_refused_local_model_leaves_a_cloud_only_profile() {
        let mut profile = default_profile(
            "https://api-dev.rayline.ai",
            "http://127.0.0.1:11434/v1",
            "llama3",
            98304,
            None,
            None,
        )
        .unwrap();
        without_local(&mut profile);
        let composed = compose(
            profile,
            Some(&settings(None)),
            Path::new("/state"),
            ProfileOrigin::Default,
        )
        .unwrap();
        let config = &composed.config;
        let kinds: Vec<_> = config["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|deployment| deployment["kind"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["cloud", "cloud", "cloud"]);
        let actions: Vec<_> = config["derived_actions"]
            .as_array()
            .unwrap()
            .iter()
            .map(|action| action["deployment_id"].as_str().unwrap())
            .collect();
        assert_eq!(actions, ["cloud-opus", "cloud-sonnet", "cloud-haiku"]);
        // Every caller price is unchanged and no digest is pinned.
        assert!(config["decisions"].get("price_sha256").is_none());
        assert_eq!(
            config["decisions"]["routing"]["prices"][LOCAL_REGISTRY_MODEL],
            json!({"input": 0.0, "output": 0.0, "cache_read": 0.0, "cache_write": 0.0})
        );
        // prefer_local still needs a local deployment; none is left.
        let mut cloud_only = default_profile(
            "https://r.example",
            "http://127.0.0.1:1/v1",
            "llama3",
            98304,
            None,
            None,
        )
        .unwrap();
        without_local(&mut cloud_only);
        // With no policy chosen, a session without a local model gets the
        // hybrid default (utility), not prefer-local.
        let composed = compose(
            cloud_only.clone(),
            Some(&settings(None)),
            Path::new("/state"),
            ProfileOrigin::Default,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET})
        );
        assert_eq!(composed.policy, PolicySource::Default);
        let prefer_local = settings(Some(json!({"policy": "prefer_local"})));
        assert!(
            compose(
                cloud_only,
                Some(&prefer_local),
                Path::new("/state"),
                ProfileOrigin::Default
            )
            .is_err()
        );
    }

    #[test]
    fn the_prefer_local_default_takes_a_threshold_and_yields_to_utility() {
        // min_p_pass alone tunes the prefer-local default.
        let composed = compose(
            profile(),
            Some(&settings(Some(json!({"min_p_pass": 0.5})))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": PREFER_LOCAL_PRESET, "overrides": {"selection": {"min_p_pass": 0.5}}})
        );
        // "utility" still selects the hybrid default preset.
        let composed = compose(
            profile(),
            Some(&settings(Some(json!({"policy": "utility"})))),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET})
        );
        // A malformed local prompt counter keeps the default on utility
        // rather than refusing a session that chose no policy.
        let mut malformed = profile();
        for deployment in malformed["deployments"].as_array_mut().unwrap() {
            if deployment["kind"] == "local" {
                deployment["prompt_counter"] = json!("llama.cpp");
            }
        }
        let composed = compose(
            malformed,
            Some(&settings(None)),
            Path::new("/p"),
            ProfileOrigin::File,
        )
        .unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET})
        );
    }
}
