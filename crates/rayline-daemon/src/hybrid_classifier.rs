//! Claude Code's auto-mode safety classifier goes to a cloud model, never to
//! the routing core.
//!
//! In auto mode Claude Code asks a classifier whether each tool action may
//! run. That request expects a verdict in an exact format; a local model
//! that misses it gets "no usable answer", Claude Code retries several times
//! and then asks the user, so every action needs manual approval. The core
//! treats the request as a harness side call and holds it on the action that
//! served the last turn, which under `prefer_local` is the local model.
//!
//! The gateway therefore sends a classifier request straight to Rayline cloud:
//! the Decisions router's `/v1/messages`, with the Decisions credential and
//! `routing.classifier_model` (passed as `--classifier-model`) as the model.

use bytes::Bytes;
use serde_json::Value;

/// The opening of the classifier's own system prompt (Claude Code 2.1.319).
/// Classifier requests carry no tools and none of Claude Code's main prompt.
const CLASSIFIER_PROMPT: &str = "You are a security monitor for autonomous AI coding agents.";
/// The block Claude Code puts before every system prompt.
const BILLING_PREAMBLE: &str = "x-anthropic-billing-header:";

/// Where the classifier is sent: the Decisions router's Messages URL and
/// credential, with the classifier's own model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Route {
    pub url: String,
    pub wire_model: String,
    pub auth_env: String,
}

/// The classifier's route: the Decisions router's `/v1/messages` beside the
/// `/v1/routes` that `decisions.url` names (the router the launcher already
/// checked, path prefix kept), with `decisions.auth_env` and `model` as the
/// wire model.
pub(crate) fn route(config: &Value, model: &str) -> Result<Route, String> {
    let decisions = |key: &str| {
        config
            .pointer(&format!("/decisions/{key}"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| format!("the core config has no `decisions.{key}`"))
    };
    let url = decisions("url")?;
    let mut messages = reqwest::Url::parse(url)
        .map_err(|error| format!("the core config's `decisions.url` is not a URL: {error}"))?;
    let prefix = messages
        .path()
        .strip_suffix("/v1/routes")
        .ok_or_else(|| {
            format!("the core config's `decisions.url` {url} is not a router's /v1/routes")
        })?
        .to_owned();
    messages.set_path(&format!("{prefix}/v1/messages"));
    messages.set_query(None);
    Ok(Route {
        url: messages.to_string(),
        wire_model: model.to_owned(),
        auth_env: decisions("auth_env")?.to_owned(),
    })
}

/// Whether a request is Claude Code's auto-mode classifier: a Messages call
/// with no tools whose system prompt opens with the classifier's own block.
pub(crate) fn is_classifier(path: &str, body: &[u8]) -> bool {
    if path != "/v1/messages" {
        return false;
    }
    let Ok(request) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    if request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty())
    {
        return false;
    }
    let opens = |text: &str| text.trim_start().starts_with(CLASSIFIER_PROMPT);
    match request.get("system") {
        Some(Value::String(text)) => opens(text),
        // The first block after Claude Code's billing preamble must be the
        // classifier's own, so an example quoted in a later block is not.
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .find(|text| !text.starts_with(BILLING_PREAMBLE))
            .is_some_and(opens),
        _ => false,
    }
}

