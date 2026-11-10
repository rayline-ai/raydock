use std::sync::{Arc, Mutex};

use crate::response::{MAX_RESPONSE_BYTES, ResponseObserver, ResponseValidity};
use crate::{MetricsSink, MetricsUpdate};

#[derive(Default)]
struct Capture(Mutex<Vec<MetricsUpdate>>);
impl MetricsSink for Capture {
    fn record(&self, update: MetricsUpdate) {
        self.0.lock().expect("capture lock").push(update);
    }
}

fn observe(body: &[u8], sse: bool, chunk: usize) -> (bool, Vec<MetricsUpdate>, ResponseValidity) {
    observe_chunks(body.chunks(chunk), sse)
}

fn observe_chunks<'a>(
    chunks: impl IntoIterator<Item = &'a [u8]>,
    sse: bool,
) -> (bool, Vec<MetricsUpdate>, ResponseValidity) {
    let sink = Arc::new(Capture::default());
    let mut observer = ResponseObserver::new(sink.clone(), "r".into(), sse);
    assert!(!observer.is_terminal());
    assert!(!observer.has_failed());
    assert_eq!(observer.validity(), ResponseValidity::Unavailable);
    for bytes in chunks {
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
            "event: message_start\r\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"virtual\",\"usage\":{\"input_tokens\":2,\"cache_creation_input_tokens\":10,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}\r\n\r\ndata: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"é\"}}\r\n\r\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\r\n\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n",
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
            "event: response.function_call_arguments.delta\ndata: {\"delta\":\"{}\"}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":17,\"input_tokens_details\":{\"cached_tokens\":30}}}}\n\n",
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

/// Feed `body` and report (terminal, failed) before `finish`, whole and byte by byte.
fn verdict_before_finish(body: &[u8]) -> (bool, bool) {
    let verdicts = [body.len().max(1), 1].map(|chunk| {
        let mut observer = ResponseObserver::new(Arc::new(Capture::default()), "r".into(), true);
        for bytes in body.chunks(chunk) {
            observer.observe(bytes);
        }
        (observer.is_terminal(), observer.has_failed())
    });
    assert_eq!(verdicts[0], verdicts[1], "chunking changed the verdict");
    verdicts[0]
}

fn big_payload() -> String {
    format!(
        "{{\"padding\":\"{}\"}}",
        "x".repeat(MAX_RESPONSE_BYTES + 1000)
    )
}

fn deep_payload() -> String {
    format!(
        "{{\"response\":{{\"input\":{}0{}}}}}",
        "[".repeat(200),
        "]".repeat(200)
    )
}

/// Streams in the shapes the three protocols use, with `nl` line endings.
fn sample_streams(nl: &str) -> Vec<String> {
    let anthropic = [
        "event: message_start",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":2,\"cache_read_input_tokens\":30,\"output_tokens\":0}}}",
        "",
        ": keep-alive",
        "",
        "event: content_block_delta",
        "data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"é\"}}",
        "",
        "event: message_delta",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}",
        "",
        "event: message_stop",
        "data: {\"type\":\"message_stop\"}",
        "",
        "",
    ];
    let responses = [
        "event: response.created",
        "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}",
        "",
        "event: response.output_text.delta",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}",
        "",
        "event: response.completed",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":17,\"input_tokens_details\":{\"cached_tokens\":30}}}}",
        "",
        "",
    ];
    let chat = [
        "data: {\"choices\":[{\"delta\":{\"content\":\"é\"}}]}",
        "",
        "data: {\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":17,\"prompt_tokens_details\":{\"cached_tokens\":30}}}",
        "",
        "data: [DONE]",
        "",
        "",
    ];
    let mut streams = Vec::new();
    for lines in [&anthropic[..], &responses[..], &chat[..]] {
        let body = lines.join(nl);
        streams.push(body.clone());
        streams.push(format!("\u{feff}{body}"));
    }
    streams
}

