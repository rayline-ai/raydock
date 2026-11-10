//! Bounded, passive response observation for Anthropic Messages, OpenAI Chat,
//! and OpenAI Responses. Feed decoded HTTP body bytes (not compressed bytes).
//! The caller owns request start/completion and transport errors. `finish`
//! reports definite in-band failure or a stream without a terminal marker.
//! Invalid/oversized data only makes telemetry unavailable, not inference failed.
//! Buffered JSON never emits FirstToken: body completion is not token arrival.
//! No error text, response models, or lifecycle updates are recorded.

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;

use serde_json::Value;

use crate::{MetricsUpdate, SharedMetricsSink};

/// Maximum JSON body or SSE frame scratch size. Oversized SSE frames are
/// discarded through their blank-line delimiter; subsequent frames still work.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Response classification independent of transport completion and telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseValidity {
    /// A recognized protocol response, or successful SSE terminal marker.
    Valid,
    /// Malformed bounded JSON, definite failure, or finalized unterminated SSE.
    Invalid,
    /// Not yet finalized, oversized JSON, or an unrecognized JSON shape.
    Unavailable,
}

pub struct ResponseObserver {
    sink: SharedMetricsSink,
    request_id: String,
    sse: bool,
    scratch: Vec<u8>,
    skipping: bool,
    line_nonempty: bool,
    after_cr: bool,
    terminal: bool,
    failed: bool,
    first_token: bool,
    finished: bool,
    json_validity: ResponseValidity,
    anthropic_input: Option<u64>,
    anthropic_read: Option<u64>,
    anthropic_creation: Option<u64>,
}

impl ResponseObserver {
    pub fn new(sink: SharedMetricsSink, request_id: String, sse: bool) -> Self {
        Self {
            sink,
            request_id,
            sse,
            scratch: Vec::new(),
            skipping: false,
            line_nonempty: false,
            after_cr: false,
            terminal: false,
            failed: false,
            first_token: false,
            finished: false,
            json_validity: ResponseValidity::Unavailable,
            anthropic_input: None,
            anthropic_read: None,
            anthropic_creation: None,
        }
    }

    /// Observe arbitrarily split chunks without retaining parsed response content.
    /// Calls after `finish` are ignored.
    pub fn observe(&mut self, bytes: &[u8]) {
        if self.finished {
            return;
        }
        if !self.sse {
            if !self.skipping {
                if bytes.len() > MAX_RESPONSE_BYTES.saturating_sub(self.scratch.len()) {
                    self.scratch.clear();
                    self.skipping = true;
                } else {
                    self.scratch.extend_from_slice(bytes);
                }
            }
            return;
        }
        for &byte in bytes {
            // SSE accepts CR, LF, and CRLF, including splits between CR and LF.
            if byte == b'\n' && self.after_cr {
                self.after_cr = false;
                continue;
            }
            self.after_cr = byte == b'\r';
            if byte == b'\r' || byte == b'\n' {
                if self.line_nonempty {
                    self.push(b'\n');
                    self.line_nonempty = false;
                } else {
                    if !self.skipping {
                        let frame = std::mem::take(&mut self.scratch);
                        self.frame(&frame);
                        self.scratch = frame;
                    }
                    self.scratch.clear();
                    self.skipping = false;
                }
            } else {
                self.line_nonempty = true;
                self.push(byte);
            }
        }
    }

    fn push(&mut self, byte: u8) {
        if self.skipping {
            return;
        }
        if self.scratch.len() == MAX_RESPONSE_BYTES {
            self.scratch.clear();
            self.skipping = true;
        } else {
            self.scratch.push(byte);
        }
    }

    /// Whether a terminal success marker or definite in-band failure has been
    /// observed. Callers can finalize SSE without waiting for another body poll.
    /// This reports both successful and failed terminals; use `has_failed` to
    /// distinguish them. Buffered JSON success has no streaming terminal marker.
    pub fn is_terminal(&self) -> bool {
        self.terminal
    }

