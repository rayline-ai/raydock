use std::sync::{Arc, Mutex};

use crate::response::{ResponseObserver, ResponseValidity};
use crate::{MetricsSink, MetricsUpdate};

#[derive(Default)]
struct Capture(Mutex<Vec<MetricsUpdate>>);
impl MetricsSink for Capture {
    fn record(&self, update: MetricsUpdate) {
        self.0.lock().expect("capture lock").push(update);
    }
}

fn observe(body: &[u8], sse: bool, chunk: usize) -> (bool, Vec<MetricsUpdate>, ResponseValidity) {
    let sink = Arc::new(Capture::default());
    let mut observer = ResponseObserver::new(sink.clone(), "r".into(), sse);
    assert!(!observer.is_terminal());
    assert!(!observer.has_failed());
    assert_eq!(observer.validity(), ResponseValidity::Unavailable);
    for bytes in body.chunks(chunk) {
        observer.observe(bytes);
    }
    let known_failure = observer.has_failed();
    if known_failure {
        assert!(observer.is_terminal());
    }
    if sse && observer.is_terminal() && !known_failure {
        // A transport may finalize as soon as the terminal arrives, without EOF.
        assert!(!observer.finish());
    }
    let failed = observer.finish();
    assert_eq!(observer.has_failed(), failed);
    assert!(!known_failure || failed);
    assert_eq!(observer.finish(), failed);
    let updates = std::mem::take(&mut *sink.0.lock().expect("capture lock"));
    (failed, updates, observer.validity())
}

// These protocol fixtures own usage normalization and chunk-boundary behavior;
// transport callers need not duplicate parser tests.
#[test]
fn each_protocol_reports_usage_and_cache_across_chunk_splits() {
    let cases = [
        (
            true,
            "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"virtual\",\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\r\n\r\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"é\"}}\r\n\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\r\n\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
            42,
            17,
            30,
        ),
        (
            true,
            "data: {\"choices\":[{\"delta\":{\"content\":\"é\"}}]}\n\ndata: {\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":17,\"prompt_tokens_details\":{\"cached_tokens\":30}}}\n\ndata: [DONE]\n\n",
            42,
            17,
            30,
        ),
        (
            true,
            "event: response.function_call_arguments.delta\ndata: {\"delta\":\"{}\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":17,\"input_tokens_details\":{\"cached_tokens\":30}}}}\n\n",
            42,
            17,
            30,
        ),
        (
            false,
            "{\"type\":\"message\",\"model\":\"virtual\",\"content\":[{\"type\":\"text\",\"text\":\"é\"}],\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":17}}",
            42,
            17,
            30,
        ),
        (
            false,
            "{\"choices\":[{\"message\":{\"content\":\"é\"}}],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":17,\"prompt_tokens_details\":{\"cached_tokens\":30}}}",
            42,
            17,
            30,
        ),
        (
            false,
            "{\"status\":\"completed\",\"output\":[{\"content\":[{\"type\":\"output_text\",\"text\":\"é\"}]}],\"usage\":{\"input_tokens\":42,\"output_tokens\":17,\"input_tokens_details\":{\"cached_tokens\":30}}}",
            42,
            17,
            30,
        ),
    ];
    for (sse, body, input, output, cache) in cases {
        for chunk in [1, 2, 7, 63, body.len()] {
            let (failed, updates, validity) = observe(body.as_bytes(), sse, chunk);
            assert!(!failed, "sse={sse}, chunk={chunk}");
            assert_eq!(validity, ResponseValidity::Valid);
            let mut got_input = None;
            let mut got_output = None;
            let mut got_cache = None;
            let mut first = 0;
            for update in updates {
                match update {
                    MetricsUpdate::TokenUsage {
                        input_tokens,
                        output_tokens,
                        selected_model,
                        ..
                    } => {
                        assert_eq!(selected_model, None);
                        got_input = input_tokens.or(got_input);
                        got_output = output_tokens.or(got_output);
                    }
                    MetricsUpdate::PromptCache { cache_tokens, .. } => {
                        got_cache = cache_tokens.or(got_cache)
                    }
                    MetricsUpdate::FirstToken { .. } => first += 1,
                    _ => panic!("observer emitted lifecycle/identity update"),
                }
            }
            assert_eq!(
                (got_input, got_output, got_cache, first),
                (Some(input), Some(output), Some(cache), usize::from(sse))
            );
        }
    }
}