#[test]
fn every_split_of_a_stream_gives_the_unsplit_result() {
    for nl in ["\n", "\r\n", "\r"] {
        for stream in sample_streams(nl) {
            let bytes = stream.as_bytes();
            let (failed, updates, validity) = observe_chunks([bytes], true);
            assert!(!failed, "{nl:?} {stream:.60}");
            assert_eq!(validity, ResponseValidity::Valid);
            assert!(!updates.is_empty());
            let expected = format!("{updates:?}");
            for split in 0..=bytes.len() {
                let (head, tail) = bytes.split_at(split);
                let (failed, updates, validity) = observe_chunks([head, tail], true);
                assert!(!failed, "split at {split}");
                assert_eq!(validity, ResponseValidity::Valid);
                assert_eq!(format!("{updates:?}"), expected, "split at {split}");
            }
            let (failed, updates, validity) = observe_chunks(bytes.chunks(1), true);
            assert!(!failed);
            assert_eq!(validity, ResponseValidity::Valid);
            assert_eq!(format!("{updates:?}"), expected, "byte at a time");
        }
    }
}

#[test]
fn stream_verdict_follows_event_framing_not_payload() {
    let big = big_payload();
    let deep = deep_payload();
    let long_name = "response.completed".repeat(10);
    let neutral_completed =
        "event: content_block_delta\ndata: {\"type\":\"response.completed\"}\n\n".to_owned();
    let neutral_oversized = format!(
        "event: content_block_delta\ndata: {}\n\n",
        big.replacen("{", "{\"type\":\"message_stop\",", 1)
    );
    let neutral_deep = format!(
        "event: content_block_delta\ndata: {{\"type\":\"response.completed\",\"x\":{}0{}}}\n\n",
        "[".repeat(200),
        "]".repeat(200)
    );
    let table: Vec<(String, (bool, bool))> = vec![
        // SSE framing: these dispatch no event, or a different one, so nothing ends.
        (
            "event: response.completed\nevent\ndata: {}\n\n".into(),
            (false, false),
        ),
        ("event: response.completed\n\n".into(), (false, false)),
        ("event: error\n\n".into(), (false, false)),
        (
            "event: response.completed\n\ndata: {}\n\n".into(),
            (false, false),
        ),
        ("data: event: error\n\n".into(), (false, false)),
        (
            format!("event: {long_name}\ndata: {{}}\n\n"),
            (false, false),
        ),
        ("event: message_stop\ndata: {}".into(), (false, false)),
        ("data: [DONE]\n".into(), (false, false)),
        ("data: [DONE]\ndata: x\n\n".into(), (false, false)),
        ("data: [DONE] \n\n".into(), (false, false)),
        // Terminal success is the event type or an exact [DONE] and nothing else.
        ("event: message_stop\ndata: {}\n\n".into(), (true, false)),
        (
            "\u{feff}event: response.completed\r\ndata: {}\r\n\r\n".into(),
            (true, false),
        ),
        ("data: [DONE]\n\n".into(), (true, false)),
        (neutral_completed, (false, false)),
        (neutral_oversized, (false, false)),
        (neutral_deep, (false, false)),
        (
            "data: {\"type\":\"message_stop\"}\n\n".into(),
            (false, false),
        ),
        // An oversized or too-deep payload under a terminal event cannot undo it.
        (
            format!("event: response.completed\ndata: {big}\n\n"),
            (true, false),
        ),
        (
            format!("event: response.completed\ndata: {deep}\n\n"),
            (true, false),
        ),
        (
            format!("event: message_stop\ndata: {deep}\n\n"),
            (true, false),
        ),
        (
            "event: response.completed\ndata: {\"truncated\n\n".to_owned(),
            (true, false),
        ),
        // Oversized failure events are failures; the payload need not be read.
        (
            format!("event: response.failed\ndata: {big}\n\n"),
            (true, true),
        ),
        (
            format!("event: response.incomplete\ndata: {big}\n\n"),
            (true, true),
        ),
        (format!("event: error\ndata: {big}\n\n"), (true, true)),
        ("event: error\ndata: not-json\n\n".into(), (true, true)),
        // A payload may add a failure, under any event name or none.
        (
            "data: {\"error\":{\"message\":\"x\"}}\n\n".into(),
            (true, true),
        ),
        (
            "event: message\ndata: {\"response\":{\"status\":\"incomplete\"}}\n\n".into(),
            (true, true),
        ),
        (
            "data: {\"response\":{\"status\":\"failed\"}}\n\n".into(),
            (true, true),
        ),
        (
            "data: {\"type\":\"response.failed\"}\n\n".into(),
            (true, true),
        ),
        (
            "event: response.completed\ndata: {\"response\":{\"status\":\"incomplete\"}}\n\n"
                .into(),
            (true, true),
        ),
        // A null error is not a failure.
        (
            "event: message\ndata: {\"error\":null}\n\n".into(),
            (false, false),
        ),
        // Nothing after a terminal changes the verdict.
        (
            "event: message_stop\ndata: {}\n\nevent: error\ndata: {}\n\n".into(),
            (true, false),
        ),
        (
            "data: [DONE]\n\ndata: {\"error\":{}}\n\n".into(),
            (true, false),
        ),
        (
            "event: error\ndata: {}\n\nevent: message_stop\ndata: {}\n\n".into(),
            (true, true),
        ),
        // An oversized payload under an ordinary event is no verdict at all.
        (
            format!("event: content_block_delta\ndata: {big}\n\n"),
            (false, false),
        ),
    ];
    for (body, expected) in table {
        assert_eq!(
            verdict_before_finish(body.as_bytes()),
            expected,
            "{body:.120}"
        );
        // `finish` agrees: only a stream with no terminal at all ends as a failure.
        let (failed, _, validity) = observe(body.as_bytes(), true, 4096);
        assert_eq!(failed, expected.1 || !expected.0, "{body:.120}");
        assert_eq!(
            validity == ResponseValidity::Valid,
            expected.0 && !expected.1
        );
    }
}