    /// Whether a definite in-band failure has been observed (or `finish` has
    /// classified SSE without a terminal marker as incomplete).
    /// Malformed/oversized data is not a known inference failure.
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Classify the observed response. Call after `finish` for buffered JSON;
    /// before finalization its validity is unavailable. SSE is valid after a
    /// successful terminal and invalid after failure or finalized missing terminal.
    /// Callers should treat `Invalid` as an error, including malformed HTTP 200
    /// JSON, while `Unavailable` may be accepted when transport completed.
    /// This does not establish transport completion itself.
    pub fn validity(&self) -> ResponseValidity {
        if self.failed {
            ResponseValidity::Invalid
        } else if self.sse {
            if self.terminal {
                ResponseValidity::Valid
            } else {
                ResponseValidity::Unavailable
            }
        } else {
            self.json_validity
        }
    }

    /// Finalize once, returning `true` only for a definite in-band error or
    /// SSE without a terminal `message_stop`, `response.completed`, or `[DONE]`.
    /// Invalid/oversized JSON or frames mean missing telemetry, not failure.
    /// Once a stream terminal is seen, an unfinished/skipped trailing frame
    /// does not turn success into failure. Repeated calls return the same result.
    /// A `false` result means no known failure, not validated inference success:
    /// callers must still establish transport completion (for example, received
    /// Content-Length bytes for JSON). No lifecycle event or buffered JSON
    /// FirstToken is emitted.
    pub fn finish(&mut self) -> bool {
        if !self.finished {
            if self.sse {
                self.failed |= !self.terminal;
            } else if !self.skipping {
                let body = std::mem::take(&mut self.scratch);
                self.json(&body, "");
            }
            self.scratch.clear();
            self.finished = true;
        }
        self.failed
    }

