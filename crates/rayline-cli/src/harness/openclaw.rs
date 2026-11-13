//! OpenClaw: `OPENCLAW_CONFIG_PATH=<dir>/openclaw.json` (state stays in the
//! user's `~/.openclaw`). `${VAR}` is OpenClaw's config substitution; the API
//! key is a file SecretRef to `<dir>/router-key` instead, so OpenClaw never
//! caches its value (#83) and the key stays out of its environment.
//!
//! The default `rayline` provider speaks OpenAI Chat Completions, OpenClaw's
//! own default protocol, which the hosted router and the local gateway
//! (`/v1/chat/completions`, #106) both accept. Memory search is off because
//! its default embeddings provider sends side calls to OpenAI; heartbeat and
//! the compaction memory flush are off so no background turns hit the router.
//!
//! Episode keying (OpenClaw 2026.9.6 and 2026.10.8; `compat` is a model field,
//! the provider schema has none):
//! - On Chat Completions, `compat.sendSessionAffinityHeaders` makes OpenClaw
//!   send its persisted session id as `x-session-affinity` (with `session_id`
//!   and `x-client-request-id` carrying the same value), which the gateway
//!   keys the episode on; `--session-id <id>` in a later launch resumes that
//!   session and so keeps its episode. A custom `anthropic-messages` provider
//!   could not do this: it runs on OpenClaw's own fetch transport, which adds
//!   no session header.
//! - The `rayline-resp` provider (e.g. `rayline-resp/arc-sage-1.0`) speaks
//!   Responses, where `compat.supportsPromptCacheKey` sends the session as
//!   `prompt_cache_key`; without it OpenClaw sends no key on a custom
//!   endpoint. ARC is not served on Responses yet, so Chat Completions stays
//!   the default. Under `rayline run openclaw --local` OpenClaw speaks only Chat
//!   Completions, its one protocol (spec section 5.2), so this provider is not
//!   written there.
//!
//! OpenAI-style hosted tools (`web_search`) are not carried on a Chat
//! Completions upstream; ordinary function tools are.

use std::ffi::OsString;

use serde_json::json;

use super::{ConfigFile, Plan, RenderContext, json_file};

pub(crate) const CONFIG_FILE: &str = "openclaw.json";
pub(crate) const PROVIDER_ID: &str = "rayline";
pub(crate) const RESPONSES_PROVIDER_ID: &str = "rayline-resp";
/// The file the key SecretRef reads, next to the config.
pub(crate) const KEY_FILE: &str = "router-key";
/// The `secrets.providers` entry that reads [`KEY_FILE`].
pub(crate) const SECRET_PROVIDER: &str = "rayline";

/// OpenClaw root options that take a value; they may precede the command.
const ROOT_VALUE_OPTIONS: &[&str] = &["--profile", "--log-level", "--container"];
/// OpenClaw boolean root options.
const ROOT_FLAGS: &[&str] = &["--dev", "--no-color"];

/// The args OpenClaw runs with. `openclaw agent` sends its turn through a
/// running Gateway, which uses its own config and environment, not this
/// launch's; `--local` runs the turn in this process so the Rayline config
/// applies. `agent exec` is already embedded.
pub(crate) fn launch_args(args: &[OsString]) -> Vec<OsString> {
    let mut args = args.to_vec();
    let Some(command) = command_index(&args) else {
        return args;
    };
    if uses_container(&args[..command]) {
        return args;
    }
    let is_agent_turn =
        args[command] == "agent" && args.get(command + 1).is_none_or(|arg| arg != "exec");
    let has_local = args
        .iter()
        .take_while(|arg| *arg != "--")
        .any(|arg| arg == "--local");
    if is_agent_turn && !has_local {
        args.insert(command + 1, OsString::from("--local"));
    }
    args
}

/// Whether the root options select container mode, which re-execs OpenClaw
/// inside a container that sees none of this launch's config or env.
pub(crate) fn uses_container(root_args: &[OsString]) -> bool {
    root_args.iter().any(|arg| {
        arg.to_str()
            .is_some_and(|arg| arg == "--container" || arg.starts_with("--container="))
    })
}