#[test]
fn only_generated_deltas_mark_first_token_once() {
    let sink = Arc::new(Capture::default());
    let mut observer = ResponseObserver::new(sink.clone(), "r".into(), true);
    observer.observe(b": heartbeat\n\ndata: {\"type\":\"message_start\",\"message\":{}}\n\ndata: {\"type\":\"content_block_start\",\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\ndata: {\"type\":\"response.output_item.added\"}\n\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"signature_delta\",\"signature\":\"metadata\"}}\n\n");
    assert!(sink.0.lock().expect("capture lock").is_empty());
    observer.observe(b"data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{}\"}}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: [DONE]\n\n");
    assert!(!observer.finish());
    assert_eq!(sink.0.lock().expect("capture lock").len(), 1);
}

#[test]
fn failures_and_truncation_do_not_leak_error_content() {
    for (sse, body) in [
        (
            true,
            "event: error\ndata: {\"error\":{\"message\":\"private\"}}\n\n",
        ),
        (
            true,
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"private\"}}}\n\n",
        ),
        (true, "data: {\"type\":\"response.incomplete\"}\n\n"),
        (true, "data: {\"type\":\"message_start\"}\n\n"),
        (true, "data: [DONE]\n"),
        (false, "{\"error\":{\"message\":\"private\"}}"),
        (true, "event: error\ndata: not-json\n\n"),
    ] {
        let (failed, updates, validity) = observe(body.as_bytes(), sse, 1);
        assert!(failed, "{body}");
        assert_eq!(validity, ResponseValidity::Invalid);
        assert!(updates.is_empty());
    }
    for (body, expected) in [
        (
            b"{\"error\":null}".as_slice(),
            ResponseValidity::Unavailable,
        ),
        (b"{\"unknown\":true}", ResponseValidity::Unavailable),
        (b"[]", ResponseValidity::Unavailable),
        (b"{\"usage\":", ResponseValidity::Invalid),
        (b"", ResponseValidity::Invalid),
        (b"not-json", ResponseValidity::Invalid),
        (b"\xff", ResponseValidity::Invalid),
    ] {
        let (failed, updates, validity) = observe(body, false, 1);
        assert!(!failed);
        assert_eq!(validity, expected);
        assert!(updates.is_empty());
    }
    // Unrecognized/malformed frames lose telemetry, but a later terminal still
    // proves stream completion. A malformed trailing frame cannot undo it.
    for body in [
        b"data: not-json\n\ndata: [DONE]\n\n".as_slice(),
        b"data: \xff\n\ndata: [DONE]\n\n",
        b"data: [DONE]\n\ndata: {\"usage\":",
    ] {
        let (failed, updates, validity) = observe(body, true, 1);
        assert!(!failed);
        assert_eq!(validity, ResponseValidity::Valid);
        assert!(updates.is_empty());
    }
}

#[test]
fn oversized_frame_skips_through_delimiter_then_resumes() {
    // The discarded frame contains a fake terminal and usage to ensure neither
    // can be recognized by scanning arbitrary bytes in a skipped payload.
    let mut body = b"data: {\"padding\":\"".to_vec();
    body.extend(std::iter::repeat_n(b'x', 300_000));
    body.extend_from_slice(b"\",\"type\":\"message_stop\",\"usage\":{\"output_tokens\":999}}\r\n\r\ndata: {\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":7}}\r\n\r\ndata: [DONE]\r\n\r\n");
    let (failed, updates, validity) = observe(&body, true, 17);
    assert!(!failed);
    assert_eq!(validity, ResponseValidity::Valid);
    assert!(updates.iter().any(|u| matches!(
        u,
        MetricsUpdate::TokenUsage {
            input_tokens: Some(3),
            output_tokens: Some(7),
            ..
        }
    )));
    assert!(!updates.iter().any(|u| matches!(
        u,
        MetricsUpdate::TokenUsage {
            output_tokens: Some(999),
            ..
        }
    )));
    assert!(observe(&body[..300_010], true, 19).0);
    // A terminal before an oversized, unfinished trailing frame stays success.
    let mut terminal_then_oversized = b"data: [DONE]\n\n".to_vec();
    terminal_then_oversized.extend_from_slice(&body[..300_010]);
    assert!(!observe(&terminal_then_oversized, true, 19).0);

    // A valid large buffered tool response provides no telemetry, not failure.
    let mut json =
        b"{\"choices\":[{\"message\":{\"tool_calls\":[{\"function\":{\"arguments\":\"".to_vec();
    json.extend(std::iter::repeat_n(b'x', 300_000));
    json.extend_from_slice(b"\"}}]}}],\"usage\":{\"completion_tokens\":999}}");
    for chunk in [19, json.len()] {
        let (failed, updates, validity) = observe(&json, false, chunk);
        assert!(!failed);
        assert_eq!(validity, ResponseValidity::Unavailable);
        assert!(updates.is_empty());
    }
}

