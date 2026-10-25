//! Rayline Local's hybrid defaults, applied to a qualified execution-core
//! profile before `rld` starts the Go core.
//!
//! With no override, Rayline Local sends the hybrid default policy explicitly
//! (`routing.policy: {"preset": "arc-policy.hybrid-default.v1"}`) through the
//! core's `execution_context_v2` Decisions contract; it never relies on an
//! absent policy. The optional `hybrid.routing` block in settings.json maps
//! onto `routing.policy` and V:
//!
//! ```json
//! {"hybrid": {"routing": {"policy": "utility" | "prefer_local", "min_p_pass": 0.65,
//!                         "schedule": {"model": ["cache_boundaries", "user_messages"]},
//!                         "value_usd": 0.1}}}
//! ```
//!
//! A profile deployment marked `"from_local_model": true` takes its endpoint
//! and served model from the `local_model` entry (`rayline local custom …`).
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
const LOCAL_REGISTRY_MODEL: &str = "qwen/qwen3.8-27b";
/// The default V with a zero-priced local model: easy work stays on device,
/// harder work goes to the cloud (dev cell probe, 2026-10-25).
const DEFAULT_VALUE_USD: f64 = 0.5;
const CALLER_PRICES_SCHEMA: &str = "arc-caller-prices.v1";

/// `hybrid.routing.local_price`: USD per million tokens for every leg of the
/// local model, 0 or within [1e-6, 1e6] so its canonical spelling is plain.
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

/// RFC 8785 number spelling for the admitted range (no exponent form).
fn canonical_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{}", value as i64)
    } else {
        format!("{value}")
    }
}

