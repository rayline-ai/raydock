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