#[test]
fn failure_terminals_still_report_the_usage_they_consumed() {
    for (sse, body) in [
        (
            true,
            "data: {\"type\":\"response.incomplete\",\"response\":{\"usage\":{\"input_tokens\":40,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":5}}}}\n\n",
        ),
        (
            true,
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"private\"},\"usage\":{\"input_tokens\":40,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":5}}}}\n\n",
        ),
        (
            false,
            "{\"object\":\"response\",\"status\":\"incomplete\",\"usage\":{\"input_tokens\":40,\"output_tokens\":9,\"input_tokens_details\":{\"cached_tokens\":5}}}",
        ),
    ] {
        let (failed, updates, validity) = observe(body.as_bytes(), sse, 7);
        assert!(failed, "{body}");
        assert_eq!(validity, ResponseValidity::Invalid);
        assert!(
            updates.iter().any(|update| matches!(
                update,
                MetricsUpdate::TokenUsage {
                    input_tokens: Some(40),
                    output_tokens: Some(9),
                    ..
                }
            )),
            "{body}"
        );
        assert!(
            updates.iter().any(|update| matches!(
                update,
                MetricsUpdate::PromptCache {
                    cache_tokens: Some(5),
                    ..
                }
            )),
            "{body}"
        );
        assert!(!format!("{updates:?}").contains("private"));
    }
}

fn deep_message(opening: usize, closing: usize) -> String {
    format!(
        "{{\"type\":\"message\",\"content\":[{{\"type\":\"tool_use\",\"input\":{}0{}}}]}}",
        "[".repeat(opening),
        "]".repeat(closing)
    )
}

#[test]
fn depth_limit_is_unavailable_only_when_the_body_is_structurally_complete() {
    // Valid JSON, within the byte bound, deeper than serde_json's recursion limit.
    let valid = deep_message(200, 200);
    assert!(valid.len() < crate::response::MAX_RESPONSE_BYTES);
    let with_brackets_in_string = valid.replace("tool_use", "tool_use\\\"]}[");
    let trailing_garbage = format!("{valid} garbage");
    let mismatched = valid.replacen("]]0", "]}0", 1).replace("0]]", "0}]");
    let mut cases: Vec<(String, ResponseValidity)> = vec![
        (valid.clone(), ResponseValidity::Unavailable),
        (format!("  \r\n{valid}\n "), ResponseValidity::Unavailable),
        (with_brackets_in_string, ResponseValidity::Unavailable),
        // Depth is exhausted before the syntax error, so each of these needs
        // the structural check to be told apart from a complete body.
        (deep_message(200, 199), ResponseValidity::Invalid),
        (
            valid[..valid.len() - 3].to_owned(),
            ResponseValidity::Invalid,
        ),
        (trailing_garbage, ResponseValidity::Invalid),
        (format!("{valid}{valid}"), ResponseValidity::Invalid),
        (mismatched, ResponseValidity::Invalid),
    ];
    // Shallow malformed bodies never reach the depth limit.
    for body in [
        "<html>502 Bad Gateway</html>",
        "{\"type\":\"message\",\"content\":[",
        "",
    ] {
        cases.push((body.to_owned(), ResponseValidity::Invalid));
    }
    for (body, expected) in cases {
        for chunk in [64, body.len().max(1)] {
            let (failed, updates, validity) = observe(body.as_bytes(), false, chunk);
            assert!(!failed, "{body:.80}");
            assert_eq!(validity, expected, "{body:.80}");
            assert!(updates.is_empty());
        }
    }
}

