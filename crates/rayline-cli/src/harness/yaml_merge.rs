//! In-place merge of Rayline's keys into an existing YAML config (Hermes'
//! `config.yaml`), keeping every other line as written.
//!
//! Handles block-style YAML as Hermes writes it (PyYAML `safe_dump`: block
//! mappings, sequences level with their key, comments). A line it cannot
//! place (tab indentation, document markers, complex keys, a non-mapping
//! document) is an error, so a file it does not understand is never
//! clobbered.

use serde_json::{Value, json};

/// Set each of `paths` (key paths into `values`) in `existing`: an entry at
/// that path is replaced, a missing one is appended to its parent mapping,
/// and a scalar or empty ancestor is replaced by a mapping. A path with no
/// value in `values` is removed. Every other line is kept as written.
pub(crate) fn merge(existing: &str, values: &Value, paths: &[&[&str]]) -> Result<String, String> {
    let existing = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    let continued = continuations(&lines)?;
    for (index, line) in lines.iter().enumerate() {
        if !continued[index] && keeps_trailing_blanks(line) {
            // Its trailing blank lines are part of its value, which an
            // insertion after it would cut.
            return Err(at(index, "block scalar with `+` chomping"));
        }
    }
    for path in paths {
        let value = path.iter().try_fold(values, |value, key| value.get(*key));
        let end = lines.len();
        set(&mut lines, 0, end, 0, path, value)?;
    }
    let mut out = lines.join("\n");
    out.push('\n');
    Ok(out)
}

/// One `key: ...` entry of a block mapping: `lines[start]` is the key line and
/// `lines[start + 1..end]` its nested value (trailing blank lines and
/// shallower comments excluded).
struct Entry {
    key: String,
    start: usize,
    end: usize,
    /// What follows the `:` on the key line, without a trailing comment.
    inline: String,
}

/// An error at 0-based line `index`. Never quotes the line: it may hold a
/// secret.
fn at(index: usize, what: &str) -> String {
    format!("line {}: {what}", index + 1)
}

fn set(
    lines: &mut Vec<String>,
    start: usize,
    end: usize,
    indent: usize,
    path: &[&str],
    value: Option<&Value>,
) -> Result<(), String> {
    let entries = entries(lines, start, end, indent)?;
    let found = entries.iter().find(|entry| entry.key == path[0]);
    if let Some(entry) = found
        && path.len() > 1
    {
        if let Some(child) = mapping_indent(lines, entry, indent) {
            return set(lines, entry.start + 1, entry.end, child, &path[1..], value);
        }
        if value.is_none() {
            return Ok(());
        }
        if !replaceable_parent(&entry.inline) {
            // A flow mapping or an anchored, aliased or tagged value holds
            // settings this merge cannot keep.
            return Err(at(
                entry.start,
                "unsupported value for a Rayline key's parent",
            ));
        }
    }
    let rendered = value.map_or_else(Vec::new, |value| render(path, value, indent));
    match found {
        Some(entry) => {
            if let Some(index) = (entry.start..entry.end).find(|&index| has_anchor(&lines[index])) {
                // Another entry may alias it.
                return Err(at(index, "YAML anchor in a Rayline-owned entry"));
            }
            lines.splice(entry.start..entry.end, rendered);
        }
        None => {
            let at = entries.last().map_or(end, |entry| entry.end);
            lines.splice(at..at, rendered);
        }
    }
    Ok(())
}

