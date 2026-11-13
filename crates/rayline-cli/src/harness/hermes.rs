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
//! `rayline run <harness> --local`: Hermes sends a session key on both wires (the header
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
//! on Messages only.
//!
//! The `rayline-resp` provider (`--provider rayline-resp`, the only provider
//! under `rayline run <harness> --local`) speaks Responses and declares no header: Hermes sends
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
//! before a Windows launch [`prepare_launch`] builds the environment once with
//! `HERMES_HOME` set to Raydock's home. Later launches of the same Hermes find
//! it and skip the repair.

use std::{
    fs, io,
    path::Path,
    process::{Command, Stdio},
};

use serde_json::{Value, json};

use super::{CONV_ID_ENV, ConfigFile, Plan, RenderContext, to_yaml, yaml_merge};

pub(crate) const CONFIG_FILE: &str = "config.yaml";
pub(crate) const MESSAGES_PROVIDER: &str = "rayline";
pub(crate) const RESPONSES_PROVIDER: &str = "rayline-resp";
/// Hermes' per-provider key naming the header its conversation id rides on.
pub(crate) const SESSION_AFFINITY_KEY: &str = "session_affinity_header";
/// Hermes' per-model capability that turns on Anthropic cache markers.
pub(crate) const PROMPT_CACHING_KEY: &str = "prompt_caching";

/// The keys Rayline sets in `config.yaml`. `fallback_providers` stays empty
/// and `fallback_model` is removed, so a failed turn never moves off the
/// router.
const OWNED: &[&[&str]] = &[
    &["model", "provider"],
    &["model", "default"],
    &["model", "context_length"],
    &["providers", MESSAGES_PROVIDER],
    &["providers", RESPONSES_PROVIDER],
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
    // In local mode Hermes speaks only Responses, its one protocol (spec
    // section 5.2), so the Messages provider is not written.
    let (default_provider, providers) = if ctx.capabilities.is_some() {
        (RESPONSES_PROVIDER, json!({ RESPONSES_PROVIDER: responses }))
    } else {
        (
            MESSAGES_PROVIDER,
            json!({ MESSAGES_PROVIDER: messages, RESPONSES_PROVIDER: responses }),
        )
    };
    let config = json!({
        "model": {
            "provider": format!("custom:{default_provider}"),
            "default": launch.main,
            "context_length": launch.main_model().context_window
        },
        "providers": providers,
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
        }],
        env_set: vec![("HERMES_HOME", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    })
}

/// Hermes' per-home dependency state: one `installs/<install key>` directory per
/// Hermes checkout, whose `facts.json` names the committed environment.
const INSTALLS_DIR: &str = "installs";
const FACTS_FILE: &str = "facts.json";
/// Raydock's record, in its Hermes home, of the Hermes launcher that last
/// started there and the environments the home held then (see
/// [`prepare_home`]).
const READY_MARKER: &str = ".raydock-hermes-ready";
/// What Hermes tells the user to run when its home has no environment.
const REPAIR_COMMAND: &str = "hermes pm repair";
/// The most lines of a failed repair's output repeated in the error.
const FAILED_OUTPUT_LINES: usize = 12;

/// A finished Hermes run: whether it exited 0, and its stdout then stderr.
pub(crate) struct Finished {
    pub success: bool,
    pub output: String,
}

/// Before a Windows launch of `command` (the Hermes launch itself), makes sure
/// Hermes can start in the home the command sets as `HERMES_HOME`. The probe
/// and the repair run the same program with the same environment changes.
pub(crate) fn prepare_launch(command: &Command) -> Result<(), String> {
    let Some(home) = command
        .get_envs()
        .find(|(name, _)| *name == "HERMES_HOME")
        .and_then(|(_, value)| value)
    else {
        return Ok(());
    };
    let launcher = launcher_identity(Path::new(command.get_program()));
    prepare_home(Path::new(home), launcher.as_deref(), |args| {
        finish(sibling(command, args))
    })
}

