//! opencode: `OPENCODE_CONFIG=<dir>/opencode.json`. opencode merges this file
//! over the user's global config (project configs still win). `{env:VAR}` is
//! opencode's config substitution; the key is written as a literal `apiKey`.
//!
//! opencode sends its session id as `x-session-affinity` on its own, which
//! the gateway keys the episode on. Against the hosted router, `opencode -c` /
//! `--session` keeps that episode; `raydock hybrid run opencode` refuses them,
//! since a saved session lacks the execution core's private history.
//!
//! OpenCode 2 runs its agent in a shared background service (port 49374) that
//! the CLI attaches to. The service was started with its own environment, so
//! it never sees this launch's `OPENCODE_CONFIG` and the Rayline provider does
//! not exist there. [`launch_args`] adds `--standalone` (a private server that
//! inherits this environment) to the commands that would attach.
//!
//! OpenCode 2 rejects the model entry's legacy `attachment` and `reasoning`
//! flags (image support comes from `modalities.input`); OpenCode 1 gates
//! image attachments and reasoning on them, so they are written only for a
//! known OpenCode 1.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use super::{CONV_ID_ENV, Plan, ROUTER_MODEL, RenderContext, json_file};

pub(crate) const CONFIG_FILE: &str = "opencode.json";
pub(crate) const PROVIDER_ID: &str = "rayline";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let model_ref = format!("{PROVIDER_ID}/{ROUTER_MODEL}");
    let mut config = json!({
        "$schema": "https://opencode.ai/config.json",
        "model": model_ref,
        // The session-title side call otherwise goes to the user's global
        // `small_model` (another provider) or a guessed small model. Keep it on
        // Rayline. The router treats it as a one-turn side call off the
        // session's episode.
        "small_model": model_ref,
        "provider": {
            PROVIDER_ID: {
                "name": format!("Raydock ({})", ctx.env_name),
                // opencode's default for custom providers (chat completions)
                // 404s at the router, so use Messages.
                "npm": "@ai-sdk/anthropic",
                "options": {
                    "baseURL": ctx.v1_base(),
                    "apiKey": ctx.key,
                    "headers": ctx.conversation_headers(&format!("{{env:{CONV_ID_ENV}}}")),
                },
                "models": {
                    ROUTER_MODEL: {
                        "name": "Raydock router",
                        "limit": { "context": ctx.context_window(), "output": ctx.max_output_tokens() },
                        "modalities": { "input": ctx.input_modalities(), "output": ["text"] },
                        "tool_call": true
                    }
                }
            }
        }
    });
    if ctx.harness_major_version.is_some_and(|major| major < 2) {
        let model = &mut config["provider"][PROVIDER_ID]["models"][ROUTER_MODEL];
        let images = ctx
            .input_modalities()
            .as_array()
            .is_some_and(|input| input.iter().any(|modality| modality == "image"));
        model["attachment"] = json!(images);
        model["reasoning"] = json!(true);
    }
    Plan {
        files: vec![json_file(CONFIG_FILE, &config, true)],
        env_set: vec![(
            "OPENCODE_CONFIG",
            ctx.config_dir.join(CONFIG_FILE).into_os_string(),
        )],
        env_remove: Vec::new(),
    }
}

/// Subcommands that attach to OpenCode 2's background service and accept
/// `--standalone`. The bare command (the TUI, optionally given a directory)
/// does too.
const ATTACHING_SUBCOMMANDS: &[&str] = &["run", "mini", "models"];
/// Every other OpenCode subcommand: left untouched.
const OTHER_SUBCOMMANDS: &[&str] = &[
    "upgrade",
    "update",
    "uninstall",
    "acp",
    "api",
    "debug",
    "auth",
    "mcp",
    "plugin",
    "stats",
    "session",
    "service",
    "reload",
    "pair",
    "serve",
    "help",
];
/// Root flags that take a value.
const VALUE_FLAGS: &[&str] = &[
    "--log-level",
    "--completions",
    "--prompt",
    "--session",
    "-s",
    "--model",
    "-m",
    "--agent",
    "--port",
    "--hostname",
    "--file",
    "-f",
    "--title",
    "--format",
];

/// The major version `opencode --version` reports (`opencode v2.0.25`), or
/// None when it cannot be read.
pub(crate) fn major_version(binary: &Path) -> Option<u64> {
    let mut command = Command::new(binary);
    crate::codex::withhold_router_key(&mut command);
    let output = command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_major(&String::from_utf8_lossy(&output.stdout))
}

fn parse_major(text: &str) -> Option<u64> {
    text.split_whitespace()
        .map(|token| token.trim_start_matches('v'))
        .find(|token| token.contains('.'))
        .and_then(|token| token.split('.').next())
        .and_then(|major| major.parse().ok())
}

