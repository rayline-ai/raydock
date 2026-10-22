//! OpenClaw: `OPENCLAW_CONFIG_PATH=<dir>/openclaw.json` (state stays in the
//! user's `~/.openclaw`). `${VAR}` is OpenClaw's config substitution; the API
//! key is a file SecretRef to `<dir>/router-key` instead, so OpenClaw never
//! caches its value (#83) and the key stays out of its environment.
//!
//! `api` must be explicit: OpenClaw's default is chat completions, which 404s
//! at the gateway. Memory search is off because its default embeddings
//! provider sends side calls to OpenAI; heartbeat and the compaction memory
//! flush are off so no background turns hit the router.
//!
//! Episode keying (OpenClaw 2026.9.6; `compat` is a model field there, the
//! provider schema has none):
//! - Messages sends no session id, so the `rayline` provider always carries
//!   the per-launch `x-conversation-id` (unless
//!   `RAYLINE_HARNESS_CONV_ID_FALLBACK=0`). A custom `anthropic-messages`
//!   provider runs on OpenClaw's own fetch transport (strategy
//!   `boundary-aware:anthropic-messages`, `createAnthropicMessagesTransportStreamFn`
//!   / `createAnthropicTransportClient` in `dist/worker/worker.mjs`), which
//!   adds no `x-session-affinity`; only the Anthropic SDK path
//!   (`@openclaw/ai/dist/anthropic-DZQ_7gD1.mjs:309`) honours
//!   `compat.sendSessionAffinityHeaders`, so that flag is not set. With
//!   `--session-id <id>` in the args the conversation id is stable
//!   (`oc-` + 32 hex of sha256(id)), so relaunching that session keeps its
//!   episode; otherwise each launch is its own episode.
//! - The `rayline-resp` provider (model `rayline-resp/rayline-router`) speaks
//!   Responses, where `compat.supportsPromptCacheKey` sends the session as
//!   `prompt_cache_key`; without it OpenClaw sends no key on a custom
//!   endpoint. It takes the per-launch header only where the env needs it.

use std::ffi::OsString;

use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, ConfigFile, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext,
    json_file,
};

pub(crate) const CONFIG_FILE: &str = "openclaw.json";
pub(crate) const PROVIDER_ID: &str = "rayline";
pub(crate) const RESPONSES_PROVIDER_ID: &str = "rayline-resp";
/// The file the key SecretRef reads, next to the config.
pub(crate) const KEY_FILE: &str = "router-key";
/// The `secrets.providers` entry that reads [`KEY_FILE`].
pub(crate) const SECRET_PROVIDER: &str = "rayline";

/// The value of `--session-id <v>` / `--session-id=<v>` in OpenClaw's args
/// (the last one wins; trimmed, as OpenClaw does; a blank value counts as none).
pub(crate) fn session_id_arg(args: &[OsString]) -> Option<String> {
    option_value(args, "--session-id")
}

/// The `--agent` value in OpenClaw's args, if any.
pub(crate) fn agent_arg(args: &[OsString]) -> Option<String> {
    option_value(args, "--agent")
}

/// The last `<name> <v>` / `<name>=<v>` value before `--`, trimmed.
fn option_value(args: &[OsString], name: &str) -> Option<String> {
    let mut found = None;
    let mut args = args.iter().map(|arg| arg.to_string_lossy());
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == name {
            found = args.next().map(|value| value.trim().to_owned());
        } else if let Some(value) = arg
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('='))
        {
            found = Some(value.trim().to_owned());
        }
    }
    found.filter(|value| !value.is_empty())
}

/// The conversation id for a launch that names a session: OpenClaw scopes a
/// `--session-id` to the `--agent` when one is given (agent ids are
/// case-insensitive), so the id does too.
pub(crate) fn session_conversation_id(args: &[OsString]) -> Option<String> {
    let session = session_id_arg(args)?;
    Some(match agent_arg(args) {
        Some(agent) => {
            stable_conversation_id(&format!("agent:{}:{session}", agent.to_ascii_lowercase()))
        }
        None => stable_conversation_id(&session),
    })
}

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

