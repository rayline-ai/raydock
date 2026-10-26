//! Hermes Agent: `HERMES_HOME=<dir>` holds `config.yaml` (plus Hermes' own
//! state: sessions, memories, skills). `api_key` is the literal key and
//! `${VAR}` is expanded in config values.
//!
//! Episode keying: Hermes sends its session id as `x-session-affinity` on
//! Responses, which the router keys the episode on, and `-c` / `--resume`
//! keeps it. On `anthropic_messages` Hermes sends no session id, so the
//! default Messages provider always carries the per-launch `x-conversation-id`
//! (unless `RAYLINE_HARNESS_CONV_ID_FALLBACK=0`): one launch is one episode,
//! and a resumed session in a new launch starts a new one. Hermes applies a
//! provider's `extra_headers` on Messages from v2026.9.21; earlier releases
//! ignore them there, so each Messages turn is its own episode. The
//! `rayline-resp` provider (`--provider rayline-resp`) speaks Responses. Each
//! entry has its own base_url because Hermes looks up `extra_headers` by
//! base_url.
//!
//! `config.yaml` also holds the user's Hermes settings (`hermes config set`,
//! `hermes tools`), so each launch merges only Rayline's keys ([`OWNED`]) into
//! the existing file and keeps every other line as written.

use std::{fs, io};

use serde_json::json;

use super::{CONV_ID_ENV, ConfigFile, Plan, ROUTER_MODEL, RenderContext, to_yaml, yaml_merge};

pub(crate) const CONFIG_FILE: &str = "config.yaml";
pub(crate) const MESSAGES_PROVIDER: &str = "rayline-anth";
pub(crate) const RESPONSES_PROVIDER: &str = "rayline-resp";

/// The keys Rayline sets in `config.yaml`. `fallback_providers` stays empty
/// and the legacy `fallback_model` is removed, so a failed turn never moves
/// off the router.
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
    let models = json!({ ROUTER_MODEL: { "context_length": ctx.context_window() } });
    let mut responses = json!({
        "base_url": ctx.v1_base(),
        "api_key": ctx.key,
        "api_mode": "codex_responses",
        "models": models
    });
    if ctx.conversation_id.enabled(true) {
        responses["extra_headers"] = json!({ super::CONV_HEADER: format!("${{{CONV_ID_ENV}}}") });
    }
    let mut messages = json!({
        "base_url": ctx.messages_base(),
        "api_key": ctx.key,
        "api_mode": "anthropic_messages",
        "models": models
    });
    // No native session id on Messages, so the per-launch id is on there
    // unless forced off.
    if ctx.conversation_id.enabled(false) {
        messages["extra_headers"] = json!({ super::CONV_HEADER: format!("${{{CONV_ID_ENV}}}") });
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
            "default": ROUTER_MODEL,
            "context_length": ctx.context_window()
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
        }],
        env_set: vec![("HERMES_HOME", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{env_value, render};
    use super::CONFIG_FILE;

    #[test]
    fn messages_provider_carries_the_conversation_id() {
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
        // Messages has no native session id, so it carries the id on dev too.
        assert_eq!(
            headers(ConversationIdFallback::new("dev", None)),
            (true, false)
        );
        assert_eq!(
            headers(ConversationIdFallback::new("prod", None)),
            (true, true)
        );
        assert_eq!(
            headers(ConversationIdFallback::new("prod", Some("0"))),
            (false, false)
        );
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
            env_name: "dev",
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: ConversationIdFallback::new("dev", None),
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: None,
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
                env_name: "dev",
                key: super::super::tests::TEST_KEY,
                router_url: TEST_URL,
                config_dir: &dir,
                conversation_id: ConversationIdFallback::new("dev", None),
                allow_empty_signature: false,
                capabilities: None,
                harness_major_version: None,
            };
            write_files(&dir, &super::plan(&ctx).unwrap().files).unwrap();
            std::fs::read_to_string(&path).unwrap()
        };
        let first = launch();
        assert_eq!(
            first,
            "model:
  default: \"rayline-router\"
  provider: \"custom:rayline-anth\"
  reasoning_effort: high
  context_length: 200000
providers:
  rayline-anth:
    api_key: \"rlk-test\"
    api_mode: \"anthropic_messages\"
    base_url: \"https://api-dev.rayline.ai\"
    extra_headers:
      x-conversation-id: \"${RAYLINE_CONV_ID}\"
    models:
      rayline-router:
        context_length: 200000
  mine:
    base_url: https://mine.example
  rayline-resp:
    api_key: \"rlk-test\"
    api_mode: \"codex_responses\"
    base_url: \"https://api-dev.rayline.ai/v1\"
    models:
      rayline-router:
        context_length: 200000
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
            env_name: "dev",
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: ConversationIdFallback::new("dev", None),
            allow_empty_signature: false,
            capabilities: None,
            harness_major_version: None,
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
  context_length: 200000
  default: \"rayline-router\"
  provider: \"custom:rayline-anth\"
providers:
  rayline-anth:
    api_key: \"rlk-test\"
    api_mode: \"anthropic_messages\"
    base_url: \"https://api-dev.rayline.ai\"
    extra_headers:
      x-conversation-id: \"${RAYLINE_CONV_ID}\"
    models:
      rayline-router:
        context_length: 200000
  rayline-resp:
    api_key: \"rlk-test\"
    api_mode: \"codex_responses\"
    base_url: \"https://api-dev.rayline.ai/v1\"
    models:
      rayline-router:
        context_length: 200000
"
        );
    }
}
