//! Token counts from Anthropic `usage` objects, and their OpenAI shapes.

use serde_json::{Value, json};

/// Token counts from Anthropic `usage` objects, merged as they arrive.
///
/// Which event carries the real counts depends on the upstream. Anthropic
/// puts the prompt counts in `message_start` and repeats them, cumulative, in
/// the final `message_delta`. The Rayline router (and other gateways that
/// translate from another provider) sends a `message_start` placeholder
/// (`input_tokens: 0`, cache fields `null`) and only the final `message_delta`
/// carries the real counts. Every usage object seen is merged, a nonzero
/// number overwriting the previous value, so the last event that reports a
/// count wins and a placeholder (`null` or `0`) never erases a real one. The
/// counts are cumulative, so a real count never drops to zero later.
///
/// `output_tokens` also remembers whether any event reported it at all, so a
/// caller can tell an explicit zero from an upstream that never said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct AnthropicUsage {
    pub(crate) input_tokens: u64,
    pub(crate) cache_creation_input_tokens: u64,
    pub(crate) cache_read_input_tokens: u64,
    pub(crate) output_tokens: Option<u64>,
}

impl AnthropicUsage {
    /// Overlay the nonzero numeric fields of an Anthropic `usage` object.
    pub(crate) fn merge(&mut self, usage: Option<&Value>) {
        let Some(usage) = usage else {
            return;
        };
        let slots = [
            ("input_tokens", &mut self.input_tokens),
            (
                "cache_creation_input_tokens",
                &mut self.cache_creation_input_tokens,
            ),
            ("cache_read_input_tokens", &mut self.cache_read_input_tokens),
        ];
        for (key, slot) in slots {
            if let Some(count) = usage.get(key).and_then(Value::as_u64)
                && count > 0
            {
                *slot = count;
            }
        }
        if let Some(count) = usage.get("output_tokens").and_then(Value::as_u64)
            && (count > 0 || self.output_tokens.is_none())
        {
            self.output_tokens = Some(count);
        }
    }

    /// The reported output count, or `estimate` when no event carried one.
    pub(crate) fn output_tokens_or(self, estimate: impl FnOnce() -> u64) -> u64 {
        self.output_tokens.unwrap_or_else(estimate)
    }

    /// The whole prompt: Anthropic's `input_tokens` excludes the cached and
    /// cache-written tokens, OpenAI's prompt counts include them.
    pub(crate) fn prompt_tokens(self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_creation_input_tokens)
            .saturating_add(self.cache_read_input_tokens)
    }

    /// Whether any upstream event reported a prompt count.
    pub(crate) fn has_prompt_tokens(self) -> bool {
        self.prompt_tokens() > 0
    }

    /// The counts in OpenAI Chat Completions' shape, with the cached part
    /// repeated under `prompt_tokens_details.cached_tokens`, which is where
    /// Chat clients read their cache-read count.
    pub(crate) fn to_chat(self) -> Value {
        let prompt_tokens = self.prompt_tokens();
        let output_tokens = self.output_tokens.unwrap_or(0);
        json!({
            "prompt_tokens": prompt_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": prompt_tokens.saturating_add(output_tokens),
            "prompt_tokens_details": {"cached_tokens": self.cache_read_input_tokens},
        })
    }

    /// The inverse of [`Self::to_chat`], for replaying a `chat.completion`.
    pub(crate) fn from_chat(usage: Option<&Value>) -> Self {
        let field = |pointer: &str| {
            usage
                .and_then(|usage| usage.pointer(pointer))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        let prompt_tokens = field("/prompt_tokens");
        let cache_read_input_tokens = field("/prompt_tokens_details/cached_tokens");
        Self {
            input_tokens: prompt_tokens.saturating_sub(cache_read_input_tokens),
            cache_creation_input_tokens: 0,
            cache_read_input_tokens,
            output_tokens: Some(field("/completion_tokens")),
        }
    }

    /// The counts in OpenAI Responses' shape: `input_tokens` is the whole
    /// prompt and the cached part is repeated under
    /// `input_tokens_details.cached_tokens`.
    pub(crate) fn to_responses(self) -> Value {
        let input_tokens = self.prompt_tokens();
        let output_tokens = self.output_tokens.unwrap_or(0);
        json!({
            "input_tokens": input_tokens,
            "input_tokens_details": {"cached_tokens": self.cache_read_input_tokens},
            "output_tokens": output_tokens,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": input_tokens.saturating_add(output_tokens),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> AnthropicUsage {
        let mut usage = AnthropicUsage::default();
        usage.merge(Some(
            &json!({"input_tokens": 10, "cache_creation_input_tokens": 20,
                                  "cache_read_input_tokens": 30, "output_tokens": 4}),
        ));
        usage
    }

    #[test]
    fn placeholders_never_erase_real_counts() {
        let mut usage = full();
        usage.merge(Some(
            &json!({"input_tokens": 0, "cache_creation_input_tokens": null,
                                 "cache_read_input_tokens": 0, "output_tokens": 5}),
        ));
        usage.merge(None);
        assert_eq!(
            usage,
            AnthropicUsage {
                input_tokens: 10,
                cache_creation_input_tokens: 20,
                cache_read_input_tokens: 30,
                output_tokens: Some(5)
            }
        );
        assert_eq!(usage.prompt_tokens(), 60);
        assert!(usage.has_prompt_tokens());
        assert!(!AnthropicUsage::default().has_prompt_tokens());
    }

    /// An explicit zero output count is a report, not a gap: it is kept over
    /// an estimate, while an upstream that never reported one gets the
    /// estimate. A later real count still replaces a placeholder zero.
    #[test]
    fn explicit_zero_output_is_reported_not_estimated() {
        let mut usage = AnthropicUsage::default();
        assert_eq!(usage.output_tokens_or(|| 7), 7);
        usage.merge(Some(&json!({"output_tokens": 0})));
        assert_eq!(usage.output_tokens_or(|| 7), 0);
        usage.merge(Some(&json!({"output_tokens": 16})));
        assert_eq!(usage.output_tokens_or(|| 7), 16);
        usage.merge(Some(&json!({"output_tokens": 0})));
        assert_eq!(usage.output_tokens_or(|| 7), 16);
    }

    #[test]
    fn openai_shapes_count_the_whole_prompt_and_repeat_the_cached_part() {
        assert_eq!(
            full().to_chat(),
            json!({"prompt_tokens": 60, "completion_tokens": 4, "total_tokens": 64,
                   "prompt_tokens_details": {"cached_tokens": 30}})
        );
        assert_eq!(
            full().to_responses(),
            json!({"input_tokens": 60, "input_tokens_details": {"cached_tokens": 30},
                   "output_tokens": 4, "output_tokens_details": {"reasoning_tokens": 0},
                   "total_tokens": 64})
        );
        let chat = full().to_chat();
        let round_trip = AnthropicUsage::from_chat(Some(&chat));
        assert_eq!(round_trip.to_chat(), full().to_chat());
    }
}