/// `command`'s program and environment changes with other `args`.
fn sibling(command: &Command, args: &[&str]) -> Command {
    let mut sibling = Command::new(command.get_program());
    sibling.args(args);
    for (name, value) in command.get_envs() {
        match value {
            Some(value) => sibling.env(name, value),
            None => sibling.env_remove(name),
        };
    }
    if let Some(dir) = command.get_current_dir() {
        sibling.current_dir(dir);
    }
    sibling
}

/// Runs a Hermes command to completion with no stdin, capturing its output.
fn finish(mut command: Command) -> io::Result<Finished> {
    let output = command.stdin(Stdio::null()).output()?;
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(Finished {
        success: output.status.success(),
        output: text,
    })
}

/// The launcher's resolved path, size and modification time. Hermes keys its
/// per-home state on the checkout the launcher runs, and a launcher for another
/// checkout (moved or reinstalled Hermes, another `hermes` first on PATH) is a
/// different file or a rewritten one. `None` when it cannot be read.
fn launcher_identity(program: &Path) -> Option<String> {
    let path = fs::canonicalize(program).ok()?;
    let meta = fs::metadata(&path).ok()?;
    let modified = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some(format!(
        "{}\n{}\n{}.{:09}\n",
        path.display(),
        meta.len(),
        modified.as_secs(),
        modified.subsec_nanos()
    ))
}

/// Makes sure Hermes can start in `home` (see the module docs). `run` runs
/// Hermes with the given args and `HERMES_HOME=home`; `launcher` is the
/// launcher's identity ([`launcher_identity`]).
///
/// The home is used as is, with no Hermes run, when the same launcher started
/// there before and the home still holds exactly the environments it held
/// then (so none of them, the launcher's own included, has gone or been
/// replaced). Otherwise a cheap
/// `hermes --version` asks Hermes itself: only when it refuses and names
/// `hermes pm repair` does the repair run, so an install that ships its own
/// environment is left alone. A probe that cannot run is not an error here;
/// the launch reports it.
pub(crate) fn prepare_home(
    home: &Path,
    launcher: Option<&str>,
    mut run: impl FnMut(&[&str]) -> io::Result<Finished>,
) -> Result<(), String> {
    let marker = home.join(READY_MARKER);
    let record = || -> Result<Option<String>, String> {
        let Some(launcher) = launcher else {
            return Ok(None);
        };
        let environments = environments(home)?;
        Ok((!environments.is_empty()).then(|| format!("{launcher}{}\n", environments.join("\n"))))
    };
    if let Some(record) = record()?
        && fs::read_to_string(&marker).is_ok_and(|seen| seen == record)
    {
        return Ok(());
    }
    let remember = || -> Result<(), String> {
        match record()? {
            Some(record) => fs::write(&marker, record)
                .map_err(|error| format!("failed to write {}: {error}", marker.display())),
            None => match fs::remove_file(&marker) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    Err(format!("failed to remove {}: {error}", marker.display()))
                }
                _ => Ok(()),
            },
        }
    };
    match run(&["--version"]) {
        Ok(probe) if probe.success => {
            remember()?;
            return Ok(());
        }
        Ok(probe) if probe.output.contains(REPAIR_COMMAND) => {}
        _ => return Ok(()),
    }
    eprintln!(
        "  Setting up Hermes for {} (one time; this can take a few minutes)...",
        crate::DISPLAY_NAME
    );
    match run(&["pm", "repair"]) {
        Ok(repair) if repair.success => {
            remember()?;
            eprintln!("  Hermes is set up.");
            Ok(())
        }
        Ok(repair) => Err(repair_failed(home, &last_lines(&repair.output))),
        Err(error) => Err(repair_failed(home, &format!("    {error}"))),
    }
}

/// The Hermes dependency environments `home` records that still exist, one
/// per install that has one, sorted. A home with no installs dir has none.
fn environments(home: &Path) -> Result<Vec<String>, String> {
    let dir = home.join(INSTALLS_DIR);
    let read_error = |error: io::Error| format!("failed to read {}: {error}", dir.display());
    let installs = match fs::read_dir(&dir) {
        Ok(installs) => installs,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(read_error(error)),
    };
    let mut environments = Vec::new();
    for install in installs {
        let facts = install.map_err(read_error)?.path().join(FACTS_FILE);
        environments.extend(recorded_environment(&facts)?);
    }
    environments.sort();
    Ok(environments)
}

