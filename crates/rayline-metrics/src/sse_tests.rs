use super::*;

type Events = Vec<(String, Option<String>)>;

fn parse(chunks: &[&[u8]]) -> Events {
    let mut parser = SseParser::new();
    let mut events = Events::new();
    let mut on_event = |event: &str, data: Option<&[u8]>| {
        events.push((
            event.to_owned(),
            data.map(|data| String::from_utf8_lossy(data).into_owned()),
        ));
    };
    for chunk in chunks {
        for &byte in *chunk {
            parser.push(byte, &mut on_event);
        }
    }
    events
}

/// The events of `input`, which must not depend on how it is chunked.
fn events(input: &[u8]) -> Events {
    let whole = parse(&[input]);
    let bytes: Vec<&[u8]> = input.chunks(1).collect();
    assert_eq!(parse(&bytes), whole, "byte at a time: {input:?}");
    for split in 0..=input.len().min(2000) {
        let (head, tail) = input.split_at(split);
        assert_eq!(parse(&[head, tail]), whole, "split at {split}: {input:?}");
    }
    whole
}

fn message(data: &str) -> (String, Option<String>) {
    (String::new(), Some(data.to_owned()))
}

fn named(event: &str, data: &str) -> (String, Option<String>) {
    (event.to_owned(), Some(data.to_owned()))
}

#[test]
fn fields_follow_the_event_stream_algorithm() {
    let cases: Vec<(&[u8], Events)> = vec![
        (b"data: a\n\n", vec![message("a")]),
        // One leading space is dropped, not two; no space is fine.
        (b"data:  x\n\n", vec![message(" x")]),
        (b"data:x\n\n", vec![message("x")]),
        // Several data lines join with newlines; a colonless `data` is an empty value.
        (b"data: a\ndata: b\n\n", vec![message("a\nb")]),
        (b"data\n\n", vec![message("")]),
        (b"data\ndata\n\n", vec![message("\n")]),
        (b"data:\ndata: b\n\n", vec![message("\nb")]),
        // `event` sets the type; a colonless `event` clears it; the last one wins.
        (b"event: x\ndata: 1\n\n", vec![named("x", "1")]),
        (b"event: x\nevent\ndata: 1\n\n", vec![message("1")]),
        (b"event: x\nevent:\ndata: 1\n\n", vec![message("1")]),
        (b"event: x\nevent: y\ndata: 1\n\n", vec![named("y", "1")]),
        (b"event:x\ndata: 1\n\n", vec![named("x", "1")]),
        // A block with no data field dispatches nothing, and resets the event type.
        (b"event: x\n\n", vec![]),
        (b"event: x\n\ndata: 1\n\n", vec![message("1")]),
        (b"\n\n\n", vec![]),
        // The type does not carry over to the next event.
        (
            b"event: a\ndata: 1\n\ndata: 2\n\n",
            vec![named("a", "1"), message("2")],
        ),
        // Comments, `id` and `retry` and unknown fields are ignored.
        (
            b": note\nid: 5\nretry: 10\nother: z\ndata: a\n\n",
            vec![message("a")],
        ),
        (b": event: x\ndata: a\n\n", vec![message("a")]),
        (b"id: 1\n\n", vec![]),
        // A field-looking line inside a data value is data.
        (b"data: event: error\n\n", vec![message("event: error")]),
        (b"data: a:b\n\n", vec![message("a:b")]),
        // Names are exact: no trimming, no case folding.
        (b"Data: a\n\n", vec![]),
        (b" data: a\n\n", vec![]),
        (b"data : a\n\n", vec![]),
        // An event at the end of the stream that was not dispatched is discarded.
        (b"data: a\n\ndata: b", vec![message("a")]),
        (b"data: a\n\ndata: b\n", vec![message("a")]),
        (b"event: x\ndata: 1", vec![]),
        // Line terminators: CRLF, LF and CR, mixed, with lone CRs and CR LF pairs.
        (b"data: a\r\n\r\n", vec![message("a")]),
        (b"data: a\r\r", vec![message("a")]),
        (
            b"data: a\r\rdata: b\r\n\r\n",
            vec![message("a"), message("b")],
        ),
        (
            b"data: a\n\r\ndata: b\r\n\n",
            vec![message("a"), message("b")],
        ),
        (
            b"data: a\r\n\rdata: b\n\n",
            vec![message("a"), message("b")],
        ),
        // One BOM at the very start is stripped; anything else is a field name.
        (b"\xEF\xBB\xBFdata: a\n\n", vec![message("a")]),
        (b"\xEF\xBB\xBF\xEF\xBB\xBFdata: a\n\n", vec![]),
        (b"data: a\n\n\xEF\xBB\xBFdata: b\n\n", vec![message("a")]),
        (b"\xEF\xBBdata: a\n\n", vec![]),
        (b"\xEFdata: a\n\n", vec![]),
        (b"\xEF\xBB", vec![]),
        // Invalid UTF-8 is just bytes here.
        (b"data: \xFF\n\n", vec![message("\u{fffd}")]),
    ];
    for (input, expected) in cases {
        assert_eq!(events(input), expected, "{input:?}");
    }
}

#[test]
fn over_long_names_and_values_are_unknown_and_never_buffered() {
    let exactly = "x".repeat(MAX_EVENT_NAME);
    let one_more = "x".repeat(MAX_EVENT_NAME + 1);
    let long_field = "f".repeat(10_000);
    let long_value = "v".repeat(10_000);
    for (input, expected) in [
        (
            format!("event: {exactly}\ndata: 1\n\n"),
            named(&exactly, "1"),
        ),
        (format!("event: {one_more}\ndata: 1\n\n"), message("1")),
        (format!("event: {long_value}\ndata: 1\n\n"), message("1")),
        // A long unknown field or value is skipped; later lines still count.
        (format!("{long_field}: z\ndata: 1\n\n"), message("1")),
        (format!("{long_field}\ndata: 1\n\n"), message("1")),
        (format!("other: {long_value}\ndata: 1\n\n"), message("1")),
        // An over-long name replaces an earlier valid one (it is the last `event`).
        (
            format!("event: a\nevent: {one_more}\ndata: 1\n\n"),
            message("1"),
        ),
    ] {
        assert_eq!(events(input.as_bytes()), vec![expected], "{input:.40}");
    }
}

#[test]
fn oversized_data_is_dispatched_unavailable_and_the_stream_resumes() {
    let big = "x".repeat(MAX_RESPONSE_BYTES + 5);
    let at_limit = "y".repeat(MAX_RESPONSE_BYTES - 1);
    let input =
        format!("event: first\ndata: {big}\n\ndata: {at_limit}\n\nevent: last\ndata: ok\n\n");
    let whole = parse(&[input.as_bytes()]);
    assert_eq!(whole.len(), 3);
    assert_eq!(whole[0], ("first".to_owned(), None));
    assert_eq!(whole[1], message(&at_limit));
    assert_eq!(whole[2], named("last", "ok"));
    // The same events whatever the chunking.
    for split in [1, 1000, MAX_RESPONSE_BYTES, input.len() - 1] {
        let (head, tail) = input.as_bytes().split_at(split);
        assert_eq!(parse(&[head, tail]), whole, "split at {split}");
    }
    // Oversize through several lines of one event, with CRLF terminators.
    let lines = format!(
        "event: e\ndata: {}\ndata: {}\r\n\r\ndata: 1\r\n\r\n",
        "x".repeat(200_000),
        "z".repeat(200_000)
    );
    assert_eq!(
        parse(&[lines.as_bytes()]),
        vec![("e".to_owned(), None), message("1")]
    );
}
