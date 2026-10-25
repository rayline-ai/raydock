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

/// Compose the core configuration from a profile and the user's settings.
/// `base_dir` is the profile's directory: relative package and registry paths
/// are made absolute because the composed file lives in the state directory.
pub(crate) fn compose(
    mut profile: Value,
    settings: Option<&Value>,
    base_dir: &Path,
) -> Result<Composed, String> {
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
    let block = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("routing"));
    if let Some(block) = block
        && !block.is_object()
    {
        return Err("settings hybrid.routing must be an object".to_owned());
    }
    let decisions = root
        .get_mut("decisions")
        .and_then(Value::as_object_mut)
        .ok_or("hybrid configuration requires a decisions object")?;
    match decisions.get("api_mode").and_then(Value::as_str) {
        None | Some(API_MODE_V2) => {}
        Some(mode) => {
            let mode = mode.to_owned();
            if block.is_some() {
                return Err(format!(
                    "settings hybrid.routing cannot apply to a profile pinned to Decisions api_mode {mode:?}"
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
                "policy" | "schedule" | "value_usd" | "min_p_pass"
            ) {
                return Err(format!(
                    "settings hybrid.routing.{key} is not supported (policy, schedule, value_usd, min_p_pass)"
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
                .filter(|value| value.is_finite() && *value >= 0.0)
                .ok_or("settings hybrid.routing.value_usd must be a nonnegative number")?,
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
            "this profile takes its local endpoint from local_model; run `rayline local custom <url> <model>` first",
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

fn custom_local_model(settings: Option<&Value>) -> Option<(String, String)> {
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

/// Saved Rayline API keys for credential references whose endpoint is the
/// same host as a signed-in environment's router. A key is only ever offered
/// to the host it was issued for; an explicit launch environment wins.
pub(crate) fn saved_credentials(
    config: &Value,
    home: &Path,
    environment: impl Fn(&str) -> Option<String>,
) -> Result<Vec<(String, String)>, String> {
    let mut references = Vec::new();
    if let Some(decisions) = config.get("decisions") {
        references.push((decisions.get("auth_env"), decisions.get("url")));
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
        let hosts: Vec<_> = references
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
            "decisions": {"url": "https://api-dev.rayline.ai/v1/routes", "auth_env": "RAYLINE_ROUTER_API_KEY",
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
        let composed = compose(profile(), Some(&settings(None)), Path::new("/profiles")).unwrap();
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
        let composed = compose(profile(), Some(&settings(Some(block))), Path::new("/p")).unwrap();
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
    fn prefer_local_maps_to_its_preset_and_threshold() {
        let block = json!({"policy": "prefer_local", "min_p_pass": 0.65});
        let composed = compose(profile(), Some(&settings(Some(block))), Path::new("/p")).unwrap();
        assert_eq!(
            composed.config["decisions"]["routing"]["policy"],
            json!({"preset": PREFER_LOCAL_PRESET, "overrides": {"selection": {"min_p_pass": 0.65}}})
        );
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
            json!({"value_usd": "0.1"}),
            json!({"selection": {"min_p_pass": 0.5}}),
        ] {
            assert!(
                compose(
                    profile(),
                    Some(&settings(Some(block.clone()))),
                    Path::new("/p")
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
        let composed = compose(legacy.clone(), Some(&settings(None)), Path::new("/p")).unwrap();
        assert_eq!(composed.config["decisions"]["routing"], json!({}));
        assert_eq!(composed.config["decisions"]["api_mode"], "current");
        assert!(
            compose(
                legacy,
                Some(&settings(Some(json!({"value_usd": 1.0})))),
                Path::new("/p")
            )
            .is_err()
        );
    }

    #[test]
    fn profile_policy_is_kept_unless_the_user_overrides_it() {
        let mut pinned = profile();
        pinned["decisions"]["routing"]["policy"] =
            json!({"preset": "arc-policy.package-default.v1"});
        let composed = compose(pinned.clone(), Some(&settings(None)), Path::new("/p")).unwrap();
        assert_eq!(
            composed.preset.as_deref(),
            Some("arc-policy.package-default.v1")
        );
        assert_eq!(composed.policy, PolicySource::Profile);
        let composed = compose(
            pinned,
            Some(&settings(Some(json!({"policy": "utility"})))),
            Path::new("/p"),
        )
        .unwrap();
        assert_eq!(composed.preset.as_deref(), Some(HYBRID_DEFAULT_PRESET));
    }

    #[test]
    fn local_model_fills_only_the_opted_in_local_deployment() {
        let composed = compose(profile(), Some(&settings(None)), Path::new("/p")).unwrap();
        let local = &composed.config["deployments"][0];
        assert_eq!(local["url"], "http://127.0.0.1:11434/v1/chat/completions");
        assert_eq!(local["wire_model"], "qwen3.8:27b");
        assert!(local.get("from_local_model").is_none());
        assert_eq!(
            composed.config["deployments"][1]["url"],
            "https://api-dev.rayline.ai/v1/messages"
        );
        let missing = json!({"local_model": {"mode": "recommended"}});
        assert!(compose(profile(), Some(&missing), Path::new("/p")).is_err());
        let mut cloud_opt_in = profile();
        cloud_opt_in["deployments"][1]["from_local_model"] = json!(true);
        assert!(compose(cloud_opt_in, Some(&settings(None)), Path::new("/p")).is_err());
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
