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

use super::{CONV_ID_ENV, Plan, ROUTER_MODEL, RenderContext, json_file, model_display_name};

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
    let launch = ctx.models();
    let entries: Vec<_> = launch
        .offered
        .iter()
        .map(|model| {
            json!({
                "id": model,
                "name": model_display_name(launch, model),
                // ARC selects thinking effort; clients must not expose controls.
                "reasoning": ctx.capabilities.is_some(),
                "input": ctx.input_modalities(),
                "contextWindow": ctx.context_window(),
                "maxTokens": ctx.max_output_tokens(model)
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
    let settings = |model: &str| {
        json!({
            "defaultProvider": PROVIDER_ID,
            "defaultModel": model
        })
    };
    let mut settings_file = json_file("settings.json", &settings(launch.main), false);
    // Hosted catalogs no longer list Legacy. Move an existing Rayline Legacy
    // selection to Sage, retaining every other Pi setting a user saved.
    if launch.main != ROUTER_MODEL {
        let path = ctx.config_dir.join("settings.json");
        if let Ok(existing) = std::fs::read_to_string(&path)
            && let Ok(mut saved) = serde_json::from_str::<serde_json::Value>(&existing)
            && saved["defaultProvider"] == PROVIDER_ID
            && saved["defaultModel"] == ROUTER_MODEL
        {
            saved["defaultModel"] = json!(launch.main);
            settings_file = json_file("settings.json", &saved, true);
            settings_file.merged_from = Some(existing);
        } else {
            // The seed 0.2.x wrote (Legacy as the default); still exactly that
            // means the user never picked a model, so it moves to the new default.
            settings_file.replaces_seed =
                Some(json_file("settings.json", &settings(ROUTER_MODEL), false).contents);
        }
    }
    Plan {
        files: vec![json_file("models.json", &models, true), settings_file],
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
    fn saved_legacy_pick_moves_to_arc_without_losing_other_settings() {
        let dir = super::super::tests::temp_dir("pi-seed");
        let ctx = super::super::tests::render_context(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let settings = dir.join("settings.json");
        // The exact seed 0.2.x wrote: no model was ever picked.
        let written = super::super::json_file(
            "settings.json",
            &serde_json::json!({"defaultProvider": "rayline", "defaultModel": "rayline-router"}),
            false,
        )
        .contents;
        std::fs::write(&settings, &written).unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        let upgraded: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(upgraded["defaultModel"], "arc-sage-1.0");
        // A saved Legacy pick must move to Sage even when pi added settings.
        let picked = "{\n  \"defaultProvider\": \"rayline\",\n  \"defaultModel\": \"rayline-router\",\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&settings, picked).unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        let migrated: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert_eq!(migrated["defaultModel"], "arc-sage-1.0");
        assert_eq!(migrated["theme"], "dark");
        // A non-Rayline pick remains the user's explicit choice.
        let other = "{\n  \"defaultProvider\": \"other\",\n  \"defaultModel\": \"rayline-router\",\n  \"theme\": \"dark\"\n}\n";
        std::fs::write(&settings, other).unwrap();
        super::super::write_files(&dir, &super::plan(&ctx).files).unwrap();
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), other);
    }
}