/// The entries of the block mapping at `indent` in `lines[start..end]`.
fn entries(
    lines: &[String],
    start: usize,
    end: usize,
    indent: usize,
) -> Result<Vec<Entry>, String> {
    let continued = continuations(lines)?;
    let mut entries: Vec<Entry> = Vec::new();
    for (index, line) in lines.iter().enumerate().take(end).skip(start) {
        if continued[index] {
            // Part of a quoted scalar, whatever its indentation.
            match entries.last_mut() {
                Some(entry) => entry.end = index + 1,
                None => return Err(at(index, "unexpected indentation")),
            }
            continue;
        }
        if line.starts_with('\t') {
            return Err(at(index, "tab indentation"));
        }
        let text = line.trim_start_matches(' ');
        let depth = line.len() - text.len();
        if text.trim().is_empty() {
            // Spaces past the indent may be block scalar content.
            if depth > indent
                && let Some(entry) = entries.last_mut()
            {
                entry.end = index + 1;
            }
            continue;
        }
        if text.starts_with('#') {
            // A deeper comment may be part of a block scalar; keep it with
            // its entry. A shallower one stays between entries.
            if depth > indent
                && let Some(entry) = entries.last_mut()
            {
                entry.end = index + 1;
            }
            continue;
        }
        if depth > indent || (depth == indent && is_sequence_item(text)) {
            match entries.last_mut() {
                Some(entry) => entry.end = index + 1,
                None => return Err(at(index, "expected a `key:` mapping")),
            }
            continue;
        }
        if depth < indent {
            return Err(at(index, "unexpected indentation"));
        }
        let (key, inline) = parse_key(text).ok_or_else(|| at(index, "expected `key:`"))?;
        if key == "<<" {
            // Inherited keys would survive the merge.
            return Err(at(index, "YAML merge key"));
        }
        if entries.iter().any(|entry| entry.key == key) {
            // YAML loaders keep the last one; a merge into the first would
            // leave it in effect.
            return Err(at(index, "duplicate key"));
        }
        entries.push(Entry {
            key,
            start: index,
            end: index + 1,
            inline,
        });
    }
    Ok(entries)
}

/// For each line, whether it continues a quoted scalar or flow collection
/// opened on an earlier line (at any depth, with any anchor or tag before
/// it).
fn continuations(lines: &[String]) -> Result<Vec<bool>, String> {
    let mut continued = Vec::with_capacity(lines.len());
    let mut scan = Scan::default();
    // The column a block scalar's or a multi-line plain scalar's lines must
    // be indented past, while in that scalar.
    let mut scalar: Option<usize> = None;
    for line in lines {
        let open = scan.quote.is_some() || scan.flow_depth > 0;
        continued.push(open);
        let text = line.trim_start_matches(' ');
        let depth = line.len() - text.len();
        if !open
            && let Some(column) = scalar
            && (text.is_empty() || depth > column)
        {
            continue;
        }
        scalar = match scan.line(line) {
            Tail::Closed => None,
            Tail::BlockScalar => Some(depth),
            Tail::Plain(column) => Some(column.unwrap_or(depth)),
        };
    }
    if scan.quote.is_some() || scan.flow_depth > 0 {
        return Err(at(
            lines.len().saturating_sub(1),
            "unterminated quoted scalar or flow collection",
        ));
    }
    Ok(continued)
}

/// What a quoted scalar or flow collection leaves open across lines.
#[derive(Default)]
struct Scan {
    quote: Option<char>,
    flow_depth: usize,
}

enum Tail {
    Closed,
    /// A block scalar (`|`, `>`) starts on the next line.
    BlockScalar,
    /// The line ends in a plain scalar, which may continue on lines indented
    /// past its key's column (`None`: no key on the line).
    Plain(Option<usize>),
}

