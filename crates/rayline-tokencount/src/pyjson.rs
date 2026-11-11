//! Python's `json.dumps`, which chat templates' `tojson` writes: `", "` and
//! `": "` separators (`","` and newlines when indented), no HTML escaping.
//! Keys come out sorted (`serde_json` without `preserve_order`); the count is
//! the same in either order.

use serde_json::Value;

pub(crate) struct Options {
    pub ensure_ascii: bool,
    pub indent: Option<usize>,
    pub sort_keys: bool,
    /// `(item, key)`; Python's default is `(", ", ": ")`, or `(",", ": ")`
    /// when indented.
    pub separators: Option<(String, String)>,
}

pub(crate) fn dumps(value: &Value, options: &Options) -> String {
    let mut out = String::new();
    write(value, options, 0, &mut out);
    out
}

fn write(value: &Value, options: &Options, depth: usize, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => string(text, options.ensure_ascii, out),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                separator(index, options, depth + 1, out);
                write(item, options, depth + 1, out);
            }
            close(options, depth, out);
            out.push(']');
        }
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            let mut entries: Vec<_> = map.iter().collect();
            if options.sort_keys {
                entries.sort_by(|left, right| left.0.cmp(right.0));
            }
            out.push('{');
            for (index, (key, item)) in entries.into_iter().enumerate() {
                separator(index, options, depth + 1, out);
                string(key, options.ensure_ascii, out);
                out.push_str(
                    options
                        .separators
                        .as_ref()
                        .map_or(": ", |(_, key)| key.as_str()),
                );
                write(item, options, depth + 1, out);
            }
            close(options, depth, out);
            out.push('}');
        }
    }
}

fn separator(index: usize, options: &Options, depth: usize, out: &mut String) {
    let item = options.separators.as_ref().map(|(item, _)| item.as_str());
    match options.indent {
        Some(width) => {
            if index > 0 {
                out.push_str(item.unwrap_or(","));
            }
            out.push('\n');
            out.extend(std::iter::repeat_n(' ', width * depth));
        }
        None if index > 0 => out.push_str(item.unwrap_or(", ")),
        None => {}
    }
}

fn close(options: &Options, depth: usize, out: &mut String) {
    if let Some(width) = options.indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', width * depth));
    }
}

fn string(text: &str, ensure_ascii: bool, out: &mut String) {
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
            char if (char as u32) < 0x20 || (ensure_ascii && !char.is_ascii()) => {
                let mut units = [0u16; 2];
                for unit in char.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            char => out.push(char),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compact(value: &Value) -> String {
        dumps(
            value,
            &Options {
                ensure_ascii: false,
                indent: None,
                sort_keys: false,
                separators: None,
            },
        )
    }

    #[test]
    fn writes_what_python_json_dumps_writes() {
        // json.dumps({"a": "x<y> & \"é\"\n", "b": [1, 2.5, None], "e": {}}, ensure_ascii=False)
        let value = json!({"a": "x<y> & \"é\"\n", "b": [1, 2.5, null], "e": {}});
        assert_eq!(
            compact(&value),
            r#"{"a": "x<y> & \"é\"\n", "b": [1, 2.5, null], "e": {}}"#
        );
    }

    #[test]
    fn escapes_non_ascii_only_when_asked_and_indents_like_python() {
        let value = json!({"k": ["é😀", true]});
        let ascii = dumps(
            &value,
            &Options {
                ensure_ascii: true,
                indent: None,
                sort_keys: false,
                separators: None,
            },
        );
        assert_eq!(ascii, r#"{"k": ["\u00e9\ud83d\ude00", true]}"#);
        let indented = dumps(
            &value,
            &Options {
                ensure_ascii: false,
                indent: Some(2),
                sort_keys: false,
                separators: None,
            },
        );
        assert_eq!(indented, "{\n  \"k\": [\n    \"é😀\",\n    true\n  ]\n}");
    }

    #[test]
    fn honours_explicit_separators() {
        let options = Options {
            ensure_ascii: false,
            indent: None,
            sort_keys: false,
            separators: Some((",".into(), ":".into())),
        };
        assert_eq!(
            dumps(&json!({"a": [1, 2], "b": {}}), &options),
            r#"{"a":[1,2],"b":{}}"#
        );
    }
}
