//! The execution-core config the local process runs on, built from
//! [`Settings`] alone: `endpoints` (spec 5.2), `models` (5.3) and `routing`
//! (5.4).
//!
//! Each `models` entry is served by its endpoint under its `model` name, with
//! one deployment per protocol the endpoint lists. A model ARC picks that is
//! not in `models` goes to Rayline cloud: the [`CLOUD_MODELS`] pins, less any
//! whose id a `models` entry takes. Each deployment has one derived action
//! named for its model, so the core binds a model's deployments as one action
//! per control, one deployment per protocol.
//!
//! Decisions go to the environment's router for `routing.decisions_model`,
//! with `routing.policy` sent unchanged and a `price` for each entry that
//! states one; ARC prices every other model.

use serde_json::{Map, Value, json};

use crate::config::{ConfigError, Protocol, Settings, child};

const API_MODE_V2: &str = "execution_context_v2";
/// The variable the core reads the Rayline router key from, for Decisions
/// and the cloud pins.
pub(crate) const ROUTER_KEY_ENV: &str = "RAYLINE_ROUTER_API_KEY";
/// Context window of the Rayline cloud models, Claude Opus 5.5, Sonnet 5.5
/// and Haiku 5.5 (pricing snapshot 2026-10-16, `context_window_tokens`; Haiku
/// read from the dev and prod catalogs 2026-11-09, `context_length`).
const CLOUD_CONTEXT_WINDOW: u64 = 1_000_000;
/// The Rayline cloud models, as `(wire_model, registry_model,
/// max_output_tokens)`. Each has one `model_pin` deployment per protocol, at
/// the router's path for it.
///
/// `wire_model` is the router's own catalog entry; `registry_model` is the
/// same model as the core's thinking registry knows it, whose measured cell
/// supplies the action's controls.
///
/// The ceiling is each model's own (catalog figures, 2026-11-06 for Opus and
/// Sonnet and 2026-11-09 for Haiku, matching dev and prod).
pub(crate) const CLOUD_MODELS: &[(&str, &str, u64)] = &[
    ("claude-opus-5-5", "anthropic/claude-opus-5.5", 128_000),
    ("claude-sonnet-5-5", "anthropic/claude-sonnet-5.5", 128_000),
    ("claude-haiku-5-5", "anthropic/claude-haiku-5.5", 128_000),
];

/// Whether ARC knows `model`, so that its limits may come from ARC (spec 5.3).
/// ARC doesn't expose the set of models it knows yet, so no model is known
/// and every `models` entry must state `context_window` and
/// `max_output_tokens`.
pub(crate) fn arc_knows(_model: &str) -> bool {
    false
}

/// The core's name for `protocol` and the path it is served at below an
/// endpoint's `base_url`.
fn wire(protocol: Protocol) -> (&'static str, &'static str) {
    match protocol {
        Protocol::AnthropicMessages => ("anthropic.messages.v1", "/v1/messages"),
        Protocol::OpenaiChat => ("openai.chat.v1", "/v1/chat/completions"),
        Protocol::OpenaiResponses => ("openai.responses.v1", "/v1/responses"),
    }
}

/// The thinking registry's provider for a local deployment over `protocol`:
/// the registry keys a model's cells by wire dialect.
fn local_registry_provider(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::AnthropicMessages => "anthropic",
        Protocol::OpenaiChat | Protocol::OpenaiResponses => "openai",
    }
}

/// The deployment serving `model`, a `models` key or a cloud pin's registry
/// id, over `protocol`.
pub(crate) fn deployment_id(model: &str, protocol: Protocol) -> String {
    let suffix = match protocol {
        Protocol::AnthropicMessages => "messages",
        Protocol::OpenaiChat => "chat",
        Protocol::OpenaiResponses => "responses",
    };
    format!("{model}-{suffix}")
}