impl Scan {
    /// Scan one line from the state the previous one left. A quote, `|`/`>`
    /// or plain text starts a scalar only where a node starts: at the line
    /// start, after `key: `, `- `, an anchor or tag, or in a flow collection;
    /// a quote elsewhere is plain text (`it's`).
    fn line(&mut self, line: &str) -> Tail {
        let mut text = line;
        let mut offset = 0;
        let mut node_start = true;
        if let Some(quote) = self.quote {
            let Some(close) = closing_quote(quote, line) else {
                return Tail::Closed;
            };
            self.quote = None;
            offset = close + 1;
            text = &line[offset..];
            node_start = false;
        }
        let mut plain: Option<usize> = None;
        let mut key_column = None;
        let mut chars = text.char_indices().peekable();
        while let Some((at, ch)) = chars.next() {
            let next = chars.peek().map(|&(_, ch)| ch);
            let ends_token = next.is_none_or(|ch| ch == ' ');
            if node_start {
                match ch {
                    ' ' => {}
                    '"' | '\'' => match closing_quote(ch, &text[at + 1..]) {
                        Some(close) => {
                            let resume = at + 1 + close;
                            while chars.peek().is_some_and(|&(next_at, _)| next_at <= resume) {
                                chars.next();
                            }
                            node_start = false;
                        }
                        None => {
                            self.quote = Some(ch);
                            return Tail::Closed;
                        }
                    },
                    '|' | '>' if self.flow_depth == 0 => return Tail::BlockScalar,
                    '-' | '?' if ends_token => {}
                    '&' | '!' => {
                        while chars.peek().is_some_and(|&(_, ch)| ch != ' ') {
                            chars.next();
                        }
                    }
                    '[' | '{' => self.flow_depth += 1,
                    ']' | '}' if self.flow_depth > 0 => {
                        self.flow_depth -= 1;
                        node_start = false;
                    }
                    '#' => return Tail::Closed,
                    _ => {
                        node_start = false;
                        plain = Some(offset + at);
                    }
                }
                continue;
            }
            match ch {
                ':' if ends_token
                    || (self.flow_depth > 0 && matches!(next, Some(',' | ']' | '}'))) =>
                {
                    node_start = true;
                    if plain.is_some() {
                        key_column = plain.take();
                    }
                }
                '#' if text[..at].ends_with(' ') => return Tail::Closed,
                ',' if self.flow_depth > 0 => {
                    node_start = true;
                    plain = None;
                }
                ']' | '}' if self.flow_depth > 0 => {
                    self.flow_depth -= 1;
                    plain = None;
                }
                _ => {}
            }
        }
        if plain.is_some() && self.flow_depth == 0 {
            Tail::Plain(key_column)
        } else {
            Tail::Closed
        }
    }
}

fn closing_quote(quote: char, text: &str) -> Option<usize> {
    if quote == '"' {
        closing_double_quote(text)
    } else {
        closing_single_quote(text)
    }
}

/// Whether an inline parent value can be replaced by a mapping without losing
/// settings: empty, an empty flow collection, or a plain or quoted scalar.
fn replaceable_parent(inline: &str) -> bool {
    matches!(inline, "{}" | "[]") || !inline.starts_with(['{', '[', '&', '*', '!'])
}

fn has_anchor(line: &str) -> bool {
    line.split(|ch: char| ch.is_whitespace() || matches!(ch, '[' | '{' | ',' | ':'))
        .any(|token| token.len() > 1 && token.starts_with('&'))
}

/// A block scalar header (`|+`, `>2+`, ...) that keeps trailing blank lines.
fn keeps_trailing_blanks(line: &str) -> bool {
    let text = line.trim_start();
    if text.starts_with('#') {
        return false;
    }
    let code = text.split(" #").next().unwrap_or(text).trim_end();
    code.rsplit(' ').next().is_some_and(|token| {
        token.starts_with(['|', '>']) && token.contains('+') && token.len() <= 3
    })
}

/// The indent of `entry`'s nested block mapping, or `None` when its value is
/// anything else (inline, a sequence, empty).
fn mapping_indent(lines: &[String], entry: &Entry, indent: usize) -> Option<usize> {
    if !entry.inline.is_empty() {
        return None;
    }
    let line = lines[entry.start + 1..entry.end].iter().find(|line| {
        let text = line.trim_start();
        !text.is_empty() && !text.starts_with('#')
    })?;
    let text = line.trim_start_matches(' ');
    let depth = line.len() - text.len();
    (depth > indent && !is_sequence_item(text)).then_some(depth)
}