    fn frame(&mut self, frame: &[u8]) {
        let Ok(text) = std::str::from_utf8(frame) else {
            return;
        };
        let mut event = "";
        let mut data = String::new();
        for line in text.lines() {
            if let Some(value) = line.strip_prefix("event:") {
                event = value.strip_prefix(' ').unwrap_or(value);
            } else if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        if is_error_type(event) {
            self.failed = true;
            self.terminal = true;
        }
        if data.trim() == "[DONE]" {
            self.terminal = true;
        } else if !data.is_empty() {
            self.json(data.as_bytes(), event);
        }
    }

    fn json(&mut self, bytes: &[u8], event: &str) {
        let value = match serde_json::from_slice::<Value>(bytes) {
            Ok(value) => value,
            Err(error) => {
                // serde_json bounds nesting depth (128). Valid but deeply nested
                // output, such as a tool input, exhausts that resource limit; it
                // is unavailable telemetry, not evidence the inference failed.
                // serde_json exposes no stable error code, so match its message.
                let depth_limit = error.to_string().starts_with("recursion limit exceeded");
                if !self.sse && !depth_limit {
                    self.json_validity = ResponseValidity::Invalid;
                }
                return;
            }
        };
        if !self.sse && recognized_response(&value) {
            self.json_validity = ResponseValidity::Valid;
        }
        let kind = value.get("type").and_then(Value::as_str).unwrap_or(event);
        let response = value.get("response").unwrap_or(&value);
        // Failed and incomplete responses still consumed the tokens they report.
        let document = value.get("message").unwrap_or(response);
        let failure = is_error_type(kind)
            || nonnull_error(&value)
            || nonnull_error(response)
            || matches!(
                response.get("status").and_then(Value::as_str),
                Some("failed" | "incomplete")
            );
        if let Some(usage) = document.get("usage") {
            self.usage(usage, kind);
        }
        if failure {
            self.failed = true;
            self.terminal = true;
            return;
        }
        if matches!(kind, "message_stop" | "response.completed") {
            self.terminal = true;
        }
        if self.sse && !self.first_token && generated_output(&value, kind) {
            self.first_token = true;
            self.sink.record(MetricsUpdate::FirstToken {
                request_id: self.request_id.clone(),
            });
        }
    }

    fn usage(&mut self, usage: &Value, kind: &str) {
        let anthropic = matches!(kind, "message" | "message_start" | "message_delta")
            || usage.get("cache_read_input_tokens").is_some()
            || usage.get("cache_creation_input_tokens").is_some();
        let (input, output, cache) = if anthropic {
            let input = number(usage, "input_tokens");
            let read = number(usage, "cache_read_input_tokens");
            let creation = number(usage, "cache_creation_input_tokens");
            if input.is_some() {
                self.anthropic_input = input;
            }
            if read.is_some() {
                self.anthropic_read = read;
            }
            if creation.is_some() {
                self.anthropic_creation = creation;
            }
            let total = if input.is_some() || read.is_some() || creation.is_some() {
                self.anthropic_input.map(|n| {
                    n.saturating_add(self.anthropic_read.unwrap_or(0))
                        .saturating_add(self.anthropic_creation.unwrap_or(0))
                })
            } else {
                None
            };
            (total, number(usage, "output_tokens"), read)
        } else {
            (
                number(usage, "prompt_tokens").or_else(|| number(usage, "input_tokens")),
                number(usage, "completion_tokens").or_else(|| number(usage, "output_tokens")),
                usage
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .or_else(|| usage.pointer("/input_tokens_details/cached_tokens"))
                    .and_then(Value::as_u64),
            )
        };
        if input.is_some() || output.is_some() {
            self.sink.record(MetricsUpdate::TokenUsage {
                request_id: self.request_id.clone(),
                input_tokens: input,
                output_tokens: output,
                // A virtual response model must not overwrite executed identity.
                selected_model: None,
            });
        }
        if input.is_some() || cache.is_some() {
            self.sink.record(MetricsUpdate::PromptCache {
                request_id: self.request_id.clone(),
                prompt_tokens: input,
                cache_tokens: cache,
                processed_tokens: input,
                prompt_ms: None,
                prompt_tps: None,
            });
        }
    }
}

fn recognized_response(value: &Value) -> bool {
    value.is_object()
        && (value.get("type").and_then(Value::as_str) == Some("message")
            || value.get("choices").is_some_and(Value::is_array)
            || value.get("object").and_then(Value::as_str) == Some("response")
            || value
                .get("type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "response" || kind.starts_with("response."))
            || value.get("response").is_some_and(Value::is_object)
            || value.get("output").is_some_and(Value::is_array))
}

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}

fn nonnull_error(value: &Value) -> bool {
    value.get("error").is_some_and(|error| !error.is_null())
}

fn is_error_type(kind: &str) -> bool {
    matches!(kind, "error" | "response.failed" | "response.incomplete")
}

fn nonempty(value: Option<&Value>) -> bool {
    value.and_then(Value::as_str).is_some_and(|s| !s.is_empty())
}

fn chat_output(value: &Value) -> bool {
    ["content", "reasoning_content", "reasoning", "text"]
        .iter()
        .any(|key| nonempty(value.get(key)))
        || value
            .get("tool_calls")
            .and_then(Value::as_array)
            .is_some_and(|calls| {
                calls.iter().any(|call| {
                    nonempty(call.pointer("/function/arguments"))
                        || nonempty(call.pointer("/function/name"))
                })
            })
        || nonempty(value.pointer("/function_call/arguments"))
        || nonempty(value.pointer("/function_call/name"))
}

fn generated_output(value: &Value, kind: &str) -> bool {
    if kind == "content_block_delta" {
        return match value.pointer("/delta/type").and_then(Value::as_str) {
            Some("text_delta") => nonempty(value.pointer("/delta/text")),
            Some("thinking_delta") => nonempty(value.pointer("/delta/thinking")),
            Some("input_json_delta") => nonempty(value.pointer("/delta/partial_json")),
            _ => false,
        };
    }
    if matches!(
        kind,
        "response.output_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
            | "response.custom_tool_call_input.delta"
    ) {
        return nonempty(value.get("delta"));
    }
    value
        .get("choices")
        .and_then(Value::as_array)
        .is_some_and(|choices| {
            choices
                .iter()
                .any(|choice| choice.get("delta").is_some_and(chat_output))
        })
}
