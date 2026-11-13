//! Bounded, passive response observation for Anthropic Messages, OpenAI Chat,
//! and OpenAI Responses. Feed decoded HTTP body bytes (not compressed bytes).
//! The caller owns request start/completion and transport errors. No error
//! text, response models, or lifecycle updates are recorded, and buffered JSON
//! never emits FirstToken: body completion is not token arrival.
//!
//! SSE is parsed to the `text/event-stream` spec by `SseParser`, whose events
//! do not depend on chunking. The stream verdict has one authority: each
//! dispatched event's type (`error`, `response.failed` and
//! `response.incomplete` fail it; `message_stop` and `response.completed`
//! succeed) or an exact `[DONE]` datum. The payload may only add a failure
//! (`error` object, failed/incomplete status or failure `type`) and supply
//! usage and first-token telemetry; it never signals success, and an
//! oversized or unparseable payload costs only telemetry. A stream that ends
//! without a terminal, or in a half-received event, is incomplete.
//!
//! A buffered body is parsed with plain serde_json when `finish` runs. An
//! unparseable body, including valid JSON nested past serde_json's depth
//! limit, is `Invalid`, which is not itself an inference failure: only a
//! definite in-band failure makes `finish` report one. Callers that trust the
//! producer to be well formed may ignore `Invalid`; callers that do not may
//! treat it as an error.

#[cfg(test)]
#[path = "response_tests.rs"]
mod tests;

use serde_json::Value;

use crate::sse::SseParser;
use crate::{MetricsUpdate, SharedMetricsSink};

/// Maximum buffered JSON body or SSE event data size. Larger SSE events are
/// still dispatched, with their data unavailable; larger JSON is unavailable.
pub const MAX_RESPONSE_BYTES: usize = 256 * 1024;

/// Response classification independent of transport completion and telemetry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseValidity {
    /// A recognized protocol response, or an SSE stream with a success terminal.
    Valid,
    /// A buffered body serde_json cannot parse (valid JSON nested too deeply
    /// included), a definite in-band failure, or a finished SSE stream without
    /// a success terminal.
    Invalid,
    /// Not yet finalized, oversized JSON, or an unrecognized JSON shape.
    Unavailable,
}

pub struct ResponseObserver {
    sink: SharedMetricsSink,
    request_id: String,
    sse: bool,
    parser: SseParser,
    /// Buffered JSON body; dropped (and `oversized` set) past the bound.
    body: Vec<u8>,
    oversized: bool,
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
            parser: SseParser::new(),
            body: Vec::new(),
            oversized: false,
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
        if self.sse {
            // The parser is moved out so its callback can borrow `self`.
            let mut parser = std::mem::take(&mut self.parser);
            let mut on_event = |event: &str, data: Option<&[u8]>| self.dispatch(event, data);
            for &byte in bytes {
                parser.push(byte, &mut on_event);
            }
            self.parser = parser;
        } else if !self.oversized {
            if bytes.len() > MAX_RESPONSE_BYTES.saturating_sub(self.body.len()) {
                self.body.clear();
                self.oversized = true;
            } else {
                self.body.extend_from_slice(bytes);
            }
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
    /// classified SSE without a terminal marker as incomplete). An unparseable
    /// or oversized body is not a known inference failure.
    pub fn has_failed(&self) -> bool {
        self.failed
    }

    /// Classify the observed response. Call after `finish` for buffered JSON;
    /// before finalization its validity is unavailable. Buffered JSON that does
    /// not parse is `Invalid` whatever the reason, including valid JSON deeper
    /// than serde_json's recursion limit, so a caller that trusts the producer
    /// should not fail a request on `Invalid` alone. SSE is valid after a
    /// success terminal and invalid after a failure or a finished stream with
    /// no terminal. This does not establish transport completion itself.
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

    /// Finalize once, returning `true` only for a definite in-band failure, or
    /// for SSE that ended without a terminal (`message_stop`,
    /// `response.completed` or `[DONE]`). An event half-received at the end is
    /// discarded. An unparseable or oversized buffered body is never a failure.
    /// Once a terminal has been seen nothing after it changes the verdict.
    /// Repeated calls return the same result. A `false` result means no known
    /// failure, not validated inference success: callers must still establish
    /// transport completion (for example, received Content-Length bytes for
    /// JSON). No lifecycle event or buffered JSON FirstToken is emitted.
    pub fn finish(&mut self) -> bool {
        if !self.finished {
            if self.sse {
                self.failed |= !self.terminal;
            } else if !self.oversized {
                match serde_json::from_slice::<Value>(&self.body) {
                    Ok(value) => {
                        if recognized_response(&value) {
                            self.json_validity = ResponseValidity::Valid;
                        }
                        self.payload(&value, "");
                    }
                    Err(_) => self.json_validity = ResponseValidity::Invalid,
                }
            }
            self.body.clear();
            self.finished = true;
        }
        self.failed
    }

    /// One SSE event. The verdict comes from framing alone; the payload, when
    /// it fit the bound and parses, can add a failure and supply telemetry.
    fn dispatch(&mut self, event: &str, data: Option<&[u8]>) {
        if self.terminal {
            return;
        }
        if is_error_type(event) {
            self.failed = true;
            self.terminal = true;
        } else if matches!(event, "message_stop" | "response.completed") {
            self.terminal = true;
        }
        match data {
            Some(b"[DONE]") => self.terminal = true,
            Some(data) => {
                if let Ok(value) = serde_json::from_slice::<Value>(data) {
                    self.payload(&value, event);
                }
            }
            None => {}
        }
    }

    /// Telemetry and failure signals of one parsed payload. Its `type` selects
    /// the usage shape and first-token rule; it can name a failure but never a success.
    fn payload(&mut self, value: &Value, event: &str) {
        let payload_type = value.get("type").and_then(Value::as_str);
        let kind = payload_type.unwrap_or(event);
        let response = value.get("response").unwrap_or(value);
        // Failed and incomplete responses still consumed the tokens they report.
        let document = value.get("message").unwrap_or(response);
        if let Some(usage) = document.get("usage") {
            self.usage(usage, kind);
        }
        if payload_type.is_some_and(is_error_type)
            || nonnull_error(value)
            || nonnull_error(response)
            || matches!(
                response.get("status").and_then(Value::as_str),
                Some("failed" | "incomplete")
            )
        {
            self.failed = true;
            self.terminal = true;
            return;
        }
        if self.sse && !self.first_token && generated_output(value, kind) {
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
