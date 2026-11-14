//! JSON as Python's `json` module reads and writes it.
//!
//! The reference projection round-trips request content through Python's
//! `json.loads` and `json.dumps`, and the encoder reads the resulting text,
//! so both directions are reproduced here exactly: object keys keep their
//! order of arrival (a repeated key keeps its first position and its last
//! value), numbers keep Python's spelling (`1.0` stays `1.0`, `1e5` becomes
//! `100000.0`), `NaN` and `Infinity` are accepted, and only the characters
//! Python escapes are escaped on the way out.

use std::collections::HashMap;
use std::fmt::Write as _;

/// Containers nested deeper than this are refused rather than recursed into.
const MAX_DEPTH: usize = 256;
/// Python refuses to read an integer literal longer than this.
const MAX_INT_DIGITS: usize = 4300;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// The number as Python's `json.dumps` writes it back.
    Number(String),
    String(String),
    Array(Vec<Json>),
    Object(Object),
}

/// A JSON object whose keys keep insertion order, as a Python `dict` does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Object(Vec<(String, Json)>);

impl Object {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        self.0
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// Sets `key`, keeping its position when it is already present.
    pub fn insert(&mut self, key: &str, value: Json) {
        match self.0.iter_mut().find(|(name, _)| name == key) {
            Some(entry) => entry.1 = value,
            None => self.0.push((key.to_owned(), value)),
        }
    }

    pub fn remove(&mut self, key: &str) -> Option<Json> {
        let index = self.0.iter().position(|(name, _)| name == key)?;
        Some(self.0.remove(index).1)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Json)> {
        self.0.iter().map(|(key, value)| (key.as_str(), value))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The string under `key`, when there is one.
    pub fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }
}

impl FromIterator<(String, Json)> for Object {
    fn from_iter<T: IntoIterator<Item = (String, Json)>>(iter: T) -> Self {
        // As `insert` per entry, with an index so a large object builds in
        // linear time.
        let mut entries: Vec<(String, Json)> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        for (key, value) in iter {
            match seen.get(&key) {
                Some(&index) => entries[index].1 = value,
                None => {
                    seen.insert(key.clone(), entries.len());
                    entries.push((key, value));
                }
            }
        }
        Object(entries)
    }
}

impl Json {
    pub fn str(text: &str) -> Json {
        Json::String(text.to_owned())
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&Object> {
        match self {
            Json::Object(object) => Some(object),
            _ => None,
        }
    }

    /// Python truthiness: `None`, `False`, zero and empty containers are false.
    pub fn truthy(&self) -> bool {
        match self {
            Json::Null => false,
            Json::Bool(value) => *value,
            Json::Number(text) => text.parse::<f64>() != Ok(0.0),
            Json::String(text) => !text.is_empty(),
            Json::Array(items) => !items.is_empty(),
            Json::Object(object) => !object.is_empty(),
        }
    }

    /// The value with every object's keys in sorted order.
    pub fn sorted(&self) -> Json {
        match self {
            Json::Array(items) => Json::Array(items.iter().map(Json::sorted).collect()),
            Json::Object(object) => {
                let mut entries: Vec<(String, Json)> = object
                    .0
                    .iter()
                    .map(|(key, value)| (key.clone(), value.sorted()))
                    .collect();
                entries.sort_by(|left, right| left.0.cmp(&right.0));
                Json::Object(Object(entries))
            }
            other => other.clone(),
        }
    }

    /// `json.dumps(value, ensure_ascii=False, sort_keys=sort_keys)`.
    pub fn dumps(&self, sort_keys: bool) -> String {
        let mut out = String::new();
        self.write(sort_keys, &mut out);
        out
    }

    fn write(&self, sort_keys: bool, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Number(text) => out.push_str(text),
            Json::String(text) => write_string(text, out),
            Json::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    item.write(sort_keys, out);
                }
                out.push(']');
            }
            Json::Object(object) => {
                let mut entries: Vec<&(String, Json)> = object.0.iter().collect();
                if sort_keys {
                    // Python orders keys by code point, which is UTF-8 byte order.
                    entries.sort_by(|left, right| left.0.cmp(&right.0));
                }
                out.push('{');
                for (index, (key, value)) in entries.into_iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    write_string(key, out);
                    out.push_str(": ");
                    value.write(sort_keys, out);
                }
                out.push('}');
            }
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for char in text.chars() {
        match char {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            char if (char as u32) < 0x20 => {
                // Writing to a String cannot fail.
                let _ = write!(out, "\\u{:04x}", char as u32);
            }
            char => out.push(char),
        }
    }
    out.push('"');
}