/// The request body with its model replaced by the route's wire model. The
/// classifier's quick first stage turns thinking off, which the router refuses
/// for a model whose reasoning is mandatory ("Reasoning is mandatory for this
/// endpoint and cannot be disabled"), so a disabled `thinking` is dropped and
/// the model's default applies.
pub(crate) fn pinned_body(body: &[u8], wire_model: &str) -> Option<Bytes> {
    let mut request: Value = serde_json::from_slice(body).ok()?;
    let fields = request.as_object_mut()?;
    fields.insert("model".to_owned(), Value::String(wire_model.to_owned()));
    if fields
        .get("thinking")
        .and_then(|thinking| thinking.get("type"))
        .and_then(Value::as_str)
        == Some("disabled")
    {
        fields.remove("thinking");
    }
    serde_json::to_vec(&request).ok().map(Bytes::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CLASSIFIER_MODEL: &str = "claude-haiku-5-5";

    #[test]
    fn goes_to_the_decisions_router_with_its_credential_and_the_chosen_model() {
        let config = json!({"decisions": {"url": "https://api.rayline.ai/v1/routes?cell=x",
                                          "auth_env": "RAYLINE_ROUTER_API_KEY"}});
        assert_eq!(
            route(&config, CLASSIFIER_MODEL),
            Ok(Route {
                url: "https://api.rayline.ai/v1/messages".into(),
                wire_model: "claude-haiku-5-5".into(),
                auth_env: "RAYLINE_ROUTER_API_KEY".into(),
            })
        );
    }

    #[test]
    fn keeps_the_router_path_prefix() {
        let config = json!({"decisions": {"url": "https://router.example/api/v1/routes",
                                          "auth_env": "RAYLINE_ROUTER_API_KEY"}});
        assert_eq!(
            route(&config, CLASSIFIER_MODEL).map(|route| route.url),
            Ok("https://router.example/api/v1/messages".to_owned())
        );
        let elsewhere = json!({"decisions": {"url": "https://router.example/decide",
                                             "auth_env": "RAYLINE_ROUTER_API_KEY"}});
        assert_eq!(
            route(&elsewhere, CLASSIFIER_MODEL),
            Err("the core config's `decisions.url` https://router.example/decide is not a router's /v1/routes".to_owned())
        );
    }

    #[test]
    fn a_config_without_the_decisions_url_or_credential_is_refused() {
        assert_eq!(
            route(&json!({"decisions": {"auth_env": "K"}}), CLASSIFIER_MODEL),
            Err("the core config has no `decisions.url`".to_owned())
        );
        assert_eq!(
            route(
                &json!({"decisions": {"url": "https://api.rayline.ai/v1/routes"}}),
                CLASSIFIER_MODEL
            ),
            Err("the core config has no `decisions.auth_env`".to_owned())
        );
    }

    #[test]
    fn recognizes_only_the_classifier() {
        // Shape captured from Claude Code 2.1.319 in auto mode.
        let classifier = json!({"model": "claude-sonnet-5", "max_tokens": 64,
            "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.319"},
                       {"type": "text", "text": "You are a security monitor for autonomous AI coding agents.\n\n## Context"}],
            "messages": [{"role": "user", "content": "<transcript>"}]});
        let bytes = serde_json::to_vec(&classifier).unwrap();
        assert!(is_classifier("/v1/messages", &bytes));
        assert!(!is_classifier("/v1/chat/completions", &bytes));

        let mut with_tools = classifier.clone();
        with_tools["tools"] = json!([{"name": "Bash"}]);
        assert!(!is_classifier(
            "/v1/messages",
            &serde_json::to_vec(&with_tools).unwrap()
        ));

        // The prompt quoted inside a user message is not the classifier.
        let quoted = json!({"system": "You are Claude Code.",
            "messages": [{"role": "user", "content": CLASSIFIER_PROMPT}]});
        assert!(!is_classifier(
            "/v1/messages",
            &serde_json::to_vec(&quoted).unwrap()
        ));
        // Nor is the prompt quoted in a system block after another prompt.
        let later_block = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.319"},
            {"type": "text", "text": "You are Claude Code. Review the example in the next block."},
            {"type": "text", "text": format!("{CLASSIFIER_PROMPT}\nAn example.")}],
            "messages": []});
        assert!(!is_classifier(
            "/v1/messages",
            &serde_json::to_vec(&later_block).unwrap()
        ));
        assert!(!is_classifier("/v1/messages", b"not json"));
    }

    #[test]
    fn pins_the_model_and_keeps_the_rest() {
        let body =
            json!({"model": "claude-sonnet-5", "max_tokens": 64, "stop_sequences": ["</block>"]});
        let pinned: Value = serde_json::from_slice(
            &pinned_body(&serde_json::to_vec(&body).unwrap(), "claude-sonnet-5-5").unwrap(),
        )
        .unwrap();
        assert_eq!(
            pinned,
            json!({"model": "claude-sonnet-5-5", "max_tokens": 64, "stop_sequences": ["</block>"]})
        );
    }

    #[test]
    fn drops_only_disabled_thinking() {
        let pinned = |thinking: Value| -> Value {
            let body = json!({"model": "claude-sonnet-5", "thinking": thinking});
            let bytes = pinned_body(&serde_json::to_vec(&body).unwrap(), "m").unwrap();
            serde_json::from_slice(&bytes).unwrap()
        };
        assert_eq!(pinned(json!({"type": "disabled"})), json!({"model": "m"}));
        assert_eq!(
            pinned(json!({"type": "adaptive"})),
            json!({"model": "m", "thinking": {"type": "adaptive"}})
        );
    }
}
