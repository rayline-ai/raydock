//! pi: `PI_CODING_AGENT_DIR=<dir>`, which holds `models.json`, `settings.json`,
//! `auth.json` and `sessions/`. pi resolves `$VAR` in header values; `apiKey`
//! is the literal key.
//!
//! `compat.sendSessionAffinityHeaders` makes pi send its session id as
//! `x-session-affinity` on Messages (pi 0.87.1 and 1.0.0; off by default for
//! non-OpenRouter endpoints), which the gateway keys the episode on. `-c` and
//! `--resume` reuse the session id, so a resumed session keeps its episode.
//!
//! `compat.allowEmptySignature` makes pi resend a thinking block that came
//! back without a signature as thinking (`signature: ""`) rather than as a
//! visible text block, which is pi's default for Messages. The router serves
//! some turns from models whose thinking carries no Anthropic signature; as
//! text, that reasoning would reach the next model as something the assistant
//! said. As thinking, the router can recognise it and leave it out for a
//! model that cannot verify it. It is on only for envs whose router does that
//! (dev today; `RAYLINE_PI_ALLOW_EMPTY_SIGNATURE=1|0` overrides): a router
//! that forwards an unsigned thinking block to an Anthropic upstream gets the
//! request rejected.

use serde_json::json;

use super::{
    CONTEXT_WINDOW, CONV_ID_ENV, MAX_OUTPUT_TOKENS, Plan, ROUTER_MODEL, RenderContext, json_file,
};

pub(crate) const PROVIDER_ID: &str = "rayline";
/// Read by the hybrid core after Pi's own x-session-affinity.
const HYBRID_SIDECALL_SESSION_HEADER: &str = "x-session-id";
/// Runtime override for [`allow_empty_signature`]: `1` or `0`.
pub(crate) const ALLOW_EMPTY_SIGNATURE_ENV: &str = "RAYLINE_PI_ALLOW_EMPTY_SIGNATURE";

/// Envs whose router drops unsigned reasoning before an Anthropic Messages
/// upstream. Elsewhere an unsigned thinking block would be forwarded as is and
/// rejected, so the flag stays off.
/// TODO: add "prod" once the prod router drops unsigned reasoning.
const UNSIGNED_REASONING_ENVS: &[&str] = &["dev"];

/// Whether `rayline pi` sets `compat.allowEmptySignature` for `env_name`:
/// [`ALLOW_EMPTY_SIGNATURE_ENV`] if set to `1`/`0`, else the env default.
pub(crate) fn allow_empty_signature(env_name: &str, override_value: Option<&str>) -> bool {
    super::parse_switch(override_value).unwrap_or(UNSIGNED_REASONING_ENVS.contains(&env_name))
}

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    plan_with_capabilities(ctx, None)
}