/// `oc-` + the first 32 hex characters of sha256(session id).
pub(crate) fn stable_conversation_id(session_id: &str) -> String {
    let digest = Sha256::digest(session_id.as_bytes());
    let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("oc-{}", &hex[..32])
}

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    // A SecretRef, not a string: OpenClaw resolves a string `apiKey` (or a
    // `${VAR}` in one) while loading the config and then writes the resolved
    // value into its agent model cache (`agents/<id>/agent/models.json`). For a
    // SecretRef it writes only a marker there (#83). A `singleValue` file
    // provider returns the whole file (one trailing newline trimmed); its ref
    // id must be `value`. OpenClaw refuses a file other users can read.
    let api_key = json!({ "source": "file", "provider": SECRET_PROVIDER, "id": "value" });
    let conv_ref = format!("${{{CONV_ID_ENV}}}");
    // No native session id on Messages (see the module docs).
    let messages_headers = ctx.conversation_headers_for(&conv_ref, false);
    let responses_headers = ctx.conversation_headers(&conv_ref);
    let model = |compat: serde_json::Value| {
        json!({
            "id": ROUTER_MODEL,
            "name": format!("Rayline router ({})", ctx.env_name),
            "reasoning": true,
            "input": ["text", "image"],
            "contextWindow": CONTEXT_WINDOW,
            "maxTokens": MAX_OUTPUT_TOKENS,
            "compat": compat
        })
    };
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
                "model": { "primary": format!("{PROVIDER_ID}/{ROUTER_MODEL}") },
                "heartbeat": { "every": "0m" },
                "compaction": { "memoryFlush": { "enabled": false } }
            }
        },
        "models": {
            "mode": "merge",
            "providers": {
                PROVIDER_ID: {
                    "baseUrl": ctx.messages_base(),
                    "apiKey": api_key,
                    "api": "anthropic-messages",
                    "headers": messages_headers,
                    "models": [model(json!({}))]
                },
                RESPONSES_PROVIDER_ID: {
                    "baseUrl": ctx.v1_base(),
                    "apiKey": api_key,
                    "api": "openai-responses",
                    "headers": responses_headers,
                    "models": [model(json!({ "supportsPromptCacheKey": true }))]
                }
            }
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
    fn renders_explicit_api_and_disables_side_calls() {
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
        assert_eq!(defaults["model"]["primary"], "rayline/rayline-router");
        assert_eq!(defaults["heartbeat"]["every"], "0m");
        assert_eq!(defaults["compaction"]["memoryFlush"]["enabled"], false);
        let provider = &config["models"]["providers"]["rayline"];
        assert_eq!(config["models"]["mode"], "merge");
        assert_eq!(provider["baseUrl"], TEST_URL);
        assert_eq!(provider["api"], "anthropic-messages");
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
        // dev: Messages still carries the per-launch id, Responses does not.
        assert_eq!(
            provider["headers"],
            serde_json::json!({ "x-conversation-id": "${RAYLINE_CONV_ID}" })
        );
        let model = &provider["models"][0];
        assert_eq!(model["contextWindow"], 200000);
        assert_eq!(model["maxTokens"], 32000);
        assert_eq!(model["compat"], serde_json::json!({}));
        let responses = &config["models"]["providers"]["rayline-resp"];
        assert_eq!(responses["baseUrl"], format!("{TEST_URL}/v1"));
        assert_eq!(responses["api"], "openai-responses");
        assert_eq!(responses["apiKey"], key_ref);
        assert_eq!(responses["headers"], serde_json::json!({}));
        assert_eq!(responses["models"][0]["id"], "rayline-router");
        assert_eq!(
            responses["models"][0]["compat"]["supportsPromptCacheKey"],
            true
        );
    }

    #[test]
    fn messages_header_follows_the_override() {
        use super::super::ConversationIdFallback;
        use super::super::tests::render_with;
        let headers = |fallback| {
            let (dir, _) = render_with(Harness::OpenClaw, fallback);
            let config: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap())
                    .unwrap();
            let providers = &config["models"]["providers"];
            (
                providers["rayline"]["headers"].clone(),
                providers["rayline-resp"]["headers"].clone(),
            )
        };
        let header = serde_json::json!({ "x-conversation-id": "${RAYLINE_CONV_ID}" });
        let none = serde_json::json!({});
        assert_eq!(
            headers(ConversationIdFallback::new("prod", None)),
            (header.clone(), header.clone())
        );
        assert_eq!(
            headers(ConversationIdFallback::new("dev", Some("0"))),
            (none.clone(), none.clone())
        );
        assert_eq!(
            headers(ConversationIdFallback::new("prod", Some("0"))),
            (none.clone(), none)
        );
    }

    #[test]
    fn session_id_arg_forms() {
        use super::session_id_arg;
        let args = |list: &[&str]| -> Vec<std::ffi::OsString> {
            list.iter().map(std::ffi::OsString::from).collect()
        };
        assert_eq!(
            session_id_arg(&args(&["agent", "--session-id", "abc", "-m", "hi"])),
            Some("abc".into())
        );
        assert_eq!(
            session_id_arg(&args(&["agent", "--session-id=abc"])),
            Some("abc".into())
        );
        assert_eq!(session_id_arg(&args(&["agent", "-m", "hi"])), None);
        assert_eq!(session_id_arg(&args(&["agent", "--session-id"])), None);
        assert_eq!(session_id_arg(&args(&["agent", "--session-id="])), None);
        assert_eq!(
            session_id_arg(&args(&["agent", "--", "--session-id", "x"])),
            None
        );
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

    #[test]
    fn stable_id_is_scoped_to_the_agent_and_trimmed() {
        use super::super::conversation_id_for;
        let args = |list: &[&str]| -> Vec<std::ffi::OsString> {
            list.iter().map(std::ffi::OsString::from).collect()
        };
        let id = |list: &[&str]| conversation_id_for(Harness::OpenClaw, &args(list));
        let alpha = id(&["agent", "--agent", "alpha", "--session-id", "1234"]);
        assert_ne!(
            alpha,
            id(&["agent", "--agent", "beta", "--session-id", "1234"])
        );
        assert_eq!(
            alpha,
            id(&["agent", "--agent=alpha", "--session-id", " 1234 "])
        );
        // OpenClaw agent ids are case-insensitive.
        assert_eq!(
            alpha,
            id(&["agent", "--agent", " Alpha", "--session-id", "1234"])
        );
        assert_eq!(
            id(&["agent", "--session-id", " 1234"]),
            id(&["agent", "--session-id", "1234"])
        );
    }

    #[test]
    fn session_id_gives_a_stable_conversation_id() {
        use super::super::conversation_id_for;
        use super::stable_conversation_id;
        let id = stable_conversation_id("session-1");
        // `printf session-1 | shasum -a 256 | cut -c1-32`
        assert_eq!(id, "oc-84097828fc31a8c8d29210df48901a85");
        assert_ne!(id, stable_conversation_id("session-2"));
        let args: Vec<std::ffi::OsString> = ["agent", "--session-id", "session-1"]
            .iter()
            .map(std::ffi::OsString::from)
            .collect();
        assert_eq!(conversation_id_for(Harness::OpenClaw, &args), id);
        assert_eq!(conversation_id_for(Harness::OpenClaw, &args), id);
        // No session id: a fresh UUID per launch. Other harnesses ignore it.
        assert_eq!(conversation_id_for(Harness::OpenClaw, &[]).len(), 36);
        assert_eq!(conversation_id_for(Harness::Pi, &args).len(), 36);
    }
}