fn is_sequence_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// Split `key: rest` into the key (unquoted) and `rest` without a trailing
/// comment. `None` for anything other than a plain or quoted scalar key.
fn parse_key(text: &str) -> Option<(String, String)> {
    let (key, rest) = if let Some(quoted) = text.strip_prefix('"') {
        let close = closing_double_quote(quoted)?;
        let key = serde_json::from_str(&text[..close + 2]).ok()?;
        (key, &quoted[close + 1..])
    } else if let Some(quoted) = text.strip_prefix('\'') {
        let close = closing_single_quote(quoted)?;
        (quoted[..close].replace("''", "'"), &quoted[close + 1..])
    } else {
        if text.starts_with(['?', '{', '[', '&', '*', '!', '|', '>', '%', '@', '`'])
            || text.starts_with("---")
            || text.starts_with("...")
        {
            return None;
        }
        let colon = text
            .char_indices()
            .find(|&(at, ch)| ch == ':' && text[at + 1..].chars().next().is_none_or(|c| c == ' '))?
            .0;
        (text[..colon].trim_end().to_owned(), &text[colon..])
    };
    let rest = rest.trim_start_matches(' ').strip_prefix(':')?;
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    let rest = rest.trim();
    let inline = if rest.starts_with('#') { "" } else { rest };
    Some((key, inline.to_owned()))
}

fn closing_double_quote(text: &str) -> Option<usize> {
    let mut escaped = false;
    for (at, ch) in text.char_indices() {
        match ch {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return Some(at),
            _ => {}
        }
    }
    None
}

fn closing_single_quote(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'\'' {
            if bytes.get(at + 1) == Some(&b'\'') {
                at += 2;
                continue;
            }
            return Some(at);
        }
        at += 1;
    }
    None
}

/// `path[0]: <path[1..] nested over value>` as YAML lines at `indent`.
fn render(path: &[&str], value: &Value, indent: usize) -> Vec<String> {
    let nested = path
        .iter()
        .rev()
        .fold(value.clone(), |inner, key| json!({ *key: inner }));
    let mut out = String::new();
    super::write_yaml(&nested, indent, &mut out);
    out.lines().map(str::to_owned).collect()
}

#[cfg(test)]
mod tests {
    use super::merge;
    use serde_json::json;

    fn values() -> serde_json::Value {
        json!({
            "model": {"provider": "custom:r", "default": "router"},
            "providers": {"r": {"base_url": "https://x", "key_env": "K"}},
            "fallback_providers": []
        })
    }

    const PATHS: &[&[&str]] = &[
        &["model", "provider"],
        &["model", "default"],
        &["providers", "r"],
        &["fallback_providers"],
    ];

    #[test]
    fn replaces_owned_keys_and_keeps_the_rest_as_written() {
        let existing = "\
# user header
model:
  default: old   # was this
  base_url: https://keep
providers:
  mine:
    base_url: https://mine
  r:
    base_url: https://stale
    note: |
      text
fallback_providers:
- mine
agent:
  disabled_toolsets:
  - web
  - browser

# commented-out sections Hermes appends
# memory:
#   enabled: true
";
        let merged = merge(existing, &values(), PATHS).unwrap();
        assert_eq!(
            merged,
            "\
# user header
model:
  default: \"router\"
  base_url: https://keep
  provider: \"custom:r\"
providers:
  mine:
    base_url: https://mine
  r:
    base_url: \"https://x\"
    key_env: \"K\"
fallback_providers: []
agent:
  disabled_toolsets:
  - web
  - browser

# commented-out sections Hermes appends
# memory:
#   enabled: true
"
        );
        assert_eq!(merge(&merged, &values(), PATHS).unwrap(), merged);
    }

    #[test]
    fn missing_and_non_mapping_parents_are_replaced() {
        let merged = merge("model: some-model\n'providers': {}\n", &values(), PATHS).unwrap();
        assert_eq!(
            merged,
            "\
model:
  provider: \"custom:r\"
  default: \"router\"
providers:
  r:
    base_url: \"https://x\"
    key_env: \"K\"
fallback_providers: []
"
        );
    }