pub(crate) fn plan_with_capabilities(
    ctx: &RenderContext<'_>,
    capabilities: Option<&rayline_local_router::ModelCapabilities>,
) -> Plan {
    let mut compat = json!({ "sendSessionAffinityHeaders": true });
    let mut headers = ctx.conversation_headers(&format!("${CONV_ID_ENV}"));
    if capabilities.is_some() {
        // Hybrid only. Pi's compaction summary call carries the provider's
        // static headers but no x-session-affinity. This lowest-priority
        // session fallback gives it its own execution session, so it never
        // advances (or is refused by) the main session's execution state.
        headers[HYBRID_SIDECALL_SESSION_HEADER] = json!(format!("${CONV_ID_ENV}"));
    }
    if ctx.allow_empty_signature {
        compat["allowEmptySignature"] = json!(true);
    }
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "api": "anthropic-messages",
                "apiKey": ctx.key,
                "headers": headers,
                "compat": compat,
                "models": [{
                    "id": ROUTER_MODEL,
                    "name": format!("Rayline router ({})", ctx.env_name),
                    "reasoning": true,
                    "input": capabilities.map(|caps| json!(caps.input_modalities)).unwrap_or_else(|| json!(["text", "image"])),
                    "contextWindow": capabilities.map_or(CONTEXT_WINDOW, |caps| caps.context_window),
                    "maxTokens": capabilities.map_or(MAX_OUTPUT_TOKENS, |caps| caps.max_output_tokens)
                }]
            }
        }
    });
    // pi writes settings.json itself (model picks, theme), so only seed it.
    let settings = json!({
        "defaultProvider": PROVIDER_ID,
        "defaultModel": ROUTER_MODEL
    });
    Plan {
        files: vec![
            json_file("models.json", &models, true),
            json_file("settings.json", &settings, false),
        ],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{TEST_URL, env_value, render};

    fn render_models(dir: &std::path::Path, env_name: &str, allow: bool) -> serde_json::Value {
        let ctx = super::super::RenderContext {
            env_name,
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: dir,
            conversation_id: super::super::ConversationIdFallback::new(env_name, None),
            allow_empty_signature: allow,
        };
        super::super::write_files(dir, &super::plan(&ctx).files).unwrap();
        serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap()).unwrap()
    }

    #[test]
    fn allow_empty_signature_defaults_by_env_and_override() {
        use super::allow_empty_signature;
        assert!(allow_empty_signature("dev", None));
        assert!(!allow_empty_signature("prod", None));
        assert!(!allow_empty_signature("staging", None));
        assert!(allow_empty_signature("prod", Some("1")));
        assert!(!allow_empty_signature("dev", Some("0")));
        // Unrecognized values keep the env default.
        assert!(!allow_empty_signature("prod", Some("maybe")));
    }

    #[test]
    fn allow_empty_signature_is_rendered_only_when_on_and_rewritten_each_launch() {
        let dir = super::super::tests::temp_dir("pi-sig");
        let on = render_models(&dir, "dev", true);
        assert_eq!(
            on["providers"]["rayline"]["compat"]["allowEmptySignature"],
            true
        );
        // The next launch (prod, or forced off) rewrites models.json without it.
        let off = render_models(&dir, "prod", false);
        let compat = &off["providers"]["rayline"]["compat"];
        assert!(compat.get("allowEmptySignature").is_none(), "{compat}");
        assert_eq!(compat["sendSessionAffinityHeaders"], true);
        let on_again = render_models(&dir, "prod", true);
        assert_eq!(
            on_again["providers"]["rayline"]["compat"]["allowEmptySignature"],
            true
        );
    }

    #[test]
    fn renders_models_and_default_settings() {
        let (dir, plan) = render(Harness::Pi);
        assert_eq!(
            env_value(&plan, "PI_CODING_AGENT_DIR"),
            Some(&dir.clone().into_os_string())
        );
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let provider = &read("models.json")["providers"]["rayline"];
        assert_eq!(provider["baseUrl"], TEST_URL);
        assert_eq!(provider["api"], "anthropic-messages");
        assert_eq!(provider["apiKey"], super::super::tests::TEST_KEY);
        assert_eq!(provider["compat"]["sendSessionAffinityHeaders"], true);
        assert_eq!(provider["compat"]["allowEmptySignature"], true);
        assert_eq!(provider["headers"], serde_json::json!({}));
        let model = &provider["models"][0];
        assert_eq!(model["id"], "rayline-router");
        assert_eq!(model["contextWindow"], 200000);
        assert_eq!(model["maxTokens"], 32000);
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "rayline");
        assert_eq!(settings["defaultModel"], "rayline-router");
    }

    #[test]
    fn hybrid_model_advertises_only_qualified_capabilities() {
        let dir = super::super::tests::temp_dir("pi-hybrid-capabilities");
        let ctx = super::super::RenderContext {
            env_name: "hybrid",
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: &dir,
            conversation_id: super::super::ConversationIdFallback {
                forced: None,
                env_needs_it: false,
            },
            allow_empty_signature: true,
        };
        let capabilities = rayline_local_router::ModelCapabilities {
            context_window: 16384,
            max_output_tokens: 2048,
            input_modalities: vec!["text".into()],
        };
        let plan = Harness::Pi.hybrid_plan(&ctx, &capabilities).unwrap();
        super::super::write_files(&dir, &plan.files).unwrap();
        let models: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.join("models.json")).unwrap()).unwrap();
        let model = &models["providers"]["rayline"]["models"][0];
        assert_eq!(model["contextWindow"], 16384);
        assert_eq!(model["maxTokens"], 2048);
        assert_eq!(model["input"], serde_json::json!(["text"]));
        assert_eq!(
            models["providers"]["rayline"]["headers"]["x-session-id"],
            "$RAYLINE_CONV_ID"
        );
        assert_eq!(
            models["providers"]["rayline"]["compat"]["sendSessionAffinityHeaders"],
            true
        );
    }
}