/// The args OpenCode runs with: `--standalone` added where OpenCode 2 would
/// otherwise attach to its background service (see the module docs). Left
/// as given for OpenCode 1 or an unknown version, for any other subcommand,
/// and when the args already pick a server (`--standalone`, `--server`).
pub(crate) fn launch_args(args: &[OsString], major_version: Option<u64>) -> Vec<OsString> {
    let mut args = args.to_vec();
    if major_version.is_none_or(|major| major < 2) {
        return args;
    }
    let before_separator = || args.iter().take_while(|arg| *arg != "--");
    let picks_server = before_separator().any(|arg| {
        let arg = arg.to_string_lossy();
        arg == "--standalone" || arg == "--server" || arg.starts_with("--server=")
    });
    if picks_server {
        return args;
    }
    // The first positional word, skipping root flags and their values.
    let mut index = 0;
    let mut command = None;
    while let Some(arg) = args.get(index).map(|arg| arg.to_string_lossy()) {
        if arg == "--" {
            break;
        }
        if VALUE_FLAGS.contains(&arg.as_ref()) {
            index += 2;
            continue;
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        command = Some((index, arg.into_owned()));
        break;
    }
    match command {
        Some((index, word)) if ATTACHING_SUBCOMMANDS.contains(&word.as_str()) => {
            args.insert(index + 1, OsString::from("--standalone"));
        }
        Some((_, word)) if OTHER_SUBCOMMANDS.contains(&word.as_str()) => {}
        // The TUI, with or without a directory argument.
        _ => args.insert(0, OsString::from("--standalone")),
    }
    args
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_KEY, TEST_URL, env_value, render};
    use super::*;

    #[test]
    fn renders_messages_provider_with_the_key() {
        let (dir, plan) = render(Harness::OpenCode);
        let path = dir.join(CONFIG_FILE);
        assert_eq!(
            env_value(&plan, "OPENCODE_CONFIG"),
            Some(&path.clone().into_os_string())
        );
        let config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(config["model"], "rayline/rayline-router");
        assert_eq!(config["small_model"], "rayline/rayline-router");
        let provider = &config["provider"]["rayline"];
        assert_eq!(provider["npm"], "@ai-sdk/anthropic");
        assert_eq!(provider["options"]["baseURL"], format!("{TEST_URL}/v1"));
        assert_eq!(provider["options"]["apiKey"], TEST_KEY);
        assert_eq!(provider["options"]["headers"], serde_json::json!({}));
        let model = &provider["models"]["rayline-router"];
        assert_eq!(model["limit"]["context"], 200000);
        assert_eq!(model["limit"]["output"], 32000);
        assert_eq!(
            model["modalities"]["input"],
            serde_json::json!(["text", "image"])
        );
        assert_eq!(model["tool_call"], true);
        // OpenCode 2 rejects these legacy keys.
        assert!(model.get("attachment").is_none() && model.get("reasoning").is_none());
    }

    fn strings(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn opencode_2_runs_standalone() {
        let cases: &[(&[&str], &[&str])] = &[
            (&[], &["--standalone"]),
            (&["."], &["--standalone", "."]),
            (&["run", "hi"], &["run", "--standalone", "hi"]),
            (
                &["--log-level", "info", "run", "hi"],
                &["--log-level", "info", "run", "--standalone", "hi"],
            ),
            (&["models"], &["models", "--standalone"]),
            (&["mini"], &["mini", "--standalone"]),
            (
                &["--prompt", "run tests"],
                &["--standalone", "--prompt", "run tests"],
            ),
            (
                &["-m", "rayline/rayline-router", "run", "hi"],
                &["-m", "rayline/rayline-router", "run", "--standalone", "hi"],
            ),
            (&["--agent", "build"], &["--standalone", "--agent", "build"]),
            // Already picks a server, or a command that never attaches.
            (
                &["run", "--standalone", "hi"],
                &["run", "--standalone", "hi"],
            ),
            (
                &["run", "--server=http://x", "hi"],
                &["run", "--server=http://x", "hi"],
            ),
            (&["auth", "list"], &["auth", "list"]),
            (&["service", "stop"], &["service", "stop"]),
        ];
        for (given, expected) in cases {
            assert_eq!(
                launch_args(&strings(given), Some(2)),
                strings(expected),
                "{given:?}"
            );
        }
        // OpenCode 1 has no background service; an unknown version is left alone.
        for version in [Some(1), None] {
            assert_eq!(
                launch_args(&strings(&["run", "hi"]), version),
                strings(&["run", "hi"])
            );
        }
    }

    #[test]
    fn opencode_1_keeps_its_attachment_and_reasoning_flags() {
        let dir = super::super::tests::temp_dir("opencode-1");
        let ctx = RenderContext {
            env_name: "dev",
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: super::super::ConversationIdFallback::new("dev", None),
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: Some(1),
        };
        let config: serde_json::Value =
            serde_json::from_str(&plan(&ctx).files[0].contents).unwrap();
        let model = &config["provider"]["rayline"]["models"]["rayline-router"];
        assert_eq!(
            (&model["attachment"], &model["reasoning"]),
            (&serde_json::json!(true), &serde_json::json!(true))
        );
    }

    #[test]
    fn parses_the_version_banner() {
        assert_eq!(parse_major("opencode v2.0.25\n"), Some(2));
        assert_eq!(parse_major("1.14.3"), Some(1));
        assert_eq!(parse_major("no version"), None);
    }
}