/// The core config for `settings`, with Decisions on `router_url` and, for
/// development, the Decisions cell `cell`. Refuses what the local process
/// must refuse before it serves anything (spec 5.8 items 10 and 11).
pub(crate) fn core_config(
    settings: &Settings,
    router_url: &str,
    cell: Option<&str>,
) -> Result<Value, ConfigError> {
    for (id, endpoint) in &settings.endpoints {
        if endpoint.api_key_env.as_deref() == Some(ROUTER_KEY_ENV) {
            return Err(ConfigError::Key {
                file: settings.file.clone(),
                key: format!("{}.api_key_env", child("endpoints", id)),
                problem: format!(
                    "names `{ROUTER_KEY_ENV}`, which holds the Rayline router key; name the \
                     variable holding this endpoint's own key"
                ),
            });
        }
    }
    crate::config::check_local_start(settings, arc_knows)?;
    let routing = settings
        .routing
        .as_ref()
        .expect("a local-mode load has routing");
    let mut deployments = Vec::new();
    let mut actions = Vec::new();
    let mut prices = Map::new();
    for (id, model) in &settings.models {
        let (Some(context_window), Some(max_output_tokens)) =
            (model.context_window, model.max_output_tokens)
        else {
            unreachable!("check_local_start refuses a model ARC doesn't know without its limits");
        };
        let endpoint = &settings.endpoints[&model.endpoint];
        let base_url = endpoint.base_url.as_str().trim_end_matches('/');
        for &protocol in &endpoint.protocols {
            let (wire_protocol, path) = wire(protocol);
            let deployment_id = deployment_id(id, protocol);
            let mut deployment = json!({
                "id": deployment_id, "kind": "local", "url": format!("{base_url}{path}"),
                "protocol": wire_protocol, "wire_model": model.model, "registry_model": id,
                "registry_provider": local_registry_provider(protocol),
                "runtime_identity": format!("local_model:{}", model.model),
                "max_output_tokens": max_output_tokens, "context_window": context_window,
                "input_modalities": ["text"], "token_units": "", "clamp_output_allowance": true});
            if let Some(var) = &endpoint.api_key_env {
                deployment["auth_env"] = json!(var);
            }
            deployments.push(deployment);
            actions.push(json!({"model": id, "worker": id, "deployment_id": deployment_id}));
        }
        if let Some(price) = &model.price {
            prices.insert(
                id.clone(),
                json!({"input": price.input, "output": price.output,
                       "cache_read": price.cache_read, "cache_write": price.cache_write}),
            );
        }
    }
    let router = router_url.trim_end_matches('/');
    // Spec 5.3 items 1 and 2: a model in `models` goes to its endpoint, so
    // its entry replaces the pin for the same id.
    for &(wire_model, registry, max_output_tokens) in CLOUD_MODELS {
        if settings.models.contains_key(registry) {
            continue;
        }
        for protocol in Protocol::ALL {
            let (wire_protocol, path) = wire(protocol);
            let deployment_id = deployment_id(registry, protocol);
            deployments.push(json!({
                "id": deployment_id, "kind": "cloud", "cloud_transport": "model_pin",
                "url": format!("{router}{path}"), "protocol": wire_protocol,
                "wire_model": wire_model, "registry_model": registry,
                "registry_provider": "openrouter",
                "runtime_identity": "rayline-catalog-model-pin-v1",
                "max_output_tokens": max_output_tokens, "context_window": CLOUD_CONTEXT_WINDOW,
                "input_modalities": ["text"], "clamp_output_allowance": true, "token_units": "",
                "auth_env": ROUTER_KEY_ENV}));
            actions.push(json!({"model": registry, "worker": registry,
                                "deployment_id": deployment_id, "bases": ["default"]}));
        }
    }
    let mut policy = json!({"preset": routing.policy.preset});
    if let Some(overrides) = &routing.policy.overrides {
        policy["overrides"] = Value::Object(overrides.clone());
    }
    let mut decisions_routing = json!({"policy": policy});
    if !prices.is_empty() {
        decisions_routing["prices"] = Value::Object(prices);
    }
    // `top` is ARC's answer for the model asked, `routing.decisions_model`.
    let mut decisions = json!({"api_mode": API_MODE_V2, "url": format!("{router}/v1/routes"),
                               "auth_env": ROUTER_KEY_ENV, "model": routing.decisions_model,
                               "result": "top", "routing": decisions_routing});
    if let Some(cell) = cell {
        decisions["cell_tag"] = json!(cell);
    }
    Ok(json!({
        "contract_version": 1,
        "allow_experimental_controls": true,
        "decisions": decisions,
        "deployments": deployments,
        "derived_actions": actions
    }))
}

