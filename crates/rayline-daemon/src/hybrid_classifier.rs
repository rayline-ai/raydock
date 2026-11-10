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
//! The gateway therefore sends a classifier request straight to a cloud model
//! (Claude Haiku 5.5 unless the user chooses another) through the router of
//! the profile's cloud Messages pins, on the same router as Decisions, with
//! that deployment's credential, exactly as the core would dispatch a
//! model-pin turn. A profile with no such deployment (local only) leaves the
//! request to the core.

use bytes::Bytes;
use serde_json::Value;

/// The opening of the classifier's own system prompt (Claude Code 2.1.319).
/// Classifier requests carry no tools and none of Claude Code's main prompt.
const CLASSIFIER_PROMPT: &str = "You are a security monitor for autonomous AI coding agents.";
/// The block Claude Code puts before every system prompt.
const BILLING_PREAMBLE: &str = "x-anthropic-billing-header:";

/// The classifier's model when the user names none.
pub(crate) const DEFAULT_CLASSIFIER_MODEL: &str = "claude-haiku-5-5";

/// Where the classifier is sent: a cloud Messages pin's router and
/// credential, with the classifier's own model.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CloudPin {
    pub url: String,
    pub wire_model: String,
    pub auth_env: String,
}

/// The classifier's route: the router URL and credential of a `model_pin`
/// Messages deployment on the same origin as `decisions.url` (the router the
/// launcher already checked), with `model` as the wire model. The router
/// serves any of its models at that URL, so `model` need not be a profile
/// target.
pub(crate) fn cloud_pin(config: &Value, model: &str) -> Option<CloudPin> {
    let decisions = origin(config.pointer("/decisions/url")?.as_str()?)?;
    config
        .get("deployments")?
        .as_array()?
        .iter()
        .filter(|deployment| {
            deployment.get("kind").and_then(Value::as_str) == Some("cloud")
                && deployment.get("cloud_transport").and_then(Value::as_str) == Some("model_pin")
                && deployment.get("protocol").and_then(Value::as_str)
                    == Some("anthropic.messages.v1")
        })
        .filter_map(|deployment| {
            let field = |name: &str| {
                deployment
                    .get(name)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            };
            let pin = CloudPin {
                url: field("url")?,
                wire_model: model.to_owned(),
                auth_env: field("auth_env")?,
            };
            (origin(&pin.url)? == decisions).then_some(pin)
        })
        .next()
}

/// Scheme, host and port of a URL; None for anything unparseable.
fn origin(url: &str) -> Option<(String, String, u16)> {
    let parsed = reqwest::Url::parse(url).ok()?;
    Some((
        parsed.scheme().to_owned(),
        parsed.host_str()?.to_ascii_lowercase(),
        parsed.port_or_known_default()?,
    ))
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

/// The request body with its model replaced by the pin's wire model. The
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

    fn config(deployments: Value) -> Value {
        json!({"decisions": {"url": "https://api.rayline.ai/v1/routes"}, "deployments": deployments})
    }

    fn pin(id: &str, url: &str, wire: &str) -> Value {
        json!({"id": id, "kind": "cloud", "cloud_transport": "model_pin", "url": url,
               "protocol": "anthropic.messages.v1", "wire_model": wire,
               "auth_env": "RAYLINE_ROUTER_API_KEY"})
    }

    #[test]
    fn takes_the_decisions_router_and_its_credential_with_the_chosen_model() {
        let config = config(json!([
            {"id": "local", "kind": "local", "url": "http://127.0.0.1:8000/v1/chat/completions",
             "protocol": "openai.chat.v1", "wire_model": "glm"},
            pin("cloud-opus", "https://api.rayline.ai/v1/messages", "claude-opus-5-5"),
            pin("cloud-sonnet", "https://api.rayline.ai/v1/messages", "claude-sonnet-5-5"),
        ]));
        assert_eq!(
            cloud_pin(&config, DEFAULT_CLASSIFIER_MODEL),
            Some(CloudPin {
                url: "https://api.rayline.ai/v1/messages".into(),
                wire_model: "claude-haiku-5-5".into(),
                auth_env: "RAYLINE_ROUTER_API_KEY".into(),
            })
        );
        assert_eq!(
            cloud_pin(&config, "claude-sonnet-5-5").map(|pin| pin.wire_model),
            Some("claude-sonnet-5-5".into())
        );
    }

    #[test]
    fn never_sends_the_key_to_another_host_and_local_only_has_no_pin() {
        let elsewhere = config(json!([pin(
            "cloud-sonnet",
            "https://elsewhere.example/v1/messages",
            "claude-sonnet-5-5"
        )]));
        assert_eq!(cloud_pin(&elsewhere, DEFAULT_CLASSIFIER_MODEL), None);
        let local_only = config(json!([{"id": "local", "kind": "local",
            "url": "http://127.0.0.1:8000/v1/chat/completions", "protocol": "openai.chat.v1"}]));
        assert_eq!(cloud_pin(&local_only, DEFAULT_CLASSIFIER_MODEL), None);
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
