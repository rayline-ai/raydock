//! Strict readers over JSON: every key required, none unknown, no coercion
//! (`"5"` is not a count, `true` is not 1, `3.0` is not an integer).

use serde_json::{Map, Value};

/// Where a member is malformed, and why.
#[derive(Debug)]
pub(crate) struct Bad {
    pub at: String,
    pub why: String,
}

pub(crate) type Checked<T> = Result<T, Bad>;

pub(crate) fn bad<T>(at: &str, why: impl Into<String>) -> Checked<T> {
    Err(Bad {
        at: at.to_owned(),
        why: why.into(),
    })
}

pub(crate) fn join(at: &str, key: &str) -> String {
    if at.is_empty() {
        key.to_owned()
    } else {
        format!("{at}.{key}")
    }
}

static NULL: Value = Value::Null;

/// A member of an object `object` has already checked.
pub(crate) fn member<'a>(map: &'a Map<String, Value>, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

/// An object with exactly `required` plus any of `optional`.
pub(crate) fn object<'a>(
    value: &'a Value,
    at: &str,
    required: &[&str],
    optional: &[&str],
) -> Checked<&'a Map<String, Value>> {
    let Value::Object(map) = value else {
        return bad(at, "must be an object");
    };
    for key in required {
        if !map.contains_key(*key) {
            return bad(&join(at, key), "is required");
        }
    }
    for key in map.keys() {
        if !required.contains(&key.as_str()) && !optional.contains(&key.as_str()) {
            return bad(at, "has an unsupported member");
        }
    }
    Ok(map)
}

/// A nullable member: `None` for JSON null.
pub(crate) fn nullable<'a, T>(
    value: &'a Value,
    read: impl FnOnce(&'a Value) -> Checked<T>,
) -> Checked<Option<T>> {
    if value.is_null() {
        Ok(None)
    } else {
        read(value).map(Some)
    }
}

pub(crate) fn boolean(value: &Value, at: &str) -> Checked<bool> {
    match value {
        Value::Bool(flag) => Ok(*flag),
        _ => bad(at, "must be a boolean"),
    }
}

pub(crate) fn integer(value: &Value, at: &str, low: u64, high: u64) -> Checked<u64> {
    match value.as_u64() {
        Some(number) if (low..=high).contains(&number) => Ok(number),
        _ => bad(at, format!("must be an integer in [{low}, {high}]")),
    }
}

/// A JSON number in `[low, high]`, or `(low, high]` when `open_low`.
pub(crate) fn float(value: &Value, at: &str, low: f64, high: f64, open_low: bool) -> Checked<f64> {
    let number = match value {
        Value::Number(number) => number.as_f64(),
        _ => None,
    };
    match number {
        Some(n) if n.is_finite() && n <= high && (n > low || (!open_low && n >= low)) => Ok(n),
        _ => bad(
            at,
            format!(
                "must be a number in {}{low}, {high}]",
                if open_low { '(' } else { '[' }
            ),
        ),
    }
}

pub(crate) fn one_of(value: &Value, at: &str, allowed: &[&str]) -> Checked<String> {
    match value.as_str() {
        Some(text) if allowed.contains(&text) => Ok(text.to_owned()),
        _ => bad(at, format!("must be one of {}", allowed.join(", "))),
    }
}

/// A string of 1 to `max` characters, each alphanumeric ASCII or one of `extra`.
fn charset(value: &Value, at: &str, max: usize, extra: &str, what: &str) -> Checked<String> {
    match value.as_str() {
        Some(text)
            if (1..=max).contains(&text.len())
                && text
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || extra.contains(c)) =>
        {
            Ok(text.to_owned())
        }
        _ => bad(at, format!("must be {what}")),
    }
}

/// An opaque caller id: no whitespace and no `/`, so no URL, path or prose fits.
pub(crate) fn opaque(value: &Value, at: &str) -> Checked<String> {
    charset(value, at, 128, "._:-", "an opaque id")
}

/// The tokenizer a count is in.
pub(crate) fn units(value: &Value, at: &str) -> Checked<String> {
    charset(value, at, 160, "._:@+/-", "a units id")
}

pub(crate) fn ttl_source(value: &Value, at: &str) -> Checked<String> {
    charset(value, at, 96, " ._:()-", "a TTL source")
}

pub(crate) fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub(crate) fn sha256(value: &Value, at: &str) -> Checked<String> {
    match value.as_str() {
        Some(text) if is_sha256(text) => Ok(text.to_owned()),
        _ => bad(at, "must be a sha256 hex digest"),
    }
}

pub(crate) fn array<'a>(value: &'a Value, at: &str, max: usize) -> Checked<&'a Vec<Value>> {
    match value {
        Value::Array(items) if items.len() <= max => Ok(items),
        Value::Array(_) => bad(at, format!("holds at most {max} entries")),
        _ => bad(at, "must be an array"),
    }
}