/// Index of the command word, after any leading root options.
fn command_index(args: &[OsString]) -> Option<usize> {
    let mut index = 0;
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        if ROOT_VALUE_OPTIONS.contains(&arg) {
            index += 2;
        } else if ROOT_FLAGS.contains(&arg)
            || ROOT_VALUE_OPTIONS.iter().any(|option| {
                arg.strip_prefix(option)
                    .is_some_and(|rest| rest.starts_with('='))
            })
        {
            index += 1;
        } else {
            return (index < args.len()).then_some(index);
        }
    }
    None
}

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    // A SecretRef, not a string: OpenClaw resolves a string `apiKey` (or a
    // `${VAR}` in one) while loading the config and then writes the resolved
    // value into its agent model cache (`agents/<id>/agent/models.json`). For a
    // SecretRef it writes only a marker there (#83). A `singleValue` file
    // provider returns the whole file (one trailing newline trimmed); its ref
    // id must be `value`. OpenClaw refuses a file other users can read.
    let api_key = json!({ "source": "file", "provider": SECRET_PROVIDER, "id": "value" });
    let launch = &ctx.models;
    let models = |compat: serde_json::Value| {
        launch
            .offered
            .iter()
            .map(|model| {
                json!({
                    "id": model.id,
                    "name": model.display_name,
                    // ARC selects thinking effort; clients must not expose controls.
                    "reasoning": ctx.capabilities.is_some(),
                    "input": ctx.input_modalities(),
                    "contextWindow": model.context_window,
                    "maxTokens": model.max_output_tokens,
                    "compat": compat
                })
            })
            .collect::<Vec<_>>()
    };
    let mut providers = json!({
        PROVIDER_ID: {
            "baseUrl": ctx.v1_base(),
            "apiKey": api_key,
            "api": "openai-completions",
            "models": models(json!({ "sendSessionAffinityHeaders": true }))
        }
    });
    if ctx.capabilities.is_none() {
        providers[RESPONSES_PROVIDER_ID] = json!({
            "baseUrl": ctx.v1_base(),
            "apiKey": api_key,
            "api": "openai-responses",
            "models": models(json!({ "supportsPromptCacheKey": true }))
        });
    }
    let config = json!({
        "secrets": {
            "providers": {
                SECRET_PROVIDER: {
                    "source": "file",
                    "path": ctx.config_dir.join(KEY_FILE).to_string_lossy(),
                    "mode": "singleValue"
                }
            }
        },
        "memory": { "search": { "provider": "none" } },
        "agents": {
            "defaults": {
                "model": { "primary": format!("{PROVIDER_ID}/{}", launch.main) },
                "heartbeat": { "every": "0m" },
                "compaction": { "memoryFlush": { "enabled": false } }
            }
        },
        "models": {
            "mode": "merge",
            "providers": providers
        }
    });
    Plan {
        files: vec![
            json_file(CONFIG_FILE, &config, true),
            ConfigFile {
                name: KEY_FILE,
                contents: ctx.key.to_owned(),
                overwrite: true,
                merged_from: None,
            },
        ],
        env_set: vec![(
            "OPENCLAW_CONFIG_PATH",
            ctx.config_dir.join(CONFIG_FILE).into_os_string(),
        )],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_KEY, TEST_URL, env_value, render};
    use super::{CONFIG_FILE, KEY_FILE};

    #[test]
    fn renders_chat_completions_with_session_affinity_and_disables_side_calls() {
        let (dir, plan) = render(Harness::OpenClaw);
        let path = dir.join(CONFIG_FILE);
        assert_eq!(
            env_value(&plan, "OPENCLAW_CONFIG_PATH"),
            Some(&path.clone().into_os_string())
        );
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["memory"]["search"]["provider"], "none");
        let defaults = &config["agents"]["defaults"];
        assert_eq!(defaults["model"]["primary"], "rayline/arc-sage-1.0");
        assert_eq!(defaults["heartbeat"]["every"], "0m");
        assert_eq!(defaults["compaction"]["memoryFlush"]["enabled"], false);
        let provider = &config["models"]["providers"]["rayline"];
        assert_eq!(config["models"]["mode"], "merge");
        assert_eq!(provider["baseUrl"], format!("{TEST_URL}/v1"));
        assert_eq!(provider["api"], "openai-completions");
        let key_ref = serde_json::json!({ "source": "file", "provider": "rayline", "id": "value" });
        assert_eq!(provider["apiKey"], key_ref);
        assert_eq!(
            config["secrets"]["providers"]["rayline"],
            serde_json::json!({
                "source": "file",
                "path": dir.join(KEY_FILE).to_string_lossy(),
                "mode": "singleValue"
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(KEY_FILE)).unwrap(),
            TEST_KEY
        );
        let model = &provider["models"][0];
        assert_eq!(model["id"], "arc-sage-1.0");
        assert_eq!(model["name"], "ARC Sage");
        assert_eq!(model["reasoning"], false);
        assert_eq!(model["input"], serde_json::json!(["text", "image"]));
        assert_eq!(model["contextWindow"], 1000000);
        assert_eq!(model["maxTokens"], 128000);
        assert_eq!(provider["models"][1]["name"], "ARC Swift");
        assert_eq!(provider["models"][1]["maxTokens"], 128000);
        for model in provider["models"].as_array().unwrap() {
            assert_eq!(
                model["compat"],
                serde_json::json!({ "sendSessionAffinityHeaders": true })
            );
        }
        let responses = &config["models"]["providers"]["rayline-resp"];
        assert_eq!(responses["baseUrl"], format!("{TEST_URL}/v1"));
        assert_eq!(responses["api"], "openai-responses");
        assert_eq!(responses["apiKey"], key_ref);
        let ids: Vec<_> = responses["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["arc-sage-1.0", "arc-swift-1.0"]);
        assert_eq!(
            responses["models"][0]["compat"]["supportsPromptCacheKey"],
            true
        );
    }

    #[test]
    fn local_mode_writes_only_the_chat_completions_provider() {
        use super::super::tests::temp_dir;
        use super::super::{LaunchModels, RenderContext};
        let dir = temp_dir("openclaw-hybrid");
        let capabilities = rayline_local_router::ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 8192,
            input_modalities: vec!["text".into()],
        };
        let ctx = RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            allow_empty_signature: true,
            capabilities: Some(&capabilities),
            harness_major_version: None,
            models: LaunchModels::hybrid(&capabilities),
        };
        let plan = super::plan(&ctx);
        let file = plan
            .files
            .iter()
            .find(|file| file.name == CONFIG_FILE)
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&file.contents).unwrap();
        let providers = config["models"]["providers"].as_object().unwrap();
        assert_eq!(providers.keys().collect::<Vec<_>>(), ["rayline"]);
        assert_eq!(providers["rayline"]["api"], "openai-completions");
    }

    #[test]
    fn agent_turns_run_embedded_so_the_rayline_config_applies() {
        use super::launch_args;
        let args = |list: &[&str]| -> Vec<std::ffi::OsString> {
            list.iter().map(std::ffi::OsString::from).collect()
        };
        assert_eq!(
            launch_args(&args(&["agent", "-m", "hi"])),
            args(&["agent", "--local", "-m", "hi"])
        );
        // OpenClaw root options may come before the command.
        assert_eq!(
            launch_args(&args(&[
                "--log-level",
                "debug",
                "--dev",
                "agent",
                "-m",
                "hi"
            ])),
            args(&[
                "--log-level",
                "debug",
                "--dev",
                "agent",
                "--local",
                "-m",
                "hi"
            ])
        );
        assert_eq!(
            launch_args(&args(&["--profile=work", "agent", "-m", "hi"])),
            args(&["--profile=work", "agent", "--local", "-m", "hi"])
        );
        for unchanged in [
            &["agent", "--local", "-m", "hi"][..],
            &["--profile", "agent"],
            &["--dev", "agent", "exec", "do it"],
            // Container mode re-execs inside the container, which sees none
            // of this launch's config or env; --local would not help there.
            &["--container", "box", "agent", "-m", "hi"],
            &["--container=box", "agent", "-m", "hi"],
            &["agent", "-m", "hi", "--local"],
            &["agent", "exec", "do it"],
            &["tui"],
            &[],
        ] {
            assert_eq!(launch_args(&args(unchanged)), args(unchanged));
        }
    }
}
