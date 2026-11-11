//! Hermes Agent: `HERMES_HOME=<dir>` holds `config.yaml` (plus Hermes' own
//! state: sessions, memories, skills). `api_key` is the literal key and
//! `${VAR}` is expanded in config values.
//!
//! Episode keying: the Messages provider declares [`SESSION_AFFINITY_KEY`],
//! Hermes' opt-in per-provider key (from v2026.9.21), so Hermes sends a
//! conversation key as `x-session-affinity` on every request there, which the
//! router keys the episode on. Hermes resolves that key as its declared scope, else its
//! ambient conversation root. A session declares a scope (`gwk_…`, kept by
//! `-c` / `--resume`; seen on interactive, `-q` and `-z` runs of Hermes
//! 0.22.5), while a `delegate_task` child gets none and sends its parent's
//! root session id. The two differ, so the child's turns land outside the
//! parent's episode while the parent's title side call stays inside it;
//! every child of one parent shares that second episode. Where a session
//! has no declared scope it sends its own root, the same value as its
//! children, and they share one episode.
//! Older Hermes releases warn once per provider that the key is unknown and
//! then ignore it, so declaring it never fails a launch; they never applied
//! `extra_headers` on Messages either, so the per-launch id gave them no
//! identity on that wire.
//!
//! Neither provider sends the per-launch `x-conversation-id` unless
//! `RAYLINE_HARNESS_CONV_ID_FALLBACK=1` forces it, including under
//! `raydock hybrid run`: Hermes sends a session key on both wires (the header
//! on Messages, `prompt_cache_key` on Responses), so the execution core's
//! `session_identity_required` check is satisfied without a
//! per-launch id. A per-launch id would instead put a subagent, the parent and
//! the side call in one episode, since Hermes attaches a provider's
//! `extra_headers` to every request of the process and the gateway ranks
//! `x-conversation-id` ahead of `x-session-affinity`. Hermes runs turns in a
//! shared gateway process per `HERMES_HOME`, so a forced id reaches the wire
//! only when the launch starts that gateway; a gateway left running by an
//! earlier launch sends the literal `${RAYLINE_CONV_ID}` instead, which the
//! router then keys every such launch on. One Hermes call skips
//! the affinity merge: the summary it requests on reaching its iteration cap,
//! on Messages only. Under hybrid on `--provider rayline` the core
//! refuses that call and Hermes reports the cap with no summary.
//!
//! The `rayline-resp` provider (`--provider rayline-resp`, the default under
//! `raydock hybrid run`) speaks Responses and declares no header: Hermes sends
//! a `prompt_cache_key` there that is distinct per session and per delegate
//! child, which the router's ladder and the hybrid core both read as the
//! conversation, so each child lands in its own episode. A header would rank
//! above it and merge the children. Each entry has its own base_url because
//! Hermes looks up `extra_headers`, the affinity header and model
//! capabilities by base_url.
//!
//! Prompt caching: Hermes places Anthropic `cache_control` markers on a custom
//! Messages route only for a model whose name looks like Claude or whose entry
//! sets [`PROMPT_CACHING_KEY`]. The ARC names look like neither, so the
//! Messages models declare it; on Responses the router's cell places one
//! breakpoint itself.
//!
//! `config.yaml` also holds the user's Hermes settings (`hermes config set`,
//! `hermes tools`), so each launch merges only Rayline's keys ([`OWNED`]) into
//! the existing file and keeps every other line as written.
//!
//! Dependency environment (Windows): Hermes keeps the Python environment it
//! runs on per home, under `HERMES_HOME\installs`, and on Windows it refuses
//! to start in a home that has none ("no dependency environment is committed
//! for this install; run `hermes pm repair`"). Raydock's home starts empty,
//! and a plain `hermes pm repair` would repair the default home instead, so
//! before a Windows launch [`prepare_home`] builds the environment once with
//! `HERMES_HOME` set to Raydock's home. Later launches find it and skip the
//! repair.

