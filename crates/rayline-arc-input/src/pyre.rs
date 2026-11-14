//! Python `re` patterns run on `fancy-regex`.
//!
//! The environment rules are published as Python regular expressions and the
//! encoder was fit on text they rewrote, so they must match the same spans
//! here. The two engines agree on backtracking order; where their character
//! classes differ, [`translate`] spells out Python's:
//!
//! - `\s` also covers the separators U+001C to U+001F;
//! - `\w` is letters, numbers and `_` (no combining marks or other connector
//!   punctuation), and `\b` is a boundary of that class;
//! - outside multi-line mode `$` also matches before a final newline.

use fancy_regex::{Captures, Regex, RegexBuilder};

/// Python's `\w` as a class body.
const WORD: &str = r"\p{L}\p{N}_";
/// What Python's `\s` matches beyond Unicode White_Space, as a class body.
const SPACE: &str = r"\s\x1c-\x1f";

/// Python's `str.isspace` for one character.
pub fn is_space(char: char) -> bool {
    char.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&char)
}

/// Python's `str.strip()`.
pub fn strip(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// The `fancy-regex` pattern matching what the Python pattern `pattern`
/// matches. `dot_all` is `re.DOTALL` given as a compile flag.
pub fn translate(pattern: &str, dot_all: bool) -> String {
    // Every published pattern sets its flags in one leading group.
    let multi_line = pattern
        .strip_prefix("(?")
        .and_then(|rest| rest.split_once(')'))
        .is_some_and(|(flags, _)| {
            flags.chars().all(|c| c.is_ascii_lowercase()) && flags.contains('m')
        });
    let mut out = String::with_capacity(pattern.len() + 16);
    if dot_all {
        out.push_str("(?s)");
    }
    let mut in_class = false;
    let mut chars = pattern.chars().peekable();
    while let Some(char) = chars.next() {
        match char {
            '\\' => match chars.next() {
                Some('s') if in_class => out.push_str(SPACE),
                Some('s') => out.push_str(&format!("[{SPACE}]")),
                Some('S') => out.push_str(&format!("[^{SPACE}]")),
                Some('w') if in_class => out.push_str(WORD),
                Some('w') => out.push_str(&format!("[{WORD}]")),
                Some('b') if !in_class => out.push_str(&format!(
                    "(?:(?<=[{WORD}])(?![{WORD}])|(?<![{WORD}])(?=[{WORD}]))"
                )),
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            },
            '[' if !in_class => {
                in_class = true;
                out.push('[');
                if chars.peek() == Some(&'^') {
                    chars.next();
                    out.push('^');
                }
            }
            ']' if in_class => {
                in_class = false;
                out.push(']');
            }
            '$' if !in_class && !multi_line => out.push_str(r"(?:\z|(?=\n\z))"),
            other => out.push(other),
        }
    }
    out
}

/// Compiles a translated pattern without the engine's default backtracking
/// budget: Python has none, and a lazy scan over a long prompt would spend it.
pub fn compile(pattern: &str) -> Result<Regex, String> {
    RegexBuilder::new(pattern)
        .backtrack_limit(usize::MAX)
        .build()
        .map_err(|error| format!("pattern {pattern:?}: {error}"))
}

/// Compiles the Python pattern `pattern`.
pub fn python(pattern: &str, dot_all: bool) -> Result<Regex, String> {
    compile(&translate(pattern, dot_all))
}

/// `re.sub` with a callable: every non-overlapping match, left to right (or
/// only the first `limit`, when it is not zero), replaced by `replace(match)`.
pub fn sub(
    regex: &Regex,
    text: &str,
    limit: usize,
    mut replace: impl FnMut(&Captures<'_>, &mut String),
) -> Result<String, String> {
    let mut out = String::new();
    let mut cursor = 0;
    for (count, found) in regex.captures_iter(text).enumerate() {
        if limit != 0 && count == limit {
            break;
        }
        let captures = found.map_err(|error| error.to_string())?;
        let Some(whole) = captures.get(0) else {
            continue;
        };
        out.push_str(&text[cursor..whole.start()]);
        replace(&captures, &mut out);
        cursor = whole.end();
    }
    if cursor == 0 {
        return Ok(text.to_owned());
    }
    out.push_str(&text[cursor..]);
    Ok(out)
}

/// One piece of a Python replacement template.
enum Piece {
    Text(String),
    Group(usize),
}

/// A Python `re.sub` replacement template: `\1` to `\9` name a group (empty
/// when it did not take part) and `\n` is a newline.
pub struct Template(Vec<Piece>);

impl Template {
    pub fn new(template: &str) -> Self {
        let mut pieces = Vec::new();
        let mut text = String::new();
        let mut chars = template.chars();
        while let Some(char) = chars.next() {
            if char != '\\' {
                text.push(char);
                continue;
            }
            match chars.next() {
                Some(digit @ '1'..='9') => {
                    if !text.is_empty() {
                        pieces.push(Piece::Text(std::mem::take(&mut text)));
                    }
                    pieces.push(Piece::Group(digit as usize - '0' as usize));
                }
                Some('n') => text.push('\n'),
                Some(other) => text.push(other),
                None => {}
            }
        }
        if !text.is_empty() {
            pieces.push(Piece::Text(text));
        }
        Self(pieces)
    }

    pub fn expand(&self, captures: &Captures<'_>, out: &mut String) {
        for piece in &self.0 {
            match piece {
                Piece::Text(text) => out.push_str(text),
                Piece::Group(index) => {
                    if let Some(group) = captures.get(*index) {
                        out.push_str(group.as_str());
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn replaced(pattern: &str, template: &str, text: &str) -> String {
        let regex = python(pattern, false).unwrap();
        let template = Template::new(template);
        sub(&regex, text, 0, |captures, out| {
            template.expand(captures, out)
        })
        .unwrap()
    }

    #[test]
    fn dollar_matches_before_a_final_newline_only_outside_multi_line() {
        // re.sub(r"b$", "X", "ab\n") == "aX\n"; re.sub(r"b$", "X", "ab\nab") == "ab\naX"
        assert_eq!(replaced(r"b$", "X", "ab\n"), "aX\n");
        assert_eq!(replaced(r"b$", "X", "ab\nab"), "ab\naX");
        assert_eq!(replaced(r"(?m)b$", "X", "ab\nab"), "aX\naX");
    }

    #[test]
    fn word_and_space_classes_are_pythons() {
        // re.sub(r"\w+", "X", "á b‿c ²") == "X́ X‿X X"
        assert_eq!(
            replaced(r"\w+", "X", "a\u{301} b\u{203f}c \u{b2}"),
            "X\u{301} X\u{203f}X X"
        );
        // re.sub(r"\s", "_", "a\x1cb c") == "a_b_c"
        assert_eq!(replaced(r"\s", "_", "a\u{1c}b\u{a0}c"), "a_b_c");
        assert_eq!(replaced(r"[^\s.]+", "X", "ab\u{1f}c.d"), "X\u{1f}X.X");
        // re.sub(r"\bcat\b", "X", "cat concat cat́ cat_") == "X concat X́ cat_"
        assert_eq!(
            replaced(r"\bcat\b", "X", "cat concat cat\u{301} cat_"),
            "X concat X\u{301} cat_"
        );
    }

    #[test]
    fn templates_expand_groups_and_newlines() {
        assert_eq!(replaced(r"(a)(x)?b", r"\2[\1]\n", "ab axb"), "[a]\n x[a]\n");
    }

    #[test]
    fn a_limit_replaces_only_the_first_matches() {
        let regex = python("a", false).unwrap();
        assert_eq!(
            sub(&regex, "aaa", 1, |_, out| out.push('b')).unwrap(),
            "baa"
        );
    }

    #[test]
    fn strip_removes_what_python_strips() {
        assert_eq!(strip("\u{1f}\u{a0} a \u{2028}\n"), "a");
    }
}
