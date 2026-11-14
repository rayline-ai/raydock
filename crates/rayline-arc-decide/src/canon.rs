//! The content digest release ids are made with: SHA-256 of a value's JSON
//! with sorted keys, no spaces and every non-ASCII character escaped.

use serde_json::Value;
use sha2::{Digest, Sha256};

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_string(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write(item, out);
            }
            out.push(']');
        }
        Value::Object(members) => {
            let mut keys: Vec<&String> = members.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write(&members[key], out);
            }
            out.push('}');
        }
    }
}

/// The canonical JSON text of `value`. Ids are digests of strings, integers,
/// nulls and containers of them; a float is written as `serde_json` writes
/// it, which is not checked against the reference.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The content digest of `value`.
pub fn digest(value: &Value) -> String {
    sha256_hex(canonical_json(value).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_sort_and_non_ascii_escapes() {
        let value = json!({"b": [1, null, true], "a": "é\u{1F600}\n\u{7f}"});
        assert_eq!(
            canonical_json(&value),
            "{\"a\":\"\\u00e9\\ud83d\\ude00\\n\\u007f\",\"b\":[1,null,true]}"
        );
    }

    #[test]
    fn digest_is_sha256_of_the_canonical_text() {
        assert_eq!(digest(&json!({})), sha256_hex(b"{}"));
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