/// The HTTPS origin authority (host and effective port) of an endpoint.
fn https_host(url: &str) -> Option<String> {
    rayline_local_router::decisions_url::https_authority(url)
}

/// The `https://host:port` origin of every hosted Rayline environment of
/// `settings`: the built-in prod plus each `environments` entry.
pub(crate) fn configured_router_urls(settings: &Settings) -> Vec<String> {
    crate::status::hosted_environments(settings)
        .iter()
        // Only the origin: a router URL's path, query or userinfo never
        // reaches `rld`'s arguments, and one with credentials is dropped.
        .filter_map(|hosted| https_host(&hosted.base_url))
        .map(|authority| format!("https://{authority}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` loaded as the settings.json of a temp dir named after `name`.
    fn load(name: &str, text: &str) -> Settings {
        let dir =
            std::env::temp_dir().join(format!("rayline-core-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        std::fs::write(&file, text).unwrap();
        let settings = crate::config::load(&crate::config::LoadOptions {
            mode: crate::config::Mode::LocalFile(&file),
            env: None,
            harness_protocol: None,
        })
        .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        settings
    }

    /// The routing every test shares.
    const ROUTING: &str = r#""routing": {"decisions_model": "arc-swift-1.0",
        "classifier_model": "anthropic/claude-haiku-5.5",
        "policy": {"preset": "arc-policy.prefer-local.v1"}}"#;

    /// A settings.json with one endpoint, `endpoint`, serving one model,
    /// `model`.
    fn one_model(endpoint: &str, model: &str) -> String {
        format!(
            r#"{{"endpoints": {{"box": {endpoint}}}, "models": {{"z-ai/glm-5.3-flash": {model}}}, {ROUTING}}}"#
        )
    }

    /// The cloud pins' deployments and derived actions, as built.
    fn cloud_pins(router: &str) -> (Vec<Value>, Vec<Value>) {
        let mut deployments = Vec::new();
        let mut actions = Vec::new();
        for &(wire, registry, ceiling) in CLOUD_MODELS {
            for (suffix, path, protocol) in [
                ("messages", "/v1/messages", "anthropic.messages.v1"),
                ("chat", "/v1/chat/completions", "openai.chat.v1"),
                ("responses", "/v1/responses", "openai.responses.v1"),
            ] {
                let id = format!("{registry}-{suffix}");
                deployments.push(
                    json!({"id": id, "kind": "cloud", "cloud_transport": "model_pin",
                           "url": format!("{router}{path}"), "protocol": protocol,
                           "wire_model": wire, "registry_model": registry, "registry_provider": "openrouter",
                           "runtime_identity": "rayline-catalog-model-pin-v1",
                           "max_output_tokens": ceiling, "context_window": 1_000_000,
                           "input_modalities": ["text"], "clamp_output_allowance": true,
                           "token_units": "", "auth_env": "RAYLINE_ROUTER_API_KEY"}),
                );
                actions.push(
                    json!({"model": registry, "worker": registry, "deployment_id": id,
                                    "bases": ["default"]}),
                );
            }
        }
        (deployments, actions)
    }

    #[test]
    fn the_core_config_comes_from_settings_json() {
        // `PATH` stands for a key variable that is set: every test process has it.
        let settings = load(
            "fixture",
            r#"{
            "endpoints": {
                "gpu-box": {"base_url": "https://gpu-box.example.com:8443/",
                            "protocols": ["anthropic_messages", "openai_chat"],
                            "api_key_env": "PATH"},
                "ollama": {"base_url": "http://127.0.0.1:11434", "protocols": ["openai_chat"]}
            },
            "models": {
                "z-ai/glm-5.3-flash": {"endpoint": "gpu-box", "model": "GLM-5.3-Flash-EXL3",
                    "context_window": 1000000, "max_output_tokens": 32000,
                    "tokenizer": "zai-org/GLM-5.3-Flash",
                    "price": {"input": 0, "output": 0.5, "cache_read": 0, "cache_write": 0}},
                "qwen/qwen3.8-27b": {"endpoint": "ollama", "model": "qwen3.8:27b",
                    "context_window": 98304, "max_output_tokens": 24576}
            },
            "routing": {"decisions_model": "arc-sage-1.0",
                "classifier_model": "anthropic/claude-haiku-5.5",
                "policy": {"preset": "arc-policy.prefer-local.v1",
                    "overrides": {"selection": {"min_p_pass": 0.5}}}}
        }"#,
        );
        let router = "https://api.rayline.ai";
        let local = |id: &str,
                     url: &str,
                     protocol: &str,
                     wire: &str,
                     registry: &str,
                     provider: &str,
                     window: u64,
                     output: u64| {
            json!({"id": id, "kind": "local", "url": url, "protocol": protocol, "wire_model": wire,
                   "registry_model": registry, "registry_provider": provider,
                   "runtime_identity": format!("local_model:{wire}"),
                   "max_output_tokens": output, "context_window": window,
                   "input_modalities": ["text"], "token_units": "", "clamp_output_allowance": true})
        };
        let mut glm_messages = local(
            "z-ai/glm-5.3-flash-messages",
            "https://gpu-box.example.com:8443/v1/messages",
            "anthropic.messages.v1",
            "GLM-5.3-Flash-EXL3",
            "z-ai/glm-5.3-flash",
            "anthropic",
            1_000_000,
            32_000,
        );
        glm_messages["auth_env"] = json!("PATH");
        let mut glm_chat = local(
            "z-ai/glm-5.3-flash-chat",
            "https://gpu-box.example.com:8443/v1/chat/completions",
            "openai.chat.v1",
            "GLM-5.3-Flash-EXL3",
            "z-ai/glm-5.3-flash",
            "openai",
            1_000_000,
            32_000,
        );
        glm_chat["auth_env"] = json!("PATH");
        let qwen = local(
            "qwen/qwen3.8-27b-chat",
            "http://127.0.0.1:11434/v1/chat/completions",
            "openai.chat.v1",
            "qwen3.8:27b",
            "qwen/qwen3.8-27b",
            "openai",
            98_304,
            24_576,
        );
        let (pins, pin_actions) = cloud_pins(router);
        let mut deployments = vec![qwen, glm_messages, glm_chat];
        deployments.extend(pins);
        let mut actions = vec![
            json!({"model": "qwen/qwen3.8-27b", "worker": "qwen/qwen3.8-27b",
                   "deployment_id": "qwen/qwen3.8-27b-chat"}),
            json!({"model": "z-ai/glm-5.3-flash", "worker": "z-ai/glm-5.3-flash",
                   "deployment_id": "z-ai/glm-5.3-flash-messages"}),
            json!({"model": "z-ai/glm-5.3-flash", "worker": "z-ai/glm-5.3-flash",
                   "deployment_id": "z-ai/glm-5.3-flash-chat"}),
        ];
        actions.extend(pin_actions);
        assert_eq!(
            core_config(&settings, router, Some("cell-a")).unwrap(),
            json!({
                "contract_version": 1,
                "allow_experimental_controls": true,
                "decisions": {
                    "api_mode": "execution_context_v2", "url": "https://api.rayline.ai/v1/routes",
                    "auth_env": "RAYLINE_ROUTER_API_KEY", "model": "arc-sage-1.0", "result": "top",
                    "cell_tag": "cell-a",
                    "routing": {
                        "policy": {"preset": "arc-policy.prefer-local.v1",
                                   "overrides": {"selection": {"min_p_pass": 0.5}}},
                        // Only the entry with a `price`; ARC prices the rest.
                        "prices": {"z-ai/glm-5.3-flash": {"input": 0.0, "output": 0.5,
                                                          "cache_read": 0.0, "cache_write": 0.0}}
                    }
                },
                "deployments": deployments,
                "derived_actions": actions
            })
        );
    }

    #[test]
    fn a_local_deployment_takes_its_registry_provider_from_its_protocol() {
        let settings = load(
            "providers",
            &one_model(
                r#"{"base_url": "http://127.0.0.1:8080",
                    "protocols": ["anthropic_messages", "openai_chat", "openai_responses"]}"#,
                r#"{"endpoint": "box", "model": "glm", "context_window": 131072, "max_output_tokens": 8192}"#,
            ),
        );
        let config = core_config(&settings, "https://api.rayline.ai", None).unwrap();
        let providers: Vec<_> = config["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|deployment| deployment["kind"] == "local")
            .map(|deployment| {
                (
                    deployment["protocol"].as_str().unwrap(),
                    deployment["registry_provider"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            providers,
            [
                ("anthropic.messages.v1", "anthropic"),
                ("openai.chat.v1", "openai"),
                ("openai.responses.v1", "openai"),
            ]
        );
    }

    #[test]
    fn with_no_price_and_no_overrides_neither_is_sent() {
        let settings = load(
            "plain",
            &one_model(
                r#"{"base_url": "http://127.0.0.1:8080", "protocols": ["openai_chat"]}"#,
                r#"{"endpoint": "box", "model": "glm", "context_window": 131072, "max_output_tokens": 8192}"#,
            ),
        );
        let config = core_config(&settings, "https://api.rayline.ai/", None).unwrap();
        assert_eq!(
            config["decisions"]["routing"],
            json!({"policy": {"preset": "arc-policy.prefer-local.v1"}})
        );
        assert!(config["decisions"].get("cell_tag").is_none());
        assert_eq!(
            config["decisions"]["url"],
            "https://api.rayline.ai/v1/routes"
        );
        assert!(config["deployments"][0].get("auth_env").is_none());
    }

    #[test]
    fn a_models_entry_replaces_the_cloud_pin_for_its_id() {
        let settings = load(
            "replaces-pin",
            &format!(
                r#"{{"endpoints": {{"box": {{"base_url": "http://127.0.0.1:8080", "protocols": ["anthropic_messages"]}}}},
                    "models": {{"anthropic/claude-haiku-5.5": {{"endpoint": "box", "model": "haiku-local",
                        "context_window": 200000, "max_output_tokens": 64000}}}}, {ROUTING}}}"#
            ),
        );
        let config = core_config(&settings, "https://api.rayline.ai", None).unwrap();
        let haiku: Vec<_> = config["deployments"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|deployment| deployment["registry_model"] == "anthropic/claude-haiku-5.5")
            .map(|deployment| deployment["id"].as_str().unwrap())
            .collect();
        assert_eq!(haiku, ["anthropic/claude-haiku-5.5-messages"]);
        let haiku_actions: Vec<_> = config["derived_actions"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|action| action["model"] == "anthropic/claude-haiku-5.5")
            .collect();
        assert_eq!(
            haiku_actions,
            [
                &json!({"model": "anthropic/claude-haiku-5.5", "worker": "anthropic/claude-haiku-5.5",
                        "deployment_id": "anthropic/claude-haiku-5.5-messages"})
            ]
        );
        // The other pins stay.
        for id in [
            "anthropic/claude-opus-5.5-responses",
            "anthropic/claude-sonnet-5.5-chat",
        ] {
            assert!(
                config["deployments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|deployment| deployment["id"] == id),
                "{id}"
            );
        }
    }

    #[test]
    fn an_api_key_env_that_is_not_set_is_refused() {
        let settings = load(
            "unset-key",
            &one_model(
                r#"{"base_url": "https://box.example.com", "protocols": ["openai_chat"],
                    "api_key_env": "RAYLINE_TEST_UNSET_ENDPOINT_KEY"}"#,
                r#"{"endpoint": "box", "model": "glm", "context_window": 131072, "max_output_tokens": 8192}"#,
            ),
        );
        let error = core_config(&settings, "https://api.rayline.ai", None)
            .unwrap_err()
            .to_string();
        assert!(
            error.ends_with(
                "`endpoints.box.api_key_env` names `RAYLINE_TEST_UNSET_ENDPOINT_KEY`, which is not set"
            ),
            "{error}"
        );
    }

    #[test]
    fn an_endpoint_naming_the_router_key_variable_is_refused() {
        let settings = load(
            "router-key",
            &one_model(
                r#"{"base_url": "https://box.example.com", "protocols": ["openai_chat"],
                    "api_key_env": "RAYLINE_ROUTER_API_KEY"}"#,
                r#"{"endpoint": "box", "model": "glm", "context_window": 131072, "max_output_tokens": 8192}"#,
            ),
        );
        let error = core_config(&settings, "https://api.rayline.ai", None)
            .unwrap_err()
            .to_string();
        assert!(
            error.ends_with(
                "`endpoints.box.api_key_env` names `RAYLINE_ROUTER_API_KEY`, which holds the \
                 Rayline router key; name the variable holding this endpoint's own key"
            ),
            "{error}"
        );
    }

    #[test]
    fn a_model_without_either_limit_is_refused() {
        for (missing, model) in [
            (
                "context_window",
                r#"{"endpoint": "box", "model": "glm", "max_output_tokens": 8192}"#,
            ),
            (
                "max_output_tokens",
                r#"{"endpoint": "box", "model": "glm", "context_window": 131072}"#,
            ),
        ] {
            let settings = load(
                missing,
                &one_model(
                    r#"{"base_url": "http://127.0.0.1:8080", "protocols": ["openai_chat"]}"#,
                    model,
                ),
            );
            let error = core_config(&settings, "https://api.rayline.ai", None)
                .unwrap_err()
                .to_string();
            assert!(
                error.ends_with(&format!(
                    "`models.\"z-ai/glm-5.3-flash\".{missing}` is required: ARC doesn't know this model"
                )),
                "{error}"
            );
        }
    }

    /// One model on loopback, with `environments` as given.
    fn with_environments(name: &str, environments: &str) -> Settings {
        load(
            name,
            &format!(
                r#"{{"environments": {environments},
                    "endpoints": {{"box": {{"base_url": "http://127.0.0.1:8080", "protocols": ["openai_chat"]}}}},
                    "models": {{"z-ai/glm-5.3-flash": {{"endpoint": "box", "model": "glm",
                        "context_window": 131072, "max_output_tokens": 8192}}}}, {ROUTING}}}"#
            ),
        )
    }

    #[test]
    fn an_environment_named_prod_is_not_the_built_in_prod() {
        let settings = with_environments(
            "configured-prod",
            r#"{"prod": {"base_url": "https://router.other.example", "login_url": "https://other.example/cli-auth"}}"#,
        );
        assert_eq!(
            configured_router_urls(&settings),
            vec![
                "https://api.rayline.ai:443".to_owned(),
                "https://router.other.example:443".to_owned()
            ]
        );
    }

    #[test]
    fn decisions_go_only_to_a_configured_router() {
        let settings = with_environments(
            "decisions",
            r#"{"stage": {"base_url": "https://router.stage.example:8443", "login_url": "https://stage.example/cli-auth"}}"#,
        );
        let routers = configured_router_urls(&settings);
        assert_eq!(
            routers,
            vec![
                "https://api.rayline.ai:443".to_owned(),
                "https://router.stage.example:8443".to_owned()
            ]
        );
        // rld holds the core config to the same allowlist.
        for router in [
            "https://api.rayline.ai",
            "https://router.stage.example:8443",
        ] {
            let config = core_config(&settings, router, None).unwrap();
            rayline_local_router::decisions_url::check_config(
                &config,
                routers.iter().map(String::as_str),
            )
            .unwrap();
        }
    }
}
