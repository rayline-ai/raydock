//! The WHATWG `text/event-stream` algorithm as one byte-at-a-time state machine,
//! so the events it reports never depend on how the body was split into chunks.
//!
//! Lines end with CRLF, LF or CR (a CR LF pair is one terminator, even across
//! chunks); one leading BOM is stripped; a blank line dispatches; `:` lines are
//! comments; a line splits at its first `:` into field and value (one leading
//! space dropped), and a line without `:` is a field with an empty value. Only
//! `event` and `data` matter here. A block with no `data` field dispatches
//! nothing, and an event left incomplete at end of stream is never dispatched.
//!
//! Memory and time are bounded: field names and `event` values are held in a
//! small head, longer ones are unknown and ignored, and an event's data stops
//! growing at `MAX_RESPONSE_BYTES`. Such an event is still dispatched, with its
//! data reported unavailable (`None`).

use crate::response::MAX_RESPONSE_BYTES;

const BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];
/// Longest `event` value kept; a longer one is an unknown name (kept empty).
const MAX_EVENT_NAME: usize = 64;
/// Longest field name kept; a longer one cannot be `event` or `data`.
const MAX_FIELD_NAME: usize = 80;

#[derive(Clone, Copy)]
enum Line {
    /// No byte of this line yet; a terminator here is a blank line.
    Start,
    /// Reading the field name, up to the first `:`.
    Name,
    /// Reading the value of a `data` field.
    Data,
    /// Reading the value of an `event` field.
    Event,
    /// A field that is neither (or a comment): skip to the terminator.
    Ignore,
}

pub(crate) struct SseParser {
    /// Bytes of a leading BOM matched so far; `None` once the start is decided.
    bom_matched: Option<usize>,
    after_cr: bool,
    line: Line,
    /// The next byte is the first after the `:` and may be the dropped space.
    skip_space: bool,
    /// Field name, or `event` value, of the current line.
    head: Vec<u8>,
    event: String,
    data: Vec<u8>,
    data_seen: bool,
    overflow: bool,
}

impl Default for SseParser {
    fn default() -> Self {
        Self::new()
    }
}

impl SseParser {
    pub(crate) fn new() -> Self {
        Self {
            bom_matched: Some(0),
            after_cr: false,
            line: Line::Start,
            skip_space: false,
            head: Vec::new(),
            event: String::new(),
            data: Vec::new(),
            data_seen: false,
            overflow: false,
        }
    }

    /// Feed one byte; `on_event(event_type, data)` runs for each dispatched
    /// event. An empty type is the default `message`. `data` is `None` when it
    /// exceeded `MAX_RESPONSE_BYTES`.
    pub(crate) fn push(&mut self, byte: u8, on_event: &mut impl FnMut(&str, Option<&[u8]>)) {
        if let Some(matched) = self.bom_matched {
            if byte == BOM[matched] {
                self.bom_matched = (matched + 1 < BOM.len()).then_some(matched + 1);
                return;
            }
            // Not a BOM after all: the held bytes were ordinary line bytes.
            self.bom_matched = None;
            for &held in &BOM[..matched] {
                self.line_byte(held, on_event);
            }
        }
        self.line_byte(byte, on_event);
    }

    fn line_byte(&mut self, byte: u8, on_event: &mut impl FnMut(&str, Option<&[u8]>)) {
        if byte == b'\n' && self.after_cr {
            self.after_cr = false;
            return;
        }
        self.after_cr = byte == b'\r';
        if byte == b'\r' || byte == b'\n' {
            self.end_line(on_event);
        } else {
            self.field_byte(byte);
        }
    }

    fn field_byte(&mut self, byte: u8) {
        match self.line {
            Line::Start | Line::Name => self.name_byte(byte),
            Line::Data => {
                if !self.take_space(byte) {
                    self.append(byte);
                }
            }
            Line::Event => {
                // One byte past the cap is kept so an over-long value is detectable.
                if !self.take_space(byte) && self.head.len() <= MAX_EVENT_NAME {
                    self.head.push(byte);
                }
            }
            Line::Ignore => {}
        }
    }

    fn name_byte(&mut self, byte: u8) {
        self.line = Line::Name;
        if byte == b':' {
            self.line = match self.head.as_slice() {
                b"data" => Line::Data,
                b"event" => Line::Event,
                _ => Line::Ignore,
            };
            self.head.clear();
            self.skip_space = true;
        } else if self.head.len() < MAX_FIELD_NAME {
            self.head.push(byte);
        } else {
            self.line = Line::Ignore;
            self.head.clear();
        }
    }

    /// Whether `byte` is the single space after the `:`, which is not part of the value.
    fn take_space(&mut self, byte: u8) -> bool {
        let skip = self.skip_space && byte == b' ';
        self.skip_space = false;
        skip
    }

    fn append(&mut self, byte: u8) {
        if self.overflow {
            return;
        }
        if self.data.len() >= MAX_RESPONSE_BYTES {
            self.overflow = true;
            self.data.clear();
        } else {
            self.data.push(byte);
        }
    }

    fn end_line(&mut self, on_event: &mut impl FnMut(&str, Option<&[u8]>)) {
        match self.line {
            Line::Start => self.dispatch(on_event),
            // A line without a colon: the whole line is the field name.
            Line::Name => match self.head.as_slice() {
                b"data" => self.end_data_line(),
                b"event" => self.event.clear(),
                _ => {}
            },
            Line::Data => self.end_data_line(),
            Line::Event => {
                self.event = match std::str::from_utf8(&self.head) {
                    Ok(name) if name.len() <= MAX_EVENT_NAME => name.to_owned(),
                    _ => String::new(),
                };
            }
            Line::Ignore => {}
        }
        self.head.clear();
        self.line = Line::Start;
    }

    fn end_data_line(&mut self) {
        self.append(b'\n');
        self.data_seen = true;
    }

    fn dispatch(&mut self, on_event: &mut impl FnMut(&str, Option<&[u8]>)) {
        let event = std::mem::take(&mut self.event);
        let data_seen = std::mem::take(&mut self.data_seen);
        let overflow = std::mem::take(&mut self.overflow);
        if data_seen {
            if overflow {
                on_event(&event, None);
            } else {
                // Every data line appended its own terminating newline.
                self.data.pop();
                on_event(&event, Some(&self.data));
            }
        }
        self.data.clear();
    }
}

#[cfg(test)]
#[path = "sse_tests.rs"]
mod tests;