fn oversized_payload() -> String {
    format!(
        "{{\"padding\":\"{}\"}}",
        "x".repeat(crate::response::MAX_RESPONSE_BYTES + 1000)
    )
}

fn deep_payload() -> String {
    format!(
        "{{\"response\":{{\"input\":{}0{}}}}}",
        "[".repeat(200),
        "]".repeat(200)
    )
}

/// Feed `body` and report (terminal, failed) before `finish`.
fn verdict_before_finish(body: &[u8], chunk: usize) -> (bool, bool) {
    let mut observer = ResponseObserver::new(Arc::new(Capture::default()), "r".into(), true);
    for bytes in body.chunks(chunk) {
        observer.observe(bytes);
    }
    (observer.is_terminal(), observer.has_failed())
}

#[test]
fn sse_verdict_comes_from_the_event_name_whatever_the_payload_does() {
    let big = oversized_payload();
    let deep = deep_payload();
    for newline in ["\n", "\r\n", "\r"] {
        for chunk in [1, 5, 4096, usize::MAX] {
            let frame = |event: &str, data: &str| {
                format!("event: {event}{newline}data: {data}{newline}{newline}")
            };
            let feed = |body: String| {
                let chunk = chunk.min(body.len());
                (
                    verdict_before_finish(body.as_bytes(), chunk),
                    observe(body.as_bytes(), true, chunk),
                )
            };
            // An oversized or too-deep terminal success frame, with no [DONE].
            for payload in [&big, &deep] {
                for event in ["response.completed", "message_stop"] {
                    let (before, (failed, _, validity)) = feed(frame(event, payload));
                    assert_eq!(before, (true, false), "{event} {newline:?} {chunk}");
                    assert!(!failed);
                    assert_eq!(validity, ResponseValidity::Valid);
                }
            }
            // Success after earlier ordinary frames, event name split by chunking.
            let stream = format!(
                "event: response.created{newline}data: {{}}{newline}{newline}{}",
                frame("response.completed", &big)
            );
            let (before, (failed, _, validity)) = feed(stream);
            assert_eq!(before, (true, false));
            assert!(!failed);
            assert_eq!(validity, ResponseValidity::Valid);
            // An oversized failure frame is a definite failure.
            for event in ["response.failed", "response.incomplete", "error"] {
                let (before, (failed, _, validity)) = feed(frame(event, &big));
                assert_eq!(before, (true, true), "{event}");
                assert!(failed);
                assert_eq!(validity, ResponseValidity::Invalid);
            }
            // An oversized payload under a non-terminal event is no verdict at all:
            // the stream still lacks a terminal and finishes incomplete as before.
            let ordinary = frame("content_block_delta", &big);
            let (before, (failed, _, validity)) = feed(ordinary.clone());
            assert_eq!(before, (false, false));
            assert!(failed);
            assert_eq!(validity, ResponseValidity::Invalid);
            // ... and a later terminal still completes it.
            let (before, (failed, _, _)) =
                feed(format!("{ordinary}data: [DONE]{newline}{newline}"));
            assert_eq!(before, (true, false));
            assert!(!failed);
        }
    }
    // The optional space after `event:` is optional; an over-long name is no event.
    assert_eq!(
        verdict_before_finish(b"event:message_stop\n\n", 3),
        (true, false)
    );
    let long_name = format!("event: {}\n\n", "response.completed".repeat(10));
    assert_eq!(
        verdict_before_finish(long_name.as_bytes(), 7),
        (false, false)
    );
    // A fake terminal inside a skipped payload, with no event line, is still ignored.
    let fake = format!(
        "data: {{\"padding\":\"{}\",\"type\":\"message_stop\"}}\n\n",
        "x".repeat(crate::response::MAX_RESPONSE_BYTES)
    );
    assert_eq!(verdict_before_finish(fake.as_bytes(), 4096), (false, false));
}
