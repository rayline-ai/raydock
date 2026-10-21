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

use serde_json::json;

use super::{CONTEXT_WINDOW, CONV_ID_ENV, Plan, ROUTER_MODEL, RenderContext, yaml_file};

pub(crate) const CONFIG_FILE: &str = "config.yaml";
pub(crate) const MESSAGES_PROVIDER: &str = "rayline-anth";
pub(crate) const RESPONSES_PROVIDER: &str = "rayline-resp";

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let models = json!({ ROUTER_MODEL: { "context_length": CONTEXT_WINDOW } });
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
    let config = json!({
        "model": {
            "provider": format!("custom:{MESSAGES_PROVIDER}"),
            "default": ROUTER_MODEL,
            "context_length": CONTEXT_WINDOW
        },
        "providers": {
            MESSAGES_PROVIDER: messages,
            RESPONSES_PROVIDER: responses
        },
        "fallback_providers": []
    });
    Plan {
        files: vec![yaml_file(CONFIG_FILE, &config, true)],
        env_set: vec![("HERMES_HOME", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    }
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