/// Why a text could not be read.
#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// Not JSON: Python's `json.loads` raises `ValueError` here too.
    Syntax(String),
    /// JSON that Python reads but this crate will not (nesting past
    /// `MAX_DEPTH`, an unpaired surrogate escape).
    Unsupported(String),
}

/// `json.loads(text)`.
pub fn loads(text: &str) -> Result<Json, ParseError> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        text,
        at: 0,
    };
    parser.whitespace();
    let value = parser.value(0)?;
    parser.whitespace();
    if parser.at != parser.bytes.len() {
        return Err(parser.syntax("extra data"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    text: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn syntax(&self, what: &str) -> ParseError {
        ParseError::Syntax(format!("{what} at byte {}", self.at))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn whitespace(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn literal(&mut self, word: &str) -> bool {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            true
        } else {
            false
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, ParseError> {
        if depth > MAX_DEPTH {
            return Err(ParseError::Unsupported(format!(
                "JSON nested deeper than {MAX_DEPTH} levels"
            )));
        }
        match self.peek() {
            Some(b'"') => Ok(Json::String(self.string()?)),
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'n') if self.literal("null") => Ok(Json::Null),
            Some(b't') if self.literal("true") => Ok(Json::Bool(true)),
            Some(b'f') if self.literal("false") => Ok(Json::Bool(false)),
            Some(b'N') if self.literal("NaN") => Ok(Json::Number("NaN".into())),
            Some(b'I') if self.literal("Infinity") => Ok(Json::Number("Infinity".into())),
            Some(b'-') if self.literal("-Infinity") => Ok(Json::Number("-Infinity".into())),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.syntax("expecting value")),
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.at += 1;
        let mut items = Vec::new();
        self.whitespace();
        if self.peek() == Some(b']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.whitespace();
            items.push(self.value(depth + 1)?);
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.syntax("expecting ',' or ']'")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, ParseError> {
        self.at += 1;
        let mut entries: Vec<(String, Json)> = Vec::new();
        let mut seen: HashMap<String, usize> = HashMap::new();
        self.whitespace();
        if self.peek() == Some(b'}') {
            self.at += 1;
            return Ok(Json::Object(Object(entries)));
        }
        loop {
            self.whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.syntax("expecting a property name"));
            }
            let key = self.string()?;
            self.whitespace();
            if self.peek() != Some(b':') {
                return Err(self.syntax("expecting ':'"));
            }
            self.at += 1;
            self.whitespace();
            let value = self.value(depth + 1)?;
            match seen.get(&key) {
                Some(&index) => entries[index].1 = value,
                None => {
                    seen.insert(key.clone(), entries.len());
                    entries.push((key, value));
                }
            }
            self.whitespace();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Json::Object(Object(entries)));
                }
                _ => return Err(self.syntax("expecting ',' or '}'")),
            }
        }
    }

    fn string(&mut self) -> Result<String, ParseError> {
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while let Some(byte) = self.peek() {
                if byte == b'"' || byte == b'\\' || byte < 0x20 {
                    break;
                }
                self.at += 1;
            }
            // Only ASCII bytes end a run, so both ends are char boundaries.
            out.push_str(&self.text[start..self.at]);
            match self.peek() {
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                Some(b'\\') => {
                    self.at += 1;
                    self.escape(&mut out)?;
                }
                Some(_) => return Err(self.syntax("control character in string")),
                None => return Err(self.syntax("unterminated string")),
            }
        }
    }

    fn escape(&mut self, out: &mut String) -> Result<(), ParseError> {
        let Some(byte) = self.peek() else {
            return Err(self.syntax("unterminated string"));
        };
        self.at += 1;
        match byte {
            b'"' => out.push('"'),
            b'\\' => out.push('\\'),
            b'/' => out.push('/'),
            b'b' => out.push('\u{08}'),
            b'f' => out.push('\u{0c}'),
            b'n' => out.push('\n'),
            b'r' => out.push('\r'),
            b't' => out.push('\t'),
            b'u' => {
                let first = self.hex4()?;
                let code = if (0xD800..0xDC00).contains(&first)
                    && self.bytes[self.at..].starts_with(b"\\u")
                {
                    let rewind = self.at;
                    self.at += 2;
                    match self.hex4() {
                        Ok(second) if (0xDC00..0xE000).contains(&second) => {
                            0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                        }
                        _ => {
                            self.at = rewind;
                            first
                        }
                    }
                } else {
                    first
                };
                match char::from_u32(code) {
                    Some(char) => out.push(char),
                    None => {
                        return Err(ParseError::Unsupported(
                            "an unpaired surrogate escape in a JSON string".into(),
                        ));
                    }
                }
            }
            _ => return Err(self.syntax("invalid escape")),
        }
        Ok(())
    }

    fn hex4(&mut self) -> Result<u32, ParseError> {
        let digits = self
            .bytes
            .get(self.at..self.at + 4)
            .filter(|digits| digits.iter().all(u8::is_ascii_hexdigit))
            .ok_or_else(|| self.syntax("invalid \\u escape"))?;
        let mut code = 0u32;
        for digit in digits {
            code = code * 16 + (*digit as char).to_digit(16).unwrap_or(0);
        }
        self.at += 4;
        Ok(code)
    }

    fn digits(&mut self) -> usize {
        let start = self.at;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.at += 1;
        }
        self.at - start
    }

    fn number(&mut self) -> Result<Json, ParseError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.digits();
            }
            _ => return Err(self.syntax("expecting value")),
        }
        let integer_end = self.at;
        let mut float = false;
        if self.peek() == Some(b'.') && matches!(self.bytes.get(self.at + 1), Some(b'0'..=b'9')) {
            self.at += 1;
            self.digits();
            float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let rewind = self.at;
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if self.digits() == 0 {
                self.at = rewind;
            } else {
                float = true;
            }
        }
        let raw = &self.text[start..self.at];
        if !float {
            if integer_end - start > MAX_INT_DIGITS {
                return Err(self.syntax("integer literal too long"));
            }
            // Python reads `-0` as the integer 0.
            let zero = raw.trim_start_matches('-').bytes().all(|byte| byte == b'0');
            return Ok(Json::Number(if zero { "0".into() } else { raw.into() }));
        }
        let value: f64 = raw
            .parse()
            .map_err(|_| self.syntax("invalid number literal"))?;
        Ok(Json::Number(float_repr(value)))
    }
}

