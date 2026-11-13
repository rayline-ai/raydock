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

use super::{CONV_ID_ENV, Plan, RenderContext, json_file};

pub(crate) const PROVIDER_ID: &str = "rayline";
/// Runtime override for [`allow_empty_signature`]: `1` or `0`.
pub(crate) const ALLOW_EMPTY_SIGNATURE_ENV: &str = "RAYLINE_PI_ALLOW_EMPTY_SIGNATURE";

/// Envs whose router drops unsigned reasoning before an Anthropic Messages
/// upstream. Elsewhere an unsigned thinking block would be forwarded as is and
/// rejected, so the flag stays off.
/// TODO: add "prod" once the prod router drops unsigned reasoning.
const UNSIGNED_REASONING_ENVS: &[&str] = &["dev"];

/// Whether `raydock pi` sets `compat.allowEmptySignature` for `env_name`:
/// [`ALLOW_EMPTY_SIGNATURE_ENV`] if set to `1`/`0`, else the env default.
pub(crate) fn allow_empty_signature(env_name: &str, override_value: Option<&str>) -> bool {
    super::parse_switch(override_value).unwrap_or(UNSIGNED_REASONING_ENVS.contains(&env_name))
}

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let mut compat = json!({ "sendSessionAffinityHeaders": true });
    if ctx.allow_empty_signature {
        compat["allowEmptySignature"] = json!(true);
    }
    let launch = &ctx.models;
    let entries: Vec<_> = launch
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
                "maxTokens": model.max_output_tokens
            })
        })
        .collect();
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "api": "anthropic-messages",
                "apiKey": ctx.key,
                "headers": ctx.conversation_headers(&format!("${CONV_ID_ENV}")),
                "compat": compat,
                "models": entries
            }
        }
    });
    // pi writes settings.json itself (model picks, theme), so only seed it.
    let settings = json!({
        "defaultProvider": PROVIDER_ID,
        "defaultModel": launch.main
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

    fn render_models(dir: &std::path::Path, allow: bool) -> serde_json::Value {
        let ctx = super::super::RenderContext {
            key: super::super::tests::TEST_KEY,
            router_url: TEST_URL,
            config_dir: dir,
            conversation_id: super::super::ConversationIdFallback::new(None),
            allow_empty_signature: allow,
            capabilities: None,
            harness_major_version: None,
            models: super::super::tests::hosted_models(),
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
        let on = render_models(&dir, true);
        assert_eq!(
            on["providers"]["rayline"]["compat"]["allowEmptySignature"],
            true
        );
        // The next launch (prod, or forced off) rewrites models.json without it.
        let off = render_models(&dir, false);
        let compat = &off["providers"]["rayline"]["compat"];
        assert!(compat.get("allowEmptySignature").is_none(), "{compat}");
        assert_eq!(compat["sendSessionAffinityHeaders"], true);
        let on_again = render_models(&dir, true);
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
        let ids: Vec<_> = provider["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["arc-sage-1.0", "arc-swift-1.0"]);
        let model = &provider["models"][0];
        assert_eq!(model["name"], "ARC Sage");
        assert_eq!(model["reasoning"], false);
        assert_eq!(model["input"], serde_json::json!(["text", "image"]));
        assert_eq!(model["contextWindow"], 1000000);
        assert_eq!(model["maxTokens"], 128000);
        assert_eq!(provider["models"][1]["name"], "ARC Swift");
        assert_eq!(provider["models"][1]["maxTokens"], 128000);
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "rayline");
        assert_eq!(settings["defaultModel"], "arc-sage-1.0");
    }

    #[test]
    fn a_contributor_key_seeds_its_listed_sage() {
        let dir = super::super::tests::temp_dir("pi-contributor");
        let mut ctx = super::super::tests::render_context(&dir);
        ctx.models = super::super::LaunchModels::from_listing(&super::super::tests::listing(Some(
            "contributor",
        )))
        .unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        let read = |name: &str| -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(dir.join(name)).unwrap()).unwrap()
        };
        let settings = read("settings.json");
        assert_eq!(settings["defaultProvider"], "rayline");
        assert_eq!(settings["defaultModel"], "arc-sage-contributor-1.0");
        let ids: Vec<_> = read("models.json")["providers"]["rayline"]["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["id"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(ids[0], "arc-sage-contributor-1.0");
    }

    #[test]
    fn saved_settings_are_left_as_pi_wrote_them() {
        let dir = super::super::tests::temp_dir("pi-saved");
        let ctx = super::super::tests::render_context(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        // A pick the key does not list stays: Rayline only seeds this file.
        let saved = "{\n  \"defaultProvider\": \"rayline\",\n  \"defaultModel\": \"rayline-router\",\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&settings, saved).unwrap();
        std::fs::write(dir.join("models.json"), "{}").unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), saved);
        // models.json holds the key and URL, so every launch rewrites it.
        let models: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("models.json")).unwrap())
                .unwrap();
        assert_eq!(models["providers"]["rayline"]["baseUrl"], TEST_URL);
    }
}