/// The environment an install's facts file records, if it records one and its
/// venv is there. An install with no facts file has none; a facts file that
/// cannot be read or is not JSON is an error.
fn recorded_environment(facts: &Path) -> Result<Option<String>, String> {
    let text = match fs::read_to_string(facts) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("failed to read {}: {error}", facts.display())),
    };
    let value = serde_json::from_str::<Value>(text.trim_start_matches('\u{feff}'))
        .map_err(|error| format!("invalid JSON in {}: {error}", facts.display()))?;
    let Some(environment) = value.pointer("/packages/venv/environment") else {
        return Ok(None);
    };
    let Some(environment) = environment.as_str() else {
        return Err(format!(
            "{}: packages.venv.environment is not a string",
            facts.display()
        ));
    };
    Ok(Path::new(environment)
        .join("pyvenv.cfg")
        .is_file()
        .then(|| environment.to_owned()))
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
        "Hermes setup failed: `{REPAIR_COMMAND}` did not finish for {}'s Hermes home.\n\
         {output}\n\
         Run it yourself with that home, then launch again:\n\
         \x20 PowerShell:     $env:HERMES_HOME = '{powershell}'; {REPAIR_COMMAND}; Remove-Item Env:HERMES_HOME\n\
         \x20 Command Prompt: set \"HERMES_HOME={home}\" & {REPAIR_COMMAND} & set \"HERMES_HOME=\"",
        crate::DISPLAY_NAME
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

    /// In local mode Hermes speaks only Responses: the render writes only
    /// `rayline-resp`, removes a `rayline` Messages provider an earlier cloud
    /// launch wrote, and carries no per-launch id (Hermes' `prompt_cache_key`
    /// is the identity the core reads).
    #[test]
    fn local_mode_writes_only_the_responses_provider() {
        use super::super::tests::{TEST_KEY, TEST_URL, hosted_models, temp_dir};
        use super::super::{ConversationIdFallback, RenderContext, write_files};
        let dir = temp_dir("hermes-hybrid");
        let capabilities = rayline_local_router::ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 8192,
            input_modalities: vec!["text".into()],
        };
        let cloud = RenderContext {
            key: TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: ConversationIdFallback { forced: None },
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: None,
            models: hosted_models(),
        };
        write_files(&dir, &super::plan(&cloud).unwrap().files).unwrap();
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert!(config.contains("  rayline:"), "{config}");

        let local = RenderContext {
            allow_empty_signature: true,
            capabilities: Some(&capabilities),
            models: super::super::LaunchModels::hybrid(&capabilities),
            ..cloud
        };
        write_files(&dir, &super::plan(&local).unwrap().files).unwrap();
        let config = std::fs::read_to_string(dir.join(CONFIG_FILE)).unwrap();
        assert!(!config.contains("  rayline:"), "{config}");
        assert!(!config.contains("anthropic_messages"), "{config}");
        assert!(config.contains("  rayline-resp:"), "{config}");
        assert!(config.contains("provider: \"custom:rayline-resp\""));
        assert!(!config.contains("x-conversation-id"));
        assert!(!config.contains("session_affinity_header"), "{config}");
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

    /// The seed holds a stale `rayline` provider; the launch replaces it.
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
  rayline:
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
  mine:
    base_url: https://mine.example
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
        use super::super::{Finished, prepare_home, sibling};
        use std::path::Path;
        use std::process::Command;

        const REFUSAL: &str = "hermes: no dependency environment is committed for this \
                               install; run `hermes pm repair`";
        const LAUNCHER: Option<&str> = Some("C:\\hermes\\bin\\hermes.exe\n100\n1.000000000\n");

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

        /// Hermes as it behaves on a clean Windows home: it refuses until the
        /// repair has committed an environment. Records each run's args.
        fn hermes<'a>(
            home: &'a Path,
            calls: &'a mut Vec<String>,
        ) -> impl FnMut(&[&str]) -> std::io::Result<Finished> + 'a {
            move |args| {
                calls.push(args.join(" "));
                if args == ["pm", "repair"] {
                    commit_environment(home);
                    finished(true, "hermes: dependency environment repaired")
                } else if home.join("installs").is_dir() {
                    finished(true, "Hermes Agent v0.23.1")
                } else {
                    finished(false, REFUSAL)
                }
            }
        }

        /// A clean home: Hermes refuses, so the repair runs once; the next
        /// launch of the same Hermes finds the environment and runs nothing.
        #[test]
        fn a_clean_home_is_repaired_once() {
            let home = temp_dir("hermes-clean-home");
            let mut calls = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut calls)).unwrap();
            assert_eq!(calls, ["--version", "pm repair"]);

            let mut again = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut again)).unwrap();
            assert!(again.is_empty(), "{again:?}");
        }

        /// An environment Raydock never saw this launcher start on (another
        /// checkout's, or one repaired by hand) is checked with Hermes once.
        #[test]
        fn another_launcher_is_probed_again() {
            let home = temp_dir("hermes-other-launcher");
            let mut calls = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut calls)).unwrap();

            let other = Some("D:\\other\\hermes.exe\n200\n2.000000000\n");
            let mut probed = Vec::new();
            prepare_home(&home, other, hermes(&home, &mut probed)).unwrap();
            assert_eq!(probed, ["--version"]);
            let mut again = Vec::new();
            prepare_home(&home, other, hermes(&home, &mut again)).unwrap();
            assert!(again.is_empty(), "{again:?}");

            // A launcher that cannot be identified is always asked.
            let mut unknown = Vec::new();
            prepare_home(&home, None, hermes(&home, &mut unknown)).unwrap();
            assert_eq!(unknown, ["--version"]);
        }

        /// With environments for two checkouts in the home, losing one is
        /// noticed even though the other is still there: Hermes is asked.
        #[test]
        fn a_lost_environment_is_noticed_beside_another_one() {
            let home = temp_dir("hermes-two-installs");
            let mut calls = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut calls)).unwrap();
            let other = home.join("installs").join("0123456789abcdef");
            let venv = other.join("environments").join("e2").join("venv");
            std::fs::create_dir_all(&venv).unwrap();
            std::fs::write(venv.join("pyvenv.cfg"), "home = y\n").unwrap();
            let facts = serde_json::json!({
                "packages": { "venv": { "environment": venv.to_string_lossy() } }
            });
            std::fs::write(other.join("facts.json"), facts.to_string()).unwrap();
            // A new environment changes the record: asked once, then skipped.
            let mut probed = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut probed)).unwrap();
            assert_eq!(probed, ["--version"]);
            let mut again = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut again)).unwrap();
            assert!(again.is_empty(), "{again:?}");

            // The launcher's own environment goes; the other one stays.
            let own = home.join("installs").join("d74c6fbcc1df5714");
            std::fs::remove_dir_all(own.join("environments")).unwrap();
            let mut lost = Vec::new();
            prepare_home(&home, LAUNCHER, |args| {
                lost.push(args.join(" "));
                if args == ["pm", "repair"] {
                    commit_environment(&home);
                    finished(true, "repaired")
                } else {
                    finished(false, REFUSAL)
                }
            })
            .unwrap();
            assert_eq!(lost, ["--version", "pm repair"]);
        }

        /// Facts that name a missing venv do not count as an environment.
        #[test]
        fn a_recorded_but_missing_venv_is_repaired() {
            let home = temp_dir("hermes-missing-venv");
            let mut calls = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut calls)).unwrap();
            std::fs::remove_dir_all(home.join("installs")).unwrap();
            let mut again = Vec::new();
            prepare_home(&home, LAUNCHER, hermes(&home, &mut again)).unwrap();
            assert_eq!(again, ["--version", "pm repair"]);
        }

        /// A facts file that cannot be read, or is not JSON, stops the launch
        /// with its path; Hermes is not run.
        #[test]
        fn a_bad_facts_file_is_an_error_naming_it() {
            for (name, bad) in [("unreadable", None), ("invalid", Some("{not json"))] {
                let home = temp_dir(&format!("hermes-facts-{name}"));
                let install = home.join("installs").join("d74c6fbcc1df5714");
                let facts = install.join("facts.json");
                match bad {
                    // A directory where the file belongs cannot be read as one.
                    None => std::fs::create_dir_all(&facts).unwrap(),
                    Some(text) => {
                        std::fs::create_dir_all(&install).unwrap();
                        std::fs::write(&facts, text).unwrap();
                    }
                }
                let mut calls = Vec::new();
                let error = prepare_home(&home, LAUNCHER, hermes(&home, &mut calls)).unwrap_err();
                assert!(
                    error.contains(&facts.display().to_string()),
                    "{name}: {error}"
                );
                assert!(calls.is_empty(), "{name}: {calls:?}");
            }
        }

        /// An installs dir that cannot be read stops the launch with its path.
        #[test]
        fn an_unreadable_installs_dir_is_an_error_naming_it() {
            let home = temp_dir("hermes-installs-file");
            let installs = home.join("installs");
            std::fs::write(&installs, "not a dir").unwrap();
            let mut calls = Vec::new();
            let error = prepare_home(&home, LAUNCHER, hermes(&home, &mut calls)).unwrap_err();
            assert!(error.contains(&installs.display().to_string()), "{error}");
            assert!(calls.is_empty(), "{calls:?}");
        }

        /// A ready marker that cannot be written, or removed, stops the launch
        /// with its path.
        #[test]
        fn a_marker_that_cannot_be_written_or_removed_is_an_error() {
            let home = temp_dir("hermes-marker-write");
            commit_environment(&home);
            // A directory where the marker belongs: writing and removing it as
            // a file both fail.
            let marker = home.join(super::super::READY_MARKER);
            std::fs::create_dir_all(&marker).unwrap();
            for launcher in [LAUNCHER, None] {
                let mut calls = Vec::new();
                let error = prepare_home(&home, launcher, hermes(&home, &mut calls)).unwrap_err();
                assert!(
                    error.contains(&marker.display().to_string()),
                    "{launcher:?}: {error}"
                );
                assert_eq!(calls, ["--version"]);
            }
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
                prepare_home(&home, None, |args| {
                    calls.push(args.join(" "));
                    finished(probe.0, probe.1)
                })
                .unwrap();
                assert_eq!(calls, ["--version"]);
            }
            let mut calls = Vec::new();
            prepare_home(&home, None, |args| {
                calls.push(args.join(" "));
                Err(std::io::Error::other("not found"))
            })
            .unwrap();
            assert_eq!(calls, ["--version"]);
        }

        /// A failed repair stops the launch, records nothing, and names the
        /// exact command, with Raydock's home, for both Windows shells.
        #[test]
        fn a_failed_repair_prints_the_command_with_the_home() {
            let home = temp_dir("hermes-repair-fails").join("it's");
            std::fs::create_dir_all(&home).unwrap();
            let output = (1..=20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n");
            let error = prepare_home(&home, LAUNCHER, |args| {
                if args == ["pm", "repair"] {
                    finished(false, &output)
                } else {
                    finished(false, REFUSAL)
                }
            })
            .unwrap_err();
            assert!(!home.join(super::super::READY_MARKER).exists());
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

            let error = prepare_home(&home, LAUNCHER, |args| {
                if args == ["pm", "repair"] {
                    Err(std::io::Error::other("cannot start"))
                } else {
                    finished(false, REFUSAL)
                }
            })
            .unwrap_err();
            assert!(error.contains("    cannot start"), "{error}");
        }

        /// The probe and repair carry the launch's environment changes (its
        /// HERMES_HOME and the removed keys), not just its program.
        #[test]
        fn probe_runs_with_the_launch_environment() {
            let mut launch = Command::new("hermes");
            launch
                .arg("-z")
                .arg("prompt")
                .env("HERMES_HOME", "home")
                .env_remove("RAYLINE_ROUTER_API_KEY")
                .current_dir("work");
            let probe = sibling(&launch, &["--version"]);
            assert_eq!(probe.get_program(), "hermes");
            assert_eq!(probe.get_args().collect::<Vec<_>>(), ["--version"]);
            assert_eq!(
                probe.get_envs().collect::<Vec<_>>(),
                launch.get_envs().collect::<Vec<_>>()
            );
            assert_eq!(probe.get_current_dir(), Some(Path::new("work")));
        }
    }
}