/// Python's `repr(float)`: the shortest digits that round-trip, written
/// positionally for exponents from -4 up to 15 and in scientific form beyond.
pub fn float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    // Rust's `{:e}` also prints the shortest round-trip digits: `d[.ddd]e[-]x`.
    let scientific = format!("{:e}", value.abs());
    let (mantissa, exponent) = scientific.split_once('e').unwrap_or((&scientific, "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if value.is_sign_negative() {
        out.push('-');
    }
    if (-4..16).contains(&exponent) {
        if exponent < 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n(
                '0',
                exponent.unsigned_abs() as usize - 1,
            ));
            out.push_str(&digits);
        } else {
            let whole = exponent.unsigned_abs() as usize + 1;
            if digits.len() <= whole {
                out.push_str(&digits);
                out.extend(std::iter::repeat_n('0', whole - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..whole]);
                out.push('.');
                out.push_str(&digits[whole..]);
            }
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        let sign = if exponent < 0 { '-' } else { '+' };
        // Writing to a String cannot fail.
        let _ = write!(out, "e{sign}{:02}", exponent.abs());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) -> String {
        loads(text).unwrap().dumps(false)
    }

    #[test]
    fn numbers_come_back_as_python_writes_them() {
        // json.dumps(json.loads(text)) for each literal.
        for (text, python) in [
            ("1", "1"),
            ("-0", "0"),
            ("-0.0", "-0.0"),
            ("1.0", "1.0"),
            ("1e5", "100000.0"),
            ("1E-5", "1e-05"),
            ("0.0001", "0.0001"),
            ("1e16", "1e+16"),
            ("9999999999999998.0", "9999999999999998.0"),
            ("1.5e300", "1.5e+300"),
            ("0.1", "0.1"),
            (
                "123456789012345678901234567890",
                "123456789012345678901234567890",
            ),
            ("2.50", "2.5"),
            ("5e-324", "5e-324"),
            ("1e999", "Infinity"),
            ("-Infinity", "-Infinity"),
            ("NaN", "NaN"),
        ] {
            assert_eq!(round_trip(text), python, "{text}");
        }
    }

    #[test]
    fn keys_keep_their_order_and_a_repeat_keeps_its_first_position() {
        assert_eq!(
            round_trip(r#"{"b": 1, "a": 2, "b": 3}"#),
            r#"{"b": 3, "a": 2}"#
        );
        assert_eq!(
            loads(r#"{"b": 1, "a": {"z": 1, "é": 2, "y": 3}}"#)
                .unwrap()
                .dumps(true),
            r#"{"a": {"y": 3, "z": 1, "é": 2}, "b": 1}"#
        );
    }

    #[test]
    fn strings_escape_only_what_python_escapes() {
        let text = r#""a\"b\\c\/d\n\t\u0001\u00e9\ud83d\ude00\u2028<\u007f""#;
        assert_eq!(
            round_trip(text),
            "\"a\\\"b\\\\c/d\\n\\t\\u0001é😀\u{2028}<\u{7f}\""
        );
    }

    #[test]
    fn what_python_rejects_is_a_syntax_error() {
        for text in [
            "",
            "01",
            "1.",
            "[1,]",
            "{\"a\":1,}",
            "\"a\nb\"",
            "'a'",
            "[1] 2",
            "\"\\x41\"",
            "nul",
            "-",
            "{1: 2}",
        ] {
            assert!(
                matches!(loads(text), Err(ParseError::Syntax(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn deep_nesting_and_lone_surrogates_are_unsupported_not_misread() {
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(matches!(loads(&deep), Err(ParseError::Unsupported(_))));
        assert!(matches!(
            loads(r#""\ud83d""#),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn an_object_collects_in_order_last_value_wins_and_in_linear_time() {
        let object: Object = [("b", 1), ("a", 2), ("b", 3)]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), Json::Number(value.to_string())))
            .collect();
        let keys: Vec<&str> = object.iter().map(|(key, _)| key).collect();
        assert_eq!(keys, ["b", "a"]);
        assert_eq!(object.get("b"), Some(&Json::Number("3".to_owned())));
        let started = std::time::Instant::now();
        let large: Object = (0..200_000)
            .map(|index| (format!("k{index}"), Json::Null))
            .collect();
        assert_eq!(large.iter().count(), 200_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn truthiness_follows_python() {
        for (text, truthy) in [
            ("null", false),
            ("false", false),
            ("0", false),
            ("0.0", false),
            ("\"\"", false),
            ("[]", false),
            ("{}", false),
            ("[0]", true),
            ("\"0\"", true),
            ("NaN", true),
        ] {
            assert_eq!(loads(text).unwrap().truthy(), truthy, "{text}");
        }
    }
}