use std::{
    fs, io,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

use super::{CONV_ID_ENV, ConfigFile, Plan, RenderContext, to_yaml, yaml_merge};

pub(crate) const CONFIG_FILE: &str = "config.yaml";
pub(crate) const MESSAGES_PROVIDER: &str = "rayline";
/// The Messages provider's name before 0.4.3, removed from existing configs.
const LEGACY_MESSAGES_PROVIDER: &str = "rayline-anth";
pub(crate) const RESPONSES_PROVIDER: &str = "rayline-resp";
/// Hermes' per-provider key naming the header its conversation id rides on.
pub(crate) const SESSION_AFFINITY_KEY: &str = "session_affinity_header";
/// Hermes' per-model capability that turns on Anthropic cache markers.
pub(crate) const PROMPT_CACHING_KEY: &str = "prompt_caching";

/// The keys Rayline sets in `config.yaml`. `fallback_providers` stays empty
/// and the legacy `fallback_model` is removed, so a failed turn never moves
/// off the router. The Messages provider's old name is removed too, so an
/// existing config lists it once, as `rayline`.
const OWNED: &[&[&str]] = &[
    &["model", "provider"],
    &["model", "default"],
    &["model", "context_length"],
    &["providers", MESSAGES_PROVIDER],
    &["providers", RESPONSES_PROVIDER],
    &["providers", LEGACY_MESSAGES_PROVIDER],
    &["fallback_providers"],
    &["fallback_model"],
];

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Result<Plan, String> {
    let launch = &ctx.models;
    // `prompt_caching` makes Hermes place its Anthropic cache markers for a
    // model whose name does not look like Claude; without it the router sees
    // no `cache_control` and nothing upstream adds one for a Messages client.
    let models = |prompt_caching: bool| -> serde_json::Map<_, _> {
        launch
            .offered
            .iter()
            .map(|model| {
                let mut entry = json!({ "context_length": model.context_window });
                if prompt_caching {
                    entry[PROMPT_CACHING_KEY] = json!(true);
                }
                (model.id.clone(), entry)
            })
            .collect()
    };
    // Responses: no affinity header. Hermes sends a per-session (and per
    // delegate child) `prompt_cache_key` there, which the router and the hybrid
    // core both read as the conversation; the header would merge the children.
    let mut responses = json!({
        "base_url": ctx.v1_base(),
        "api_key": ctx.key,
        "api_mode": "codex_responses",
        "models": models(false)
    });
    // Messages: the header is the only identity Hermes can send on this wire.
    let mut messages = json!({
        "base_url": ctx.messages_base(),
        "api_key": ctx.key,
        "api_mode": "anthropic_messages",
        "models": models(true),
        SESSION_AFFINITY_KEY: super::SESSION_AFFINITY_HEADER
    });
    // Both wires carry a Hermes session key (the header on Messages,
    // `prompt_cache_key` on Responses), so the per-launch id is opt-in on
    // both, with no hybrid exception.
    if ctx.conversation_id.enabled() {
        let header = json!({ super::CONV_HEADER: format!("${{{CONV_ID_ENV}}}") });
        responses["extra_headers"] = header.clone();
        messages["extra_headers"] = header;
    }
    // Under hybrid routing the default is Responses: on Messages Hermes always
    // asks for its model's full output ceiling (128k for an unknown model; it
    // has no setting to lower it), which the execution core refuses. On
    // Responses it sends no output cap and the core applies its own.
    let default_provider = if ctx.capabilities.is_some() {
        RESPONSES_PROVIDER
    } else {
        MESSAGES_PROVIDER
    };
    let config = json!({
        "model": {
            "provider": format!("custom:{default_provider}"),
            "default": launch.main,
            "context_length": launch.main_model().context_window
        },
        "providers": {
            MESSAGES_PROVIDER: messages,
            RESPONSES_PROVIDER: responses
        },
        "fallback_providers": []
    });
    let path = ctx.config_dir.join(CONFIG_FILE);
    let (contents, merged_from) = match fs::read_to_string(&path) {
        Ok(existing) => {
            let merged = yaml_merge::merge(&existing, &config, OWNED).map_err(|error| {
                format!(
                    "cannot update {} ({error}); fix or remove the file",
                    path.display()
                )
            })?;
            (merged, Some(existing))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => (to_yaml(&config), None),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    Ok(Plan {
        files: vec![ConfigFile {
            name: CONFIG_FILE,
            contents,
            overwrite: true,
            merged_from,
            replaces_seed: None,
        }],
        env_set: vec![("HERMES_HOME", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    })
}

/// Hermes' per-home dependency state: one `installs/<install key>` directory per
/// Hermes checkout, whose `facts.json` names the committed environment.
const INSTALLS_DIR: &str = "installs";
const FACTS_FILE: &str = "facts.json";
/// What Hermes tells the user to run when its home has no environment.
const REPAIR_COMMAND: &str = "hermes pm repair";
/// The most lines of a failed repair's output repeated in the error.
const FAILED_OUTPUT_LINES: usize = 12;

/// A finished Hermes run: whether it exited 0, and its stdout then stderr.
pub(crate) struct Finished {
    pub success: bool,
    pub output: String,
}

/// Runs a prepared Hermes command to completion with no stdin, capturing its
/// output.
pub(crate) fn finish(mut command: Command) -> io::Result<Finished> {
    let output = command.stdin(Stdio::null()).output()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(Finished {
        success: output.status.success(),
        output: text,
    })
}

/// Makes sure Hermes can start in `home` (see the module docs). `run` runs
/// Hermes with the given args and `HERMES_HOME=home`.
///
/// A home that records an environment is used as is, with no Hermes run.
/// Otherwise a cheap `hermes --version` asks Hermes itself: only when it
/// refuses and names `hermes pm repair` does the repair run, so an install
/// that ships its own environment is left alone. A probe that cannot run is
/// not an error here; the launch reports it.
pub(crate) fn prepare_home(
    home: &Path,
    mut run: impl FnMut(&[&str]) -> io::Result<Finished>,
) -> Result<(), String> {
    if has_dependency_environment(home) {
        return Ok(());
    }
    match run(&["--version"]) {
        Ok(probe) if !probe.success && probe.output.contains(REPAIR_COMMAND) => {}
        _ => return Ok(()),
    }
    eprintln!("  Setting up Hermes for Raydock (one time; this can take a few minutes)...");
    match run(&["pm", "repair"]) {
        Ok(repair) if repair.success => {
            eprintln!("  Hermes is set up.");
            Ok(())
        }
        Ok(repair) => Err(repair_failed(home, &last_lines(&repair.output))),
        Err(error) => Err(repair_failed(home, &format!("    {error}"))),
    }
}

/// Whether `home` records a Hermes dependency environment that still exists.
fn has_dependency_environment(home: &Path) -> bool {
    let Ok(installs) = fs::read_dir(home.join(INSTALLS_DIR)) else {
        return false;
    };
    installs
        .flatten()
        .any(|install| records_environment(&install.path().join(FACTS_FILE)))
}

fn records_environment(facts: &Path) -> bool {
    let Ok(text) = fs::read_to_string(facts) else {
        return false;
    };
    let Ok(facts) = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}')) else {
        return false;
    };
    facts
        .pointer("/packages/venv/environment")
        .and_then(Value::as_str)
        .is_some_and(|environment| Path::new(environment).join("pyvenv.cfg").is_file())
}

fn last_lines(output: &str) -> String {
    let lines: Vec<&str> = output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    lines[lines.len().saturating_sub(FAILED_OUTPUT_LINES)..]
        .iter()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The error for a failed repair: Hermes' last output, then the command to run
/// by hand in PowerShell and in Command Prompt, each scoped to Raydock's home
/// so the user's own Hermes home is untouched afterwards.
fn repair_failed(home: &Path, output: &str) -> String {
    let home = home.display().to_string();
    let powershell = home.replace('\'', "''");
    format!(
        "Hermes setup failed: `{REPAIR_COMMAND}` did not finish for Raydock's Hermes home.\n\
         {output}\n\
         Run it yourself with that home, then launch again:\n\
         \x20 PowerShell:     $env:HERMES_HOME = '{powershell}'; {REPAIR_COMMAND}; Remove-Item Env:HERMES_HOME\n\
         \x20 Command Prompt: set \"HERMES_HOME={home}\" & {REPAIR_COMMAND} & set \"HERMES_HOME=\""
    )
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{env_value, render};
    use super::CONFIG_FILE;

    /// Both providers treat the per-launch id the same way: only when forced.
    #[test]
    fn neither_provider_carries_the_conversation_id_unless_forced() {
        use super::super::ConversationIdFallback;
        use super::super::tests::render_with;
        let headers = |fallback| {
            let (dir, _) = render_with(Harness::Hermes, fallback);
            let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
            let messages = config
                .split("  rayline-resp:")
                .next()
                .unwrap()
                .contains("x-conversation-id: \"${RAYLINE_CONV_ID}\"");
            let responses = config
                .split("  rayline-resp:")
                .nth(1)
                .unwrap()
                .contains("x-conversation-id: \"${RAYLINE_CONV_ID}\"");
            (messages, responses)
        };
        // Neither provider sends the per-launch id unless forced: Messages
        // declares session_affinity_header and Responses relies on Hermes'
        // prompt_cache_key, so Hermes' own keys drive the episode.
        assert_eq!(headers(ConversationIdFallback::new(None)), (false, false));
        assert_eq!(
            headers(ConversationIdFallback::new(Some("1"))),
            (true, true)
        );
        assert_eq!(
            headers(ConversationIdFallback::new(Some("0"))),
            (false, false)
        );
    }

    /// Under hybrid the rendered config carries no per-launch id: the Messages
    /// provider declares the header and the Responses provider relies on
    /// Hermes' `prompt_cache_key`, which the core reads as identity. (The one
    /// Hermes request that sends neither is documented in the module docs;
    /// this checks the config contract, not runtime identity.)
    #[test]
    fn hybrid_config_carries_no_per_launch_id() {
        use super::super::tests::{TEST_KEY, TEST_URL, temp_dir};
        use super::super::{ConversationIdFallback, RenderContext, write_files};
        let dir = temp_dir("hermes-hybrid");
        let capabilities = rayline_local_router::ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 8192,
            input_modalities: vec!["text".into()],
        };
        let ctx = RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: ConversationIdFallback { forced: None },
            allow_empty_signature: true,
            capabilities: Some(&capabilities),
            harness_major_version: None,
            models: super::super::LaunchModels::hybrid(&capabilities),
        };
        write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        let (messages, responses) = config.split_once("  rayline-resp:").unwrap();
        assert!(!config.contains("x-conversation-id"));
        assert!(
            messages.contains("session_affinity_header: \"x-session-affinity\""),
            "{messages}"
        );
        assert!(
            !responses.contains("session_affinity_header"),
            "{responses}"
        );
        assert!(config.contains("provider: \"custom:rayline-resp\""));
    }

    /// Turning the fallback off after a forced launch drops the stale
    /// `extra_headers` from both providers in place: each provider map is
    /// owned whole, so the merge replaces it rather than keeping old children.
    #[test]
    fn unforcing_removes_the_per_launch_header_in_place() {
        use super::super::tests::{TEST_KEY, TEST_URL, temp_dir};
        use super::super::{ConversationIdFallback, RenderContext, write_files};
        let dir = temp_dir("hermes-toggle");
        let render = |forced: Option<&str>| {
            let ctx = RenderContext {
                key: TEST_KEY,
                router_url: TEST_URL,
                config_dir: &dir,
                conversation_id: ConversationIdFallback::new(forced),
                allow_empty_signature: false,
                capabilities: None,
                harness_major_version: None,
                models: super::super::tests::hosted_models(),
            };
            write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
            std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap()
        };
        assert_eq!(render(Some("1")).matches("x-conversation-id").count(), 2);
        let off = render(None);
        assert!(
            !off.contains("x-conversation-id") && !off.contains("extra_headers"),
            "{off}"
        );
        assert_eq!(render(None), off);
    }

    /// Launch Hermes twice over a config Hermes and the user already wrote.
    #[test]
    fn an_edit_between_merge_and_write_is_kept() {
        use super::super::tests::{TEST_URL, temp_dir};
        use super::super::{ConversationIdFallback, RenderContext, write_files};
        let dir = temp_dir("hermes-race");
        let path = dir.join(CONFIG_FILE);
        std::fs::write(&path, "agent:\n  max_turns: 40\n").unwrap();
        let ctx = RenderContext {
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: ConversationIdFallback::new(None),
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: None,
            models: super::super::tests::hosted_models(),
        };
        let plan = super::plan(&ctx).unwrap();
        // Hermes saves a setting after Rayline read the file.
        std::fs::write(&path, "agent:\n  max_turns: 60\n").unwrap();
        let error = write_files(&dir, &plan.files).unwrap_err();
        assert!(
            error.to_string().contains("run the command again"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "agent:\n  max_turns: 60\n"
        );
    }

    /// The seed holds the Messages provider under its pre-0.4.3 name
    /// (`rayline-anth`); the launch removes it and writes `rayline`.
    #[test]
    fn relaunch_keeps_user_settings_and_is_idempotent() {
        use super::super::tests::{TEST_URL, temp_dir};
        use super::super::{ConversationIdFallback, RenderContext, write_files};
        let dir = temp_dir("hermes-merge");
        let path = dir.join(CONFIG_FILE);
        std::fs::write(
            &path,
            "model:
  default: claude-opus
  provider: anthropic
  reasoning_effort: high
providers:
  rayline-anth:
    base_url: https://stale.example
    models:
      arc-sage-1.0:
        prompt_caching: false
  mine:
    base_url: https://mine.example
agent:
  max_turns: 40
  disabled_toolsets:
  - web
  - browser
fallback_providers:
- mine
fallback_model:
  provider: openrouter
  model: external
",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let launch = || {
            let ctx = RenderContext {
                key: super::super::tests::TEST_KEY,
                router_url: TEST_URL,
                config_dir: &dir,
                conversation_id: ConversationIdFallback::new(None),
                allow_empty_signature: false,
                capabilities: None,
                harness_major_version: None,
                models: super::super::tests::hosted_models(),
            };
            write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
            std::fs::read_to_string(&path).unwrap()
        };
        let first = launch();
        assert_eq!(
            first,
            "model:
  default: \"arc-sage-1.0\"
  provider: \"custom:rayline\"
  reasoning_effort: high
  context_length: 1000000
providers:
  mine:
    base_url: https://mine.example
  rayline:
    api_key: \"rlk-test\"
    api_mode: \"anthropic_messages\"
    base_url: \"https://api-dev.rayline.ai\"
    models:
      arc-sage-1.0:
        context_length: 1000000
        prompt_caching: true
      arc-swift-1.0:
        context_length: 1000000
        prompt_caching: true
    session_affinity_header: \"x-session-affinity\"
  rayline-resp:
    api_key: \"rlk-test\"
    api_mode: \"codex_responses\"
    base_url: \"https://api-dev.rayline.ai/v1\"
    models:
      arc-sage-1.0:
        context_length: 1000000
      arc-swift-1.0:
        context_length: 1000000
agent:
  max_turns: 40
  disabled_toolsets:
  - web
  - browser
fallback_providers: []
"
        );
        assert_eq!(launch(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn unreadable_config_is_an_error_not_a_rewrite() {
        use super::super::tests::{TEST_URL, temp_dir};
        use super::super::{ConversationIdFallback, RenderContext};
        let dir = temp_dir("hermes-bad");
        std::fs::write(dir.join(CONFIG_FILE), "---\nmodel: x\n").unwrap();
        let ctx = RenderContext {
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: ConversationIdFallback::new(None),
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: None,
            models: super::super::tests::hosted_models(),
        };
        let error = super::plan(&ctx).unwrap_err();
        assert!(error.contains("fix or remove the file"), "{error}");
    }

    #[test]
    fn renders_messages_default_and_responses_provider() {
        let (dir, plan) = render(Harness::Hermes);
        assert_eq!(
            env_value(&plan, "HERMES_HOME"),
            Some(&dir.clone().into_os_string())
        );
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert_eq!(
            config,
            "fallback_providers: []
model:
  context_length: 1000000
  default: \"arc-sage-1.0\"
  provider: \"custom:rayline\"
providers:
  rayline:
    api_key: \"rlk-test\"
    api_mode: \"anthropic_messages\"
    base_url: \"https://api-dev.rayline.ai\"
    models:
      arc-sage-1.0:
        context_length: 1000000
        prompt_caching: true
      arc-swift-1.0:
        context_length: 1000000
        prompt_caching: true
    session_affinity_header: \"x-session-affinity\"
  rayline-resp:
    api_key: \"rlk-test\"
    api_mode: \"codex_responses\"
    base_url: \"https://api-dev.rayline.ai/v1\"
    models:
      arc-sage-1.0:
        context_length: 1000000
      arc-swift-1.0:
        context_length: 1000000
"
        );
    }

    mod prepare_home {
        use super::super::super::tests::temp_dir;
        use super::super::{Finished, prepare_home};
        use std::path::Path;

        const REFUSAL: &str = "hermes: no dependency environment is committed for this \
                               install; run `hermes pm repair`";

        /// Lays out `installs/<key>/facts.json` the way `hermes pm repair`
        /// leaves it, with the recorded venv present.
        fn commit_environment(home: &Path) {
            let install = home.join("installs").join("d74c6fbcc1df5714");
            let venv = install.join("environments").join("e1").join("venv");
            std::fs::create_dir_all(&venv).unwrap();
            std::fs::write(venv.join("pyvenv.cfg"), "home = x\n").unwrap();
            let facts = serde_json::json!({
                "packages": { "venv": { "environment": venv.to_string_lossy() } },
                "schema": 1
            });
            std::fs::write(install.join("facts.json"), facts.to_string()).unwrap();
        }

        fn finished(success: bool, output: &str) -> std::io::Result<Finished> {
            Ok(Finished {
                success,
                output: output.to_owned(),
            })
        }

        /// A clean home: Hermes refuses, so the repair runs once; the next
        /// launch finds the environment and runs nothing.
        #[test]
        fn a_clean_home_is_repaired_once() {
            let home = temp_dir("hermes-clean-home");
            let mut calls = Vec::new();
            prepare_home(&home, |args| {
                calls.push(args.join(" "));
                if args == ["pm", "repair"] {
                    commit_environment(&home);
                    finished(true, "hermes: dependency environment repaired")
                } else {
                    finished(false, REFUSAL)
                }
            })
            .unwrap();
            assert_eq!(calls, ["--version", "pm repair"]);

            let mut again = Vec::new();
            prepare_home(&home, |args| {
                again.push(args.join(" "));
                finished(true, "")
            })
            .unwrap();
            assert!(again.is_empty(), "{again:?}");
        }

        /// Facts that name a missing venv do not count as an environment.
        #[test]
        fn a_recorded_but_missing_venv_is_repaired() {
            let home = temp_dir("hermes-missing-venv");
            commit_environment(&home);
            let install = home.join("installs").join("d74c6fbcc1df5714");
            std::fs::remove_dir_all(install.join("environments")).unwrap();
            let mut calls = Vec::new();
            prepare_home(&home, |args| {
                calls.push(args.join(" "));
                finished(args != ["--version"], REFUSAL)
            })
            .unwrap();
            assert_eq!(calls, ["--version", "pm repair"]);
        }

        /// An install that starts without a per-home environment (or fails for
        /// another reason) is not repaired; the launch goes ahead and Hermes
        /// reports any other failure itself.
        #[test]
        fn no_repair_unless_hermes_asks_for_it() {
            let home = temp_dir("hermes-no-ask");
            for probe in [
                (true, "Hermes Agent v0.23.1"),
                (false, "some other failure"),
            ] {
                let mut calls = Vec::new();
                prepare_home(&home, |args| {
                    calls.push(args.join(" "));
                    finished(probe.0, probe.1)
                })
                .unwrap();
                assert_eq!(calls, ["--version"]);
            }
            let mut calls = Vec::new();
            prepare_home(&home, |args| {
                calls.push(args.join(" "));
                Err(std::io::Error::other("not found"))
            })
            .unwrap();
            assert_eq!(calls, ["--version"]);
        }

        /// A failed repair stops the launch and names the exact command, with
        /// Raydock's home, for both Windows shells.
        #[test]
        fn a_failed_repair_prints_the_command_with_the_home() {
            let home = temp_dir("hermes-repair-fails").join("it's");
            let output = (1..=20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            let error = prepare_home(&home, |args| {
                if args == ["pm", "repair"] {
                    finished(false, &output)
                } else {
                    finished(false, REFUSAL)
                }
            })
            .unwrap_err();
            let shown = home.display().to_string();
            assert!(error.contains("    line 20"), "{error}");
            assert!(!error.contains("line 8\n"), "{error}");
            assert!(
                error.contains(&format!(
                    "$env:HERMES_HOME = '{}'; hermes pm repair; Remove-Item Env:HERMES_HOME",
                    shown.replace('\'', "''")
                )),
                "{error}"
            );
            assert!(
                error.contains(&format!(
                    "set \"HERMES_HOME={shown}\" & hermes pm repair & set \"HERMES_HOME=\""
                )),
                "{error}"
            );

            let error = prepare_home(&home, |args| {
                if args == ["pm", "repair"] {
                    Err(std::io::Error::other("cannot start"))
                } else {
                    finished(false, REFUSAL)
                }
            })
            .unwrap_err();
            assert!(error.contains("    cannot start"), "{error}");
        }
    }
}
