//! omp (oh-my-pi): `PI_CODING_AGENT_DIR=<dir>` holds `models.yml` and
//! `config.yml`. A named profile (`OMP_PROFILE`/`PI_PROFILE`) makes omp ignore
//! that override, so both are removed. omp also reads Claude Code's config from
//! `CLAUDE_CONFIG_DIR` (MCP servers, skills); in testing, a
//! `CLAUDE_CONFIG_DIR` inherited from `raydock claude` pulling another config
//! into omp, so it is removed too. Header values name env vars; `apiKey` is the
//! literal key.
//!
//! omp sends its session id as `X-Claude-Code-Session-Id`, which the gateway
//! keys the episode on. In `auth: apiKey` mode it does so from 18.1.10; an
//! older omp gets a warning, since each of its turns is then a new episode.

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::json;

use super::{CONV_ID_ENV, Plan, ROUTER_MODEL, RenderContext, yaml_file};

pub(crate) const PROVIDER_ID: &str = "rayline";
/// First omp that sends `X-Claude-Code-Session-Id` in `auth: apiKey` mode.
pub(crate) const MIN_SESSION_HEADER_VERSION: (u64, u64, u64) = (18, 1, 10);

/// Warn when `omp --version` reports a version that sends no session id in
/// apiKey mode. Silent when the version cannot be read.
pub(crate) fn warn_if_old(binary: &Path) {
    let mut command = Command::new(binary);
    crate::codex::withhold_router_key(&mut command);
    let Ok(output) = command
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    else {
        return;
    };
    if let Some(warning) = version_warning(&String::from_utf8_lossy(&output.stdout)) {
        eprintln!("{warning}");
    }
}

/// The warning for `omp --version` output (`omp/18.5.0`), if it is too old.
pub(crate) fn version_warning(version_output: &str) -> Option<String> {
    let version = parse_version(version_output)?;
    (version < MIN_SESSION_HEADER_VERSION).then(|| {
        let (major, minor, patch) = MIN_SESSION_HEADER_VERSION;
        format!(
            "warning: omp {}.{}.{} sends no session id with an API key, so every turn is a \
             separate Rayline episode. Upgrade to omp {major}.{minor}.{patch} or later.",
            version.0, version.1, version.2
        )
    })
}

/// The first `X.Y.Z` in `text`.
fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    text.split(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .find_map(|token| {
            let mut parts = token.split('.').map(|part| part.parse::<u64>().ok());
            match (parts.next(), parts.next(), parts.next()) {
                (Some(Some(major)), Some(Some(minor)), Some(Some(patch))) => {
                    Some((major, minor, patch))
                }
                _ => None,
            }
        })
}

pub(crate) fn plan(ctx: &RenderContext<'_>) -> Plan {
    let models = json!({
        "providers": {
            PROVIDER_ID: {
                "baseUrl": ctx.messages_base(),
                "apiKey": ctx.key,
                "api": "anthropic-messages",
                // Without `auth: apiKey` omp poses as Claude Code (renamed tools,
                // billing system block). apiKey mode is the honest request shape.
                "auth": "apiKey",
                "headers": ctx.conversation_headers(CONV_ID_ENV),
                "models": [{
                    "id": ROUTER_MODEL,
                    "name": format!("Raydock router ({})", ctx.env_name),
                    "reasoning": true,
                    "input": ctx.input_modalities(),
                    "contextWindow": ctx.context_window(),
                    "maxTokens": ctx.max_output_tokens()
                }]
            }
        }
    });
    let model_ref = format!("{PROVIDER_ID}/{ROUTER_MODEL}");
    // omp writes its settings to config.yml, so only seed it.
    let config = json!({
        "modelRoles": {
            "default": model_ref,
            "smol": model_ref,
            "slow": model_ref,
            "plan": model_ref
        }
    });
    Plan {
        files: vec![
            yaml_file("models.yml", &models, true),
            yaml_file("config.yml", &config, false),
        ],
        env_set: vec![("PI_CODING_AGENT_DIR", ctx.config_dir.as_os_str().to_owned())],
        env_remove: vec!["CLAUDE_CONFIG_DIR", "OMP_PROFILE", "PI_PROFILE"],
    }
}

#[cfg(test)]
mod tests {
    use super::super::Harness;
    use super::super::tests::{env_value, render};

    #[test]
    fn renders_api_key_mode_and_clears_claude_config_dir() {
        let (dir, plan) = render(Harness::Omp);
        assert_eq!(
            env_value(&plan, "PI_CODING_AGENT_DIR"),
            Some(&dir.clone().into_os_string())
        );
        assert!(plan.env_remove.contains(&"CLAUDE_CONFIG_DIR"));
        let models = std::fs::read_to_string(dir.join("models.yml")).unwrap();
        assert_eq!(
            models,
            "providers:
  rayline:
    api: \"anthropic-messages\"
    apiKey: \"rlk-test\"
    auth: \"apiKey\"
    baseUrl: \"https://api-dev.rayline.ai\"
    headers: {}
    models:
      - contextWindow: 200000
        id: \"rayline-router\"
        input: [\"text\", \"image\"]
        maxTokens: 32000
        name: \"Raydock router (dev)\"
        reasoning: true
"
        );
        let config = std::fs::read_to_string(dir.join("config.yml")).unwrap();
        assert!(config.starts_with("modelRoles:\n"));
        for role in ["default", "smol", "slow", "plan"] {
            assert!(config.contains(&format!("  {role}: \"rayline/rayline-router\"\n")));
        }
    }

    #[test]
    fn warns_below_18_1_10_only() {
        use super::version_warning;
        let old = version_warning("omp/18.0.5\n").expect("18.0.5 warns");
        assert!(
            old.contains("omp 18.0.5") && old.contains("18.1.10"),
            "{old}"
        );
        assert!(version_warning("omp 18.1.9").is_some());
        assert_eq!(version_warning("omp/18.1.10"), None);
        assert_eq!(version_warning("omp/18.5.0"), None);
        assert_eq!(version_warning("omp/19.0.0"), None);
        assert_eq!(version_warning("no version here"), None);
    }
}