    #[test]
    fn quoted_keys_match() {
        let merged = merge(
            "\"model\" :\n  'default': x\n",
            &values(),
            &[&["model", "default"]],
        );
        assert_eq!(merged.unwrap(), "\"model\" :\n  default: \"router\"\n");
        let merged = merge(
            "\"model\":\n  'default': x\n",
            &values(),
            &[&["model", "default"]],
        )
        .unwrap();
        assert_eq!(merged, "\"model\":\n  default: \"router\"\n");
    }

    #[test]
    fn multi_line_scalars_stay_whole() {
        let existing =
            "\u{feff}model:\n  note: 'hello\n    provider: world'\n  text: |\n    hello\n      \n";
        let merged = merge(existing, &values(), &[&["model", "provider"]]).unwrap();
        assert_eq!(
            merged,
            "model:\n  note: 'hello\n    provider: world'\n  text: |\n    hello\n      \n  provider: \"custom:r\"\n"
        );
    }

    #[test]
    fn dedented_quoted_continuations_stay_with_their_scalar() {
        for existing in [
            "model:\n  notes:\n    prompt: 'hello\n  provider: world'\n",
            "model:\n  note: &n 'hello\n  provider: world'\n",
            "model:\n  note: !!str \"a \\\" b\n  provider: world\"\n",
        ] {
            let merged = merge(existing, &values(), &[&["model", "provider"]]).unwrap();
            assert_eq!(merged, format!("{existing}  provider: \"custom:r\"\n"));
        }
        let block = "model:\n  note: |\n    \"opens no quote\n  provider: old\n";
        let merged = merge(block, &values(), &[&["model", "provider"]]).unwrap();
        assert_eq!(
            merged,
            "model:\n  note: |\n    \"opens no quote\n  provider: \"custom:r\"\n"
        );
        let wrapped = "notes:\n  festive: use the expression\n    'tis the season.\nmodel:\n  provider: old\n  effort: high\nterminal:\n  cwd: /home/o'connor\n";
        let merged = merge(wrapped, &values(), &[&["model", "provider"]]).unwrap();
        assert_eq!(
            merged,
            wrapped.replace("provider: old", "provider: \"custom:r\"")
        );
        let flow = "model:\n  body: &b {t: 0.2,\n  provider: {order: [a]}}\n  effort: high\n";
        let merged = merge(flow, &values(), &[&["model", "provider"]]).unwrap();
        assert_eq!(merged, format!("{flow}  provider: \"custom:r\"\n"));
        let plain = "model:\n  note: it's here\n  provider: old\n";
        let merged = merge(plain, &values(), &[&["model", "provider"]]).unwrap();
        assert_eq!(
            merged,
            "model:\n  note: it's here\n  provider: \"custom:r\"\n"
        );
    }

    #[test]
    fn path_without_a_value_is_removed() {
        let merged = merge(
            "fallback_model:\n  provider: openrouter\nagent: {}\n",
            &values(),
            &[&["fallback_model"], &["model", "gone"]],
        )
        .unwrap();
        assert_eq!(merged, "agent: {}\n");
    }

    #[test]
    fn unsupported_yaml_is_an_error_not_a_rewrite() {
        for text in [
            "fallback_providers: []\nfallback_providers:\n- other\n",
            "model: {provider: old, reasoning_effort: high}\n",
            "model: &m\n  default: x\n",
            "model:\n  default: &saved old\nother: *saved\n",
            "agent:\n  note: |+\n    hello\n\n",
            "  SECRET-VALUE\n",
            "<<: *defaults\nmodel:\n  default: x\n",
            "providers:\n  r: {\"base_url\":&saved \"x\"}\nother: *saved\n",
            "model: {provider: old,\n  reasoning_effort: high}\n",
            "model:\n  note: 'never closed\n",
            "model:\n  notes:\n    prompt: 'hello\n  provider: world\n",
            "model:\n\tdefault: x\n",
            "? complex\n: key\n",
            "- a\n- b\n",
            "  indented: start\n",
            "just a scalar\n",
        ] {
            let error = merge(text, &values(), PATHS).unwrap_err();
            assert!(!error.contains("SECRET"), "{error}");
        }
    }
}