fn deep_body(inner: &str) -> String {
    format!(
        "{{\"type\":\"message\",\"content\":[{{\"type\":\"tool_use\",\"input\":{}{inner}{}}}]}}",
        "[".repeat(200),
        "]".repeat(200)
    )
}

#[test]
fn buffered_json_is_parsed_whole_and_only_in_band_signals_fail() {
    let valid = deep_body("0");
    assert!(valid.len() < MAX_RESPONSE_BYTES);
    let mut cases: Vec<(String, ResponseValidity)> = vec![
        (
            "{\"type\":\"message\",\"content\":[]}".to_owned(),
            ResponseValidity::Valid,
        ),
        // Valid JSON nested past serde_json's recursion limit does not parse.
        (valid.clone(), ResponseValidity::Invalid),
        // So none of these balanced-but-malformed bodies can be told apart from it,
        // and none may be taken for a valid or unavailable one.
        (deep_body("0,"), ResponseValidity::Invalid),
        (deep_body("nul"), ResponseValidity::Invalid),
        (deep_body("01"), ResponseValidity::Invalid),
        (deep_body("\"\\q\""), ResponseValidity::Invalid),
        (deep_body("x"), ResponseValidity::Invalid),
        (
            valid[..valid.len() - 3].to_owned(),
            ResponseValidity::Invalid,
        ),
        (format!("{valid} garbage"), ResponseValidity::Invalid),
        // Shallow malformed bodies.
        (
            "<html>502 Bad Gateway</html>".to_owned(),
            ResponseValidity::Invalid,
        ),
        (
            "{\"type\":\"message\",\"content\":[".to_owned(),
            ResponseValidity::Invalid,
        ),
        (String::new(), ResponseValidity::Invalid),
        // Parsed but unrecognized.
        (
            "{\"unknown\":true}".to_owned(),
            ResponseValidity::Unavailable,
        ),
        // Over the bound: unavailable, never a failure.
        (
            format!("{{\"padding\":\"{}\"}}", "x".repeat(MAX_RESPONSE_BYTES)),
            ResponseValidity::Unavailable,
        ),
    ];
    for (body, expected) in cases.drain(..) {
        for chunk in [64, body.len().max(1)] {
            let (failed, updates, validity) = observe(body.as_bytes(), false, chunk);
            assert!(
                !failed,
                "an unparseable body is not an inference failure: {body:.80}"
            );
            assert_eq!(validity, expected, "{body:.80}");
            assert!(updates.is_empty());
        }
    }
    // A definite in-band failure still fails, and reports its usage.
    let (failed, updates, validity) = observe(
        b"{\"type\":\"message\",\"error\":{\"message\":\"x\"},\"usage\":{\"input_tokens\":1,\"output_tokens\":2}}",
        false,
        16,
    );
    assert!(failed);
    assert_eq!(validity, ResponseValidity::Invalid);
    assert!(!updates.is_empty());
}