/// The service's caller price digest: sha256(JCS({schema, exclusive, prices})).
fn caller_price_sha256(model: &str, price: f64) -> String {
    use sha2::Digest;
    let number = canonical_number(price);
    let canonical = format!(
        "{{\"exclusive\":false,\"prices\":{{{}:{{\"cache_read\":{number},\"cache_write\":{number},\"input\":{number},\"output\":{number}}}}},\"schema\":\"{CALLER_PRICES_SCHEMA}\"}}",
        serde_json::to_string(model).expect("model id serializes")
    );
    sha2::Sha256::digest(canonical.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
const SCHEDULE_CLASSES: &[&str] = &["cache_boundaries", "user_messages", "all_turns"];

/// Where the applied policy came from, for the start message.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PolicySource {
    /// Rayline Local's default: no override block and no profile policy.
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
    let (policy, source, set_value) = map_block(block, existing.as_ref())?;
    let preset = policy
        .get("preset")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    routing.insert("policy".to_owned(), Value::Object(policy));
    if let Some(value) = set_value {
        routing.insert("value_usd".to_owned(), json!(value));
        // The core executes the returned custom result for a configured V.
        decisions.insert("result".to_owned(), json!("custom"));
    }
    if preset.as_deref() == Some(PREFER_LOCAL_PRESET) {
        // Prefer-local excludes a local target whose capacity is unknown, so
        // without a qualified token counter it would never choose local.
        let counted = profile
            .get("deployments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|deployment| deployment.get("kind").and_then(Value::as_str) == Some("local"))
            .all(|deployment| {
                deployment
                    .get("prompt_counter")
                    .is_some_and(Value::is_object)
            });
        if !counted {
            return Err(
                "settings hybrid.routing.policy \"prefer_local\" needs a local deployment with a qualified llama.cpp prompt_counter (a profile file); without counted capacity it never selects the local model. Use \"utility\" with the default profile"
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

/// The mapped `routing.policy`, its source and an optional V.
type MappedRouting = (Map<String, Value>, PolicySource, Option<f64>);

/// Map the override block onto `routing.policy` and `routing.value_usd`.
fn map_block(block: Option<&Value>, existing: Option<&Value>) -> Result<MappedRouting, String> {
    let block = block.and_then(Value::as_object);
    if let Some(block) = block {
        for key in block.keys() {
            if !matches!(
                key.as_str(),
                "policy" | "schedule" | "value_usd" | "min_p_pass" | "local_price"
            ) {
                return Err(format!(
                    "settings hybrid.routing.{key} is not supported (policy, schedule, value_usd, min_p_pass, local_price)"
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
        policy.insert("preset".to_owned(), json!(HYBRID_DEFAULT_PRESET));
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
    let value = match block.and_then(|block| block.get("value_usd")) {
        None => None,
        Some(value) => Some(
            value
                .as_f64()
                .filter(|value| (0.1..=5.0).contains(value))
                .ok_or("settings hybrid.routing.value_usd must be a number between 0.1 and 5")?,
        ),
    };
    Ok((policy, source, value))
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
            "this profile takes its local endpoint from local_model; run `rayline local custom --url <URL> --model <NAME>` first",
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

/// The default package-less profile: local Qwen from `local_model`, Opus and
/// Sonnet through the signed-in router (exact model pins), decisions from the
/// router's `rayline-router` model with the hybrid default preset applied by
/// [`compose`]. Actions are derived by the core from its embedded registry.
pub(crate) fn default_profile(
    router_url: &str,
    base_url: &str,
    model: &str,
    context_window: u64,
    cell: Option<&str>,
    settings: Option<&Value>,
) -> Result<Value, String> {
    // On-device inference costs the user about nothing per token: price the
    // local model at `local_price` (default 0, every leg, USD per million
    // tokens) and leave cloud models at their market prices.
    let local_price = local_price(settings)?;
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
    let max_output = (context_window / 4).clamp(1024, 4096);
    let cloud = |id: &str, wire: &str| {
        json!({"id": id, "kind": "cloud", "cloud_transport": "model_pin", "url": format!("{router}/v1/messages"),
               "protocol": "anthropic.messages.v1", "wire_model": wire, "registry_model": wire,
               "registry_provider": "openrouter", "runtime_identity": "rayline-catalog-model-pin-v1",
               "max_output_tokens": 4096, "context_window": 200000, "input_modalities": ["text"],
               "token_units": "", "auth_env": ROUTER_KEY_ENV})
    };
    let legs = json!({"input": local_price, "output": local_price, "cache_read": local_price, "cache_write": local_price});
    let mut prices = Map::new();
    prices.insert(LOCAL_REGISTRY_MODEL.to_owned(), legs);
    let mut decisions = json!({"api_mode": API_MODE_V2, "url": format!("{router}/v1/routes"),
                               "auth_env": ROUTER_KEY_ENV, "model": "rayline-router", "result": "custom",
                               "routing": {"value_usd": DEFAULT_VALUE_USD, "prices": prices, "exclusive": false},
                               "price_sha256": caller_price_sha256(LOCAL_REGISTRY_MODEL, local_price)});
    if let Some(cell) = cell {
        decisions["cell_tag"] = json!(cell);
    }
    Ok(json!({
        "contract_version": 1,
        "allow_experimental_controls": true,
        "decisions": decisions,
        "deployments": [
            {"id": "local", "kind": "local", "url": local_url, "protocol": "openai.chat.v1", "wire_model": model,
             "registry_model": LOCAL_REGISTRY_MODEL, "registry_provider": "openai",
             "runtime_identity": format!("local_model:{model}"), "max_output_tokens": max_output,
             "context_window": context_window, "input_modalities": ["text"], "token_units": ""},
            cloud("cloud-opus", "anthropic/claude-opus-5.5"),
            cloud("cloud-sonnet", "anthropic/claude-sonnet-5.5")
        ],
        "derived_actions": [
            {"model": "qwen3.8-27b", "worker": "qwen3.8-27b", "deployment_id": "local"},
            {"model": "claude-opus-5.5", "worker": "claude-opus-5.5", "deployment_id": "cloud-opus", "bases": ["default"]},
            {"model": "claude-sonnet-5.5", "worker": "claude-sonnet-5.5", "deployment_id": "cloud-sonnet", "bases": ["default"]}
        ]
    }))
}

/// Reads llama.cpp's per-slot context window from `/props`; None when the
/// runtime does not report one (it is then the user's to state).
pub(crate) async fn probe_llamacpp_context(
    client: &reqwest::Client,
    base_url: &str,
) -> Option<u64> {
    let url = format!("{}/props", crate::local_model::normalize_base_url(base_url));
    let props: Value = client
        .get(url)
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    props
        .get("default_generation_settings")
        .and_then(|settings| settings.get("n_ctx"))
        .or_else(|| props.get("n_ctx"))
        .and_then(Value::as_u64)
        .filter(|window| *window > 0)
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
    let url = reqwest::Url::parse(url).ok()?;
    if url.scheme() != "https" {
        return None;
    }
    Some(format!(
        "{}:{}",
        url.host_str()?.to_ascii_lowercase(),
        url.port_or_known_default()?
    ))
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
    fn no_override_sends_the_hybrid_default_preset_explicitly() {
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
            json!({"preset": HYBRID_DEFAULT_PRESET})
        );
        assert_eq!(decisions["result"], "top");
        assert!(decisions["routing"].get("value_usd").is_none());
        assert_eq!(composed.policy, PolicySource::Default);
        assert_eq!(composed.config["package_path"], "/profiles/package.json");
    }

    #[test]
    fn override_block_maps_to_policy_schedule_and_value() {
        let block = json!({"policy": "utility", "schedule": {"model": ["cache_boundaries", "user_messages"], "thinking": ["all_turns"]}, "value_usd": 0.1});
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
        assert_eq!(decisions["routing"]["value_usd"], 0.1);
        assert_eq!(decisions["result"], "custom");
        assert_eq!(composed.policy, PolicySource::Settings);
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
        assert_eq!(config["decisions"]["model"], "rayline-router");
        assert_eq!(
            config["decisions"]["routing"]["policy"],
            json!({"preset": HYBRID_DEFAULT_PRESET})
        );
        assert!(config.get("package_path").is_none() && config.get("bindings").is_none());
        let local = &config["deployments"][0];
        assert_eq!(local["url"], "http://127.0.0.1:11434/v1/chat/completions");
        assert_eq!(local["wire_model"], "qwen3.8:27b");
        assert_eq!(local["context_window"], 16384);
        assert_eq!(local["max_output_tokens"], 4096);
        assert_eq!(config["derived_actions"].as_array().unwrap().len(), 3);
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
        assert_eq!(decisions["routing"]["exclusive"], false);
        assert_eq!(decisions["routing"]["value_usd"], 0.5);
        assert_eq!(decisions["result"], "custom");
        // Equal to the dev service's own echo for this price world.
        assert_eq!(
            decisions["price_sha256"],
            "5c2ccbe09f4b5f9938df5ca93282b5a39b3573cae108d5e6abf89de3466d254c"
        );
        // A user V override keeps the local price.
        let block = json!({"value_usd": 2.0});
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
            Some(&settings(Some(block))),
            Path::new("/state"),
            ProfileOrigin::Default,
        )
        .unwrap()
        .config;
        assert_eq!(config["decisions"]["routing"]["value_usd"], 2.0);
        assert!(config["decisions"]["routing"].get("prices").is_some());
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
        assert_ne!(
            profile["decisions"]["price_sha256"],
            "5c2ccbe09f4b5f9938df5ca93282b5a39b3573cae108d5e6abf89de3466d254c"
        );
        let negative = settings(Some(json!({"local_price": -1})));
        assert!(
            default_profile(
                "https://r.example",
                "http://l",
                "q",
                16384,
                None,
                Some(&negative)
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
        // Without a qualified counter the local target's capacity is unknown,
        // which prefer-local excludes: refused rather than never choosing local.
        assert!(
            compose(
                profile(),
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
                Some(&settings(Some(json!({"value_usd": 1.0})))),
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
            json!({"min_p_pass": 0.5}),
            json!({"policy": "prefer_local", "min_p_pass": 1.0}),
            json!({"policy": "fastest"}),
            json!({"schedule": {"model": ["every_5"]}}),
            json!({"schedule": {"model": []}}),
            json!({"schedule": {"model": ["user_messages", "user_messages"]}}),
            json!({"schedule": {"tools": ["all_turns"]}}),
            json!({"value_usd": -1}),
            json!({"value_usd": 0}),
            json!({"value_usd": 6}),
            json!({"value_usd": "0.1"}),
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
                Some(&settings(Some(json!({"value_usd": 1.0})))),
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
}
