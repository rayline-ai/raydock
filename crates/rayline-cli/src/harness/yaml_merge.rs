//! In-place merge of Rayline's keys into an existing YAML config (Hermes'
//! `config.yaml`), keeping every other line as written.
//!
//! Accepts what PyYAML `safe_dump` writes for Hermes' settings, plus
//! comments: block mappings; block sequences of scalars or mappings; plain,
//! single- or double-quoted scalars, which may wrap when every continuation
//! line is indented deeper than its key; and the empty flow collections `[]`
//! and `{}`. Then an entry's value is exactly the lines indented past its
//! key. Anything else (non-empty flow collections, block scalars, anchors,
//! aliases, tags, merge keys, document markers, tabs, a scalar starting on
//! the line after its key) is an error, so a file this module cannot split
//! by indentation is never rewritten.
//!
//! Known limits, all refusals rather than rewrites: keys with YAML-only
//! escapes (`"\xE9"`), explicit `?` keys, a quoted `'<<'` key, keys that
//! differ only by quoting (`null` and `'null'`), sequences nested directly in
//! sequences, literal U+2028/U+2029, and the anchors `safe_dump` writes for
//! shared objects. A file that is already invalid YAML may still be rewritten.
//!
//! Comments are never dropped. Replacing an entry keeps the comment on its key
//! line after the new value (on the `key:` line of a mapping), and moves every
//! other comment from inside the old value (comment lines, and comments after
//! a nested key, item or wrapped line) onto its own line right after the key
//! line, at the child indentation. Removing an entry keeps its comments, each
//! on its own line where the entry was.

use serde_json::{Value, json};

/// Set each of `paths` (key paths into `values`) in `existing`: an entry at
/// that path is replaced, a missing one is appended to its parent mapping,
/// and a scalar or empty ancestor is replaced by a mapping. A path with no
/// value in `values` is removed. Every other line is kept as written.
pub(crate) fn merge(existing: &str, values: &Value, paths: &[&[&str]]) -> Result<String, String> {
    let mut lines = checked_lines(existing)?;
    for path in paths {
        let value = path.iter().try_fold(values, |value, key| value.get(*key));
        let end = lines.len();
        set(&mut lines, 0, end, 0, path, value)?;
    }
    let mut out = lines.join("\n");
    if existing.is_empty() || existing.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

fn checked_lines(existing: &str) -> Result<Vec<String>, String> {
    let existing = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let mut lines: Vec<String> = existing.lines().map(str::to_owned).collect();
    // YAML whitespace is space and tab, and its line breaks include `\r`,
    // NEL, LS and PS, which `str::lines` does not split on. Refuse them all so
    // every check below can trust `' '` and `'\n'` alone.
    if let Some(index) = lines.iter().position(|line| line.contains('\t')) {
        return Err(at(index, "tab character"));
    }
    if let Some(index) = lines
        .iter()
        .position(|line| line.contains(['\r', '\u{85}', '\u{2028}', '\u{2029}']))
    {
        return Err(at(index, "line break other than \\n"));
    }
    if let Some(index) = lines
        .iter()
        .position(|line| line.chars().any(char::is_control))
    {
        return Err(at(index, "control character"));
    }
    // `safe_dump({})` writes `{}`: an empty root mapping, which the merge
    // fills like an empty file.
    if let Some(index) = lines.iter().position(|line| is_content(line)) {
        if lines[index].trim_end_matches(' ') == "{}"
            && !lines[index + 1..].iter().any(|line| is_content(line))
        {
            lines.remove(index);
        }
    }
    // Check the whole document, not only the entries the paths touch.
    mapping(&lines, 0, lines.len(), 0, false, &mut Vec::new())?;
    Ok(lines)
}

/// Rewrite scalar selectors in the root `modelRoles` mapping. Only that
/// mapping uses the conservative merge grammar: unrelated settings may use
/// arbitrary block scalars, anchors and flow collections.
pub(super) fn rewrite_model_roles(
    existing: &str,
    mut rewrite: impl FnMut(&str, &str) -> Option<String>,
) -> Result<String, String> {
    let source = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let lines: Vec<String> = source.lines().map(str::to_owned).collect();
    let mut target = None;
    let mut target_end = lines.len();
    let mut flow = 0_usize;
    let mut quote = None;
    let mut escaped = false;
    let mut root_merge = false;
    let mut documents = false;
    for (index, line) in lines.iter().enumerate() {
        let (depth, text) = split(line);
        if quote.is_some() || flow != 0 {
            scan_flow_line(text, &mut flow, &mut quote, &mut escaped);
            continue;
        }
        if depth != 0 || !is_content(text) {
            continue;
        }
        if target.is_some() && target_end == lines.len() {
            target_end = index;
        }
        if text.starts_with("---") || text.starts_with("...") {
            documents = true;
        }
        let Some((key, rest)) = key_parts(text) else {
            continue;
        };
        if key == "modelRoles" && target.replace(index).is_some() {
            return Err(at(index, "duplicate modelRoles key"));
        }
        root_merge |= key == "<<";
        let mut value = value_text(rest);
        // Tags and anchor names can precede an unrelated flow/quoted value.
        while value.starts_with(['&', '!']) {
            value = value
                .split_once(' ')
                .map_or("", |(_, rest)| rest.trim_start_matches(' '));
        }
        if value.starts_with(['[', '{', '"', '\'']) {
            scan_flow_line(value, &mut flow, &mut quote, &mut escaped);
        }
    }
    let Some(start) = target else {
        return Ok(existing.to_owned());
    };
    if root_merge || documents || quote.is_some() || flow != 0 {
        return Err(at(start, "ambiguous root YAML around modelRoles"));
    }
    if lines[start..target_end].iter().any(|line| {
        line.contains(['\t', '\r', '\u{85}', '\u{2028}', '\u{2029}'])
            || line.chars().any(char::is_control)
    }) {
        return Err(at(start, "unsupported whitespace in modelRoles"));
    }
    let entries = mapping(&lines, start, target_end, 0, false, &mut Vec::new())?;
    let roles = &entries[0];
    let Some(indent) = roles.child else {
        let (_, rest) = key_parts(split(&lines[roles.start]).1).unwrap();
        if matches!(
            value_text(rest).split(" #").next().unwrap().trim(),
            "" | "{}" | "null" | "~"
        ) {
            return Ok(existing.to_owned());
        }
        return Err(at(roles.start, "modelRoles must be a block mapping"));
    };
    let roles = mapping(
        &lines,
        roles.start + 1,
        roles.end,
        indent,
        false,
        &mut Vec::new(),
    )?;
    let mut edits = Vec::new();
    for role in roles {
        let (_, text) = split(&lines[role.start]);
        let (_, rest) = key_parts(text).unwrap();
        let inline = value_text(rest);
        if !inline.is_empty() {
            role_scalar(
                &lines,
                role.start,
                role.end,
                inline,
                &role.key,
                &mut rewrite,
                &mut edits,
            )?;
        } else {
            // Model selectors also support ordered fallback arrays.
            for index in role.start + 1..role.end {
                let (_, text) = split(&lines[index]);
                if !is_content(text) {
                    continue;
                }
                if !is_sequence_item(text) {
                    return Err(at(index, "model role must be a scalar or scalar sequence"));
                }
                let inline = text[1..].trim_start_matches(' ');
                role_scalar(
                    &lines,
                    index,
                    index + 1,
                    inline,
                    &role.key,
                    &mut rewrite,
                    &mut edits,
                )?;
            }
        }
    }
    if edits.is_empty() {
        return Ok(existing.to_owned());
    }
    // Keep every byte outside the edited scalar spans, including the BOM.
    let mut offsets = Vec::with_capacity(lines.len());
    let mut offset = usize::from(existing.starts_with('\u{feff}')) * '\u{feff}'.len_utf8();
    // `str::lines` strips CRLF; count original bytes, not parsed line lengths.
    for raw in source.split_inclusive('\n') {
        offsets.push(offset);
        offset += raw.len();
    }
    let mut out = existing.to_owned();
    for (line, start, end, value) in edits.into_iter().rev() {
        out.replace_range(offsets[line] + start..offsets[line] + end, &value);
    }
    Ok(out)
}

/// Track only multiline quoted/flow values while locating root keys. This
/// does not interpret or rewrite those unrelated values.
fn scan_flow_line(text: &str, flow: &mut usize, quote: &mut Option<char>, escaped: &mut bool) {
    let mut chars = text.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(ch) = chars.next() {
        match *quote {
            Some('"') => {
                if *escaped {
                    *escaped = false;
                } else if ch == '\\' {
                    *escaped = true;
                } else if ch == '"' {
                    *quote = None;
                }
            }
            Some('\'') => {
                if ch == '\'' {
                    if chars.peek() == Some(&'\'') {
                        chars.next();
                    } else {
                        *quote = None;
                    }
                }
            }
            _ => match ch {
                '#' if previous.is_none_or(char::is_whitespace) => break,
                '"' | '\'' if previous.is_none_or(|c| c.is_whitespace() || "[{,:".contains(c)) => {
                    *quote = Some(ch);
                }
                '[' | '{' => *flow += 1,
                ']' | '}' => *flow = flow.saturating_sub(1),
                _ => {}
            },
        }
        previous = Some(ch);
    }
}

fn role_scalar(
    lines: &[String],
    index: usize,
    end: usize,
    inline: &str,
    role: &str,
    rewrite: &mut impl FnMut(&str, &str) -> Option<String>,
    edits: &mut Vec<(usize, usize, usize, String)>,
) -> Result<(), String> {
    if (index + 1..end).any(|next| is_content(split(&lines[next]).1)) {
        return Err(at(index, "wrapped model role selector is unsupported"));
    }
    let start = lines[index].trim_end_matches(' ').len() - inline.trim_end_matches(' ').len();
    let (length, decoded, quote) = if let Some(text) = inline.strip_prefix('"') {
        let close =
            closing_double_quote(text).ok_or_else(|| at(index, "unclosed model role selector"))?;
        let length = close + 2;
        let decoded: String = serde_json::from_str(&inline[..length])
            .map_err(|_| at(index, "unsupported model role string escape"))?;
        (length, decoded, Some('"'))
    } else if let Some(text) = inline.strip_prefix('\'') {
        let close =
            closing_single_quote(text).ok_or_else(|| at(index, "unclosed model role selector"))?;
        (close + 2, text[..close].replace("''", "'"), Some('\''))
    } else {
        let text = inline.split(" #").next().unwrap().trim_end_matches(' ');
        if text.starts_with(['[', '{']) || parse_key(text).is_some() {
            return Err(at(index, "model role must be a scalar or scalar sequence"));
        }
        (text.len(), text.to_owned(), None)
    };
    if let Some(value) = rewrite(role, &decoded) {
        let encoded = match quote {
            Some('"') => serde_json::to_string(&value).unwrap(),
            Some('\'') => format!("'{}'", value.replace('\'', "''")),
            _ => value,
        };
        edits.push((index, start, start + length, encoded));
    }
    Ok(())
}

/// One `key: ...` entry of a block mapping: `lines[start]` is the key line and
/// `lines[start + 1..end]` the rest of its value. Comments after the value's
/// last line are not part of it.
struct Entry {
    key: String,
    start: usize,
    end: usize,
    /// The indent of the value's keys when it is a nested block mapping.
    child: Option<usize>,
}

/// A comment: the 0-based line and the byte column of its `#`.
type Note = (usize, usize);

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
    let mut notes = Vec::new();
    let entries = mapping(lines, start, end, indent, false, &mut notes)?;
    let found = entries.iter().find(|entry| entry.key == path[0]);
    if let Some(entry) = found
        && path.len() > 1
    {
        if let Some(child) = entry.child {
            return set(lines, entry.start + 1, entry.end, child, &path[1..], value);
        }
        if value.is_none() {
            return Ok(());
        }
    }
    let mut rendered = value.map_or_else(Vec::new, |value| render(path, value, indent));
    let range = match found {
        Some(entry) => {
            keep_comments(lines, entry, &mut notes, indent, &mut rendered);
            entry.start..entry.end
        }
        None => {
            let at = entries.last().map_or(end, |entry| entry.end);
            at..at
        }
    };
    lines.splice(range, rendered);
    Ok(())
}

/// Carry the comments inside `entry` over to `rendered`, its replacement (or
/// nothing, when it is removed); see the module docs.
fn keep_comments(
    lines: &[String],
    entry: &Entry,
    notes: &mut Vec<Note>,
    indent: usize,
    rendered: &mut Vec<String>,
) {
    notes.sort_unstable();
    notes.dedup();
    let mut inline = None;
    let mut own_lines = Vec::new();
    for &(line, column) in notes.iter() {
        if !(entry.start..entry.end).contains(&line) {
            continue;
        }
        let comment = &lines[line][column..];
        if line == entry.start && !rendered.is_empty() {
            inline = Some(comment);
        } else {
            own_lines.push(comment);
        }
    }
    let pad = " ".repeat(if rendered.is_empty() {
        indent
    } else {
        indent + 2
    });
    let own_lines = own_lines
        .into_iter()
        .map(|comment| format!("{pad}{comment}"));
    match rendered.first_mut() {
        Some(first) => {
            if let Some(comment) = inline {
                first.push(' ');
                first.push_str(comment);
            }
            rendered.splice(1..1, own_lines);
        }
        None => rendered.extend(own_lines),
    }
}

/// Record each line of `lines[from..to]` that is a whole-line comment. Only
/// for lines outside quoted values.
fn comment_lines(lines: &[String], from: usize, to: usize, notes: &mut Vec<Note>) {
    for (index, line) in lines.iter().enumerate().take(to).skip(from) {
        let (depth, text) = split(line);
        if text.starts_with('#') {
            notes.push((index, depth));
        }
    }
}

/// The entries of the block mapping with keys at column `indent` in
/// `lines[start..end]`, checking each value. With `item`, `lines[start]` is a
/// sequence item whose first key follows its `- ` at column `indent`. Records
/// every comment in `lines[start..end]` in `notes`.
fn mapping(
    lines: &[String],
    start: usize,
    end: usize,
    indent: usize,
    item: bool,
    notes: &mut Vec<Note>,
) -> Result<Vec<Entry>, String> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut index = start;
    while index < end {
        let (depth, text) = if item && index == start {
            (indent, &lines[index][indent..])
        } else {
            split(&lines[index])
        };
        if !is_content(text) {
            comment_lines(lines, index, index + 1, notes);
            index += 1;
            continue;
        }
        if depth != indent {
            return Err(at(index, "unexpected indentation"));
        }
        let (key, rest) = key_parts(text).ok_or_else(|| at(index, "expected `key:`"))?;
        let inline = value_text(rest);
        if key == "<<" {
            // Inherited keys would survive the merge.
            return Err(at(index, "YAML merge key"));
        }
        if entries.iter().any(|entry| entry.key == key) {
            // YAML loaders keep the last one; a merge into the first would
            // leave it in effect.
            return Err(at(index, "duplicate key"));
        }
        // The value runs to the next line at the key's column or left of it,
        // except for a sequence written level with its key.
        let nested = inline.is_empty();
        let boundary = (index + 1..end)
            .find(|&next| {
                let (depth, text) = split(&lines[next]);
                is_content(text)
                    && (depth < indent || (depth == indent && !(nested && is_sequence_item(text))))
            })
            .unwrap_or(end);
        let (value_end, child) = if nested {
            if let Some(hash) = rest.find('#') {
                notes.push((index, indent + text.len() - rest.len() + hash));
            }
            nested_value(lines, index, boundary, notes)?
        } else {
            (scalar(lines, index, boundary, indent, inline, notes)?, None)
        };
        comment_lines(lines, value_end, boundary, notes);
        entries.push(Entry {
            key,
            start: index,
            end: value_end,
            child,
        });
        index = boundary;
    }
    Ok(entries)
}

/// Check the value on the lines after the key on `lines[start]`, up to
/// `end`: nothing, a nested block mapping or a block sequence. Returns one
/// past its last line and, for a mapping, the indent of its keys.
fn nested_value(
    lines: &[String],
    start: usize,
    end: usize,
    notes: &mut Vec<Note>,
) -> Result<(usize, Option<usize>), String> {
    let Some(first) = (start + 1..end).find(|&index| is_content(split(&lines[index]).1)) else {
        return Ok((start + 1, None));
    };
    comment_lines(lines, start + 1, first, notes);
    let (depth, text) = split(&lines[first]);
    if is_sequence_item(text) {
        return Ok((sequence(lines, first, end, depth, notes)?, None));
    }
    if parse_key(text).is_none() {
        return Err(at(first, "scalar on the line after its key"));
    }
    let entries = mapping(lines, first, end, depth, false, notes)?;
    let value_end = entries.last().map_or(first + 1, |entry| entry.end);
    Ok((value_end, Some(depth)))
}

/// Check the block sequence with its `- ` at column `indent` in
/// `lines[start..end]`; returns one past its last line.
fn sequence(
    lines: &[String],
    start: usize,
    end: usize,
    indent: usize,
    notes: &mut Vec<Note>,
) -> Result<usize, String> {
    let mut value_end = start + 1;
    let mut index = start;
    while index < end {
        let (depth, text) = split(&lines[index]);
        if !is_content(text) {
            comment_lines(lines, index, index + 1, notes);
            index += 1;
            continue;
        }
        if depth != indent || !is_sequence_item(text) {
            return Err(at(index, "unexpected indentation"));
        }
        let boundary = (index + 1..end)
            .find(|&next| {
                let (depth, text) = split(&lines[next]);
                is_content(text) && depth <= indent
            })
            .unwrap_or(end);
        let rest = text[1..].trim_start_matches(' ');
        if !is_content(rest) || is_sequence_item(rest) {
            return Err(at(index, "empty or nested sequence item"));
        }
        value_end = if parse_key(rest).is_some() {
            let column = indent + text.len() - rest.len();
            let entries = mapping(lines, index, boundary, column, true, notes)?;
            entries.last().map_or(index + 1, |entry| entry.end)
        } else {
            scalar(lines, index, boundary, indent, rest, notes)?
        };
        comment_lines(lines, value_end, boundary, notes);
        index = boundary;
    }
    Ok(value_end)
}

/// Check the scalar `inline` that follows its key (at column `indent`) or
/// `- ` on `lines[start]` and may wrap onto the lines before `end`; returns
/// one past its last line. Records the comments that end its lines.
fn scalar(
    lines: &[String],
    start: usize,
    end: usize,
    indent: usize,
    inline: &str,
    notes: &mut Vec<Note>,
) -> Result<usize, String> {
    // `inline` ends `lines[start]`, give or take trailing spaces.
    let offset = lines[start].trim_end_matches(' ').len() - inline.trim_end_matches(' ').len();
    let quote = match inline.chars().next() {
        Some(quote @ ('"' | '\'')) => quote,
        Some('[' | '{') => {
            let code = inline
                .split(" #")
                .next()
                .unwrap_or(inline)
                .trim_end_matches(' ');
            let wraps = (start + 1..end).any(|index| is_content(split(&lines[index]).1));
            if !matches!(code, "[]" | "{}") || wraps {
                return Err(at(start, "non-empty flow collection"));
            }
            if let Some(space) = inline.find(" #") {
                notes.push((start, offset + space + 1));
            }
            return Ok(start + 1);
        }
        Some('|' | '>') => return Err(at(start, "block scalar")),
        Some('&' | '*' | '!' | '%' | '@' | '`') => {
            return Err(at(start, "anchor, alias, tag or reserved indicator"));
        }
        // Plain: every line before `end` is already deeper than its key.
        _ => {
            if !is_plain_text(inline, true) {
                return Err(at(start, "not a plain scalar"));
            }
            // A comment ends a plain scalar: no content may follow it.
            let mut commented = inline.contains(" #");
            if let Some(space) = inline.find(" #") {
                notes.push((start, offset + space + 1));
            }
            for (index, line) in lines.iter().enumerate().take(end).skip(start + 1) {
                let (depth, text) = split(line);
                if text.starts_with('#') {
                    commented = true;
                } else if is_content(text) {
                    if commented || !is_plain_text(text, false) {
                        return Err(at(index, "not a plain scalar"));
                    }
                    if let Some(space) = text.find(" #") {
                        commented = true;
                        notes.push((index, depth + space + 1));
                    }
                }
            }
            let last = (start + 1..end)
                .rev()
                .find(|&index| is_content(split(&lines[index]).1));
            return Ok(last.unwrap_or(start) + 1);
        }
    };
    // Quoted: it runs to its closing quote, and a `#` line before that is
    // part of it.
    let mut text = inline[1..].to_owned();
    for line in &lines[start + 1..end] {
        text.push('\n');
        text.push_str(line);
    }
    let close = if quote == '"' {
        closing_double_quote(&text)
    } else {
        closing_single_quote(&text)
    }
    .ok_or_else(|| {
        at(
            start,
            "quoted value not closed on a line deeper than its key",
        )
    })?;
    let close_line = start + text[..close].matches('\n').count();
    if let Some(index) = (start + 1..=close_line).find(|&index| {
        let (depth, text) = split(&lines[index]);
        !text.trim_matches(' ').is_empty() && depth <= indent
    }) {
        return Err(at(index, "continuation line not deeper than its key"));
    }
    let after = text[close + 1..].split('\n').next().unwrap_or("");
    let after_text = after.trim_matches(' ');
    if !(after_text.is_empty() || after.starts_with(' ') && after_text.starts_with('#')) {
        return Err(at(close_line, "text after a quoted value"));
    }
    if let Some(hash) = after.find('#') {
        // The closing quote's column on its own line.
        let column = match text[..close].rfind('\n') {
            Some(newline) => close - newline - 1,
            None => offset + 1 + close,
        };
        notes.push((close_line, column + 1 + hash));
    }
    if let Some(index) = (close_line + 1..end).find(|&index| is_content(split(&lines[index]).1)) {
        return Err(at(index, "unexpected indentation"));
    }
    Ok(close_line + 1)
}

/// Whether `text` (one line, comment allowed) can be plain-scalar text in a
/// block mapping: no `: ` or trailing `:` that would make it a mapping, and on
/// the `first` line no leading `- `, `? ` or `: ` indicator and no leading
/// `,`, `]` or `}`. A wrapped continuation line may start with those
/// (`safe_dump` wraps plain text at any space). Anything else is refused
/// rather than guessed at.
fn is_plain_text(text: &str, first: bool) -> bool {
    let code = text
        .split(" #")
        .next()
        .unwrap_or(text)
        .trim_end_matches(' ');
    let indicator = |c: char| {
        code.strip_prefix(c)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
    };
    let bad_start = first
        && (indicator('-')
            || indicator('?')
            || indicator(':')
            || code.starts_with([',', ']', '}']));
    !(bad_start || code.contains(": ") || code.ends_with(':'))
}

/// A line's indentation and the text after it.
fn split(line: &str) -> (usize, &str) {
    let text = line.trim_start_matches(' ');
    (line.len() - text.len(), text)
}

/// Not blank and not a comment. (Inside a quoted scalar a `#` line is
/// content; [`scalar`] handles that.)
fn is_content(text: &str) -> bool {
    let text = text.trim_matches(' ');
    !text.is_empty() && !text.starts_with('#')
}

fn is_sequence_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

/// Split `key: rest` into the key (unquoted) and `rest` without a trailing
/// comment. `None` for anything other than a plain or quoted scalar key.
fn parse_key(text: &str) -> Option<(String, String)> {
    let (key, rest) = key_parts(text)?;
    Some((key, value_text(rest).to_owned()))
}

/// The value in `rest`, the text after a key's `:`: empty when there is none
/// or only a comment.
fn value_text(rest: &str) -> &str {
    let rest = rest.trim_matches(' ');
    if rest.starts_with('#') { "" } else { rest }
}

/// Split `key: rest` into the key (unquoted) and everything after its `:`.
fn key_parts(text: &str) -> Option<(String, &str)> {
    let (key, rest) = if let Some(quoted) = text.strip_prefix('"') {
        let close = closing_double_quote(quoted)?;
        let key = serde_json::from_str(&text[..close + 2]).ok()?;
        (key, &quoted[close + 1..])
    } else if let Some(quoted) = text.strip_prefix('\'') {
        let close = closing_single_quote(quoted)?;
        (quoted[..close].replace("''", "'"), &quoted[close + 1..])
    } else {
        if text.starts_with(['?', '{', '[', '&', '*', '!', '|', '>', '%', '@', '`'])
            || is_sequence_item(text)
            || text.starts_with("---")
            || text.starts_with("...")
        {
            return None;
        }
        let colon = text
            .char_indices()
            .find(|&(at, ch)| ch == ':' && text[at + 1..].chars().next().is_none_or(|c| c == ' '))?
            .0;
        (
            text[..colon].trim_end_matches(' ').to_owned(),
            &text[colon..],
        )
    };
    let rest = rest.trim_start_matches(' ').strip_prefix(':')?;
    if !(rest.is_empty() || rest.starts_with(' ')) {
        return None;
    }
    Some((key, rest))
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

    fn set_provider(existing: &str) -> Result<String, String> {
        merge(existing, &values(), &[&["model", "provider"]])
    }

    #[test]
    fn wrapped_plain_lines_may_start_with_indicators() {
        // `safe_dump` wraps plain text at any space, so a continuation line
        // can start with `- `, `? ` or `,`.
        let text = "model:\n  provider: old\nnote: xxxxxxxx\n  - rest ? more\n  , tail\n";
        assert_eq!(
            set_provider(text).unwrap(),
            "model:\n  provider: \"custom:r\"\nnote: xxxxxxxx\n  - rest ? more\n  , tail\n"
        );
    }

    #[test]
    fn only_spaces_are_whitespace_in_keys_and_comments() {
        // A no-break space is part of the key in YAML: `provider\u{a0}` is a
        // different key and stays, next to the appended `provider`.
        assert_eq!(
            set_provider("model:\n  provider\u{a0}: keep\n").unwrap(),
            "model:\n  provider\u{a0}: keep\n  provider: \"custom:r\"\n"
        );
        // `\u{a0}#keep` is a key, not a comment, so replacing the entry above
        // it keeps its block.
        assert_eq!(
            set_provider(
                "model:\n  provider: old\n  \u{a0}#keep:\n    enabled: true\n  effort: high\n"
            )
            .unwrap(),
            "model:\n  provider: \"custom:r\"\n  \u{a0}#keep:\n    enabled: true\n  effort: high\n"
        );
    }

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
    note: 'text
      # not a comment'
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
  default: \"router\" # was this
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

    /// PyYAML 6 `safe_dump(sort_keys=False)` output, wrapped strings and a
    /// list of mappings included.
    #[test]
    fn merges_a_safe_dump_config() {
        let existing = "\
model:
  default: anthropic/claude-opus-4
  provider: openrouter
  base_url: https://openrouter.ai/api/v1
agent:
  max_turns: 60
  system_prompt: You are a careful assistant. Always explain your reasoning before
    acting, and never run destructive commands without asking the user first.
  personalities:
    concise: 'Be brief: answer in one or two sentences unless asked for more. Don''t
      pad.'
  disabled_toolsets:
  - web
  - browser
providers:
  mine:
    base_url: https://mine.example/v1
    key_env: MINE_KEY
custom_providers:
- name: local
  base_url: http://localhost:8080/v1
  note: A long description of this local provider that PyYAML will wrap across several
    lines because it is long.
- name: other
  api_key: ''
fallback_providers: []
toolsets: {}
terminal:
  cwd: .
  env_passthrough: []
  timeout: 180
display:
  banner: 'line one

    line two: with colon'
  greeting: \"Hello\\tworld, this is a deliberately long string with a tab so that PyYAML\\
    \\ picks double quotes and then wraps it.\"
  skin: default
memory:
  enabled: true
  user_profile: null
";
        let merged = merge(existing, &values(), PATHS).unwrap();
        let expected = existing
            .replace(
                "  default: anthropic/claude-opus-4\n  provider: openrouter\n",
                "  default: \"router\"\n  provider: \"custom:r\"\n",
            )
            .replace(
                "    key_env: MINE_KEY\n",
                "    key_env: MINE_KEY\n  r:\n    base_url: \"https://x\"\n    key_env: \"K\"\n",
            );
        assert_eq!(merged, expected);
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
    fn wrapped_scalars_stay_whole() {
        let appended = "\u{feff}model:\n  note: 'hello\n    provider: world'\n";
        assert_eq!(
            set_provider(appended).unwrap(),
            "model:\n  note: 'hello\n    provider: world'\n  provider: \"custom:r\"\n"
        );
        for existing in [
            "notes:\n  festive: use the expression\n    'tis the season.\nmodel:\n  provider: old\n  effort: high\nterminal:\n  cwd: /home/o'connor\n",
            "model:\n  note: it's here\n  provider: old\n",
            "model:\n  note: \"a \\\" b\\\n    \\ provider: c\"\n  provider: old\n",
            // Finding 3, quoted-key variant: the item's keys are at column 4.
            "model:\n  items:\n  - \"a\": b\n    c: 'x\n      provider: y'\n  - z\n  provider: old\n",
        ] {
            assert_eq!(
                set_provider(existing).unwrap(),
                existing.replace("provider: old", "provider: \"custom:r\"")
            );
        }
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

    /// Finding 4: a missing final newline stays missing.
    #[test]
    fn final_newline_is_kept_as_written() {
        assert_eq!(
            set_provider("model:\n  provider: old\nnote: hi").unwrap(),
            "model:\n  provider: \"custom:r\"\nnote: hi"
        );
        assert_eq!(
            set_provider("note: hi").unwrap(),
            "note: hi\nmodel:\n  provider: \"custom:r\""
        );
        assert_eq!(
            set_provider("").unwrap(),
            "model:\n  provider: \"custom:r\"\n"
        );
        // `safe_dump({})` writes `{}`.
        assert_eq!(
            set_provider("{}\n").unwrap(),
            "model:\n  provider: \"custom:r\"\n"
        );
    }

    /// Finding 5: a comment after a value is not part of it.
    #[test]
    fn comments_after_a_replaced_value_are_kept() {
        assert_eq!(
            set_provider("model:\n  provider: old\n    # about effort\n  effort: high\n").unwrap(),
            "model:\n  provider: \"custom:r\"\n    # about effort\n  effort: high\n"
        );
        assert_eq!(
            merge(
                "providers:\n  r:\n    base_url: x\n      # keep\n  mine: {}\n",
                &values(),
                &[&["providers", "r"]],
            )
            .unwrap(),
            "providers:\n  r:\n    base_url: \"https://x\"\n    key_env: \"K\"\n      # keep\n  mine: {}\n"
        );
    }

    #[test]
    fn inline_comment_on_a_replaced_scalar_is_kept() {
        for (existing, expected) in [
            (
                "model:\n  provider: old # keep this explanation\n",
                "model:\n  provider: \"custom:r\" # keep this explanation\n",
            ),
            (
                "model:\n  provider: 'o''ld'   #  spaced # twice\n",
                "model:\n  provider: \"custom:r\" #  spaced # twice\n",
            ),
            (
                "model:\n  provider: \"a #b\" # c\n",
                "model:\n  provider: \"custom:r\" # c\n",
            ),
            (
                "model:\n  provider: [] # none yet\n",
                "model:\n  provider: \"custom:r\" # none yet\n",
            ),
            // Not comments: `#` without a space before it, or inside quotes.
            (
                "model:\n  provider: a#b\n",
                "model:\n  provider: \"custom:r\"\n",
            ),
            (
                "model:\n  provider: 'x # y'\n",
                "model:\n  provider: \"custom:r\"\n",
            ),
            // A wrapped value's comments get their own lines.
            (
                "model:\n  provider: a long\n    value # end\n",
                "model:\n  provider: \"custom:r\"\n    # end\n",
            ),
            (
                "model:\n  provider: 'a long\n    # inside\n    value' # end\n",
                "model:\n  provider: \"custom:r\"\n    # end\n",
            ),
        ] {
            let merged = set_provider(existing).unwrap();
            assert_eq!(merged, expected, "{existing}");
            assert_eq!(set_provider(&merged).unwrap(), merged);
        }
    }

    #[test]
    fn inline_comment_on_a_replaced_mapping_key_is_kept() {
        let merged = merge(
            "providers:\n  r: # Rayline's entry\n    base_url: x\n",
            &values(),
            &[&["providers", "r"]],
        )
        .unwrap();
        assert_eq!(
            merged,
            "providers:\n  r: # Rayline's entry\n    base_url: \"https://x\"\n    key_env: \"K\"\n"
        );
        // A scalar replaced by a mapping keeps its comment on the `key:` line.
        assert_eq!(
            set_provider("model: some-model # pick one\n").unwrap(),
            "model: # pick one\n  provider: \"custom:r\"\n"
        );
    }

    #[test]
    fn comments_inside_a_replaced_mapping_are_kept() {
        let existing = "\
providers:
  r:
    # why this router
    base_url: x # old url

    # about the key
    key_env: OLD
    list: # items
    # first
    - a # one
    - 'b
      # not a comment'
  mine: {}
";
        let merged = merge(existing, &values(), &[&["providers", "r"]]).unwrap();
        assert_eq!(
            merged,
            "\
providers:
  r:
    # why this router
    # old url
    # about the key
    # items
    # first
    # one
    base_url: \"https://x\"
    key_env: \"K\"
  mine: {}
"
        );
        assert_eq!(
            merge(&merged, &values(), &[&["providers", "r"]]).unwrap(),
            merged
        );
    }

    /// A removed entry's comments stay, each on its own line where the entry
    /// was: comments never change what the file means, so none is dropped.
    #[test]
    fn comments_of_a_removed_entry_are_kept() {
        let merged = merge(
            "\
fallback_model: # old fallback
  # use openrouter
  provider: openrouter # cheap
agent: {}
",
            &values(),
            &[&["fallback_model"]],
        )
        .unwrap();
        assert_eq!(
            merged,
            "# old fallback\n# use openrouter\n# cheap\nagent: {}\n"
        );
        assert_eq!(
            merge(&merged, &values(), &[&["fallback_model"]]).unwrap(),
            merged
        );
    }

    #[test]
    fn unsupported_yaml_is_an_error_not_a_rewrite() {
        for text in [
            // Round-5 findings 1-4.
            "model:\n  provider:\n    hello\n    'tis\n  effort: high # '\n",
            "model:\n  body: {\"x\":\"}\",\n  provider: old}\n  effort: high\n",
            "model:\n  items:\n  - a: |2\n      hi\n    body: [x,\n  provider: old]\n  effort: high\n",
            "model:\n  items:\n  - \"a\": b\n    body: [x,\n  provider: old]\n  effort: high\n",
            "model:\n  provider: old\nnote: |\n  hi",
            // Block scalars.
            "model:\n  note: |\n    hello\n",
            "model:\n  note: >-\n    hello\n",
            "agent:\n  note: |+\n    hello\n\n",
            // Quoted continuations not deeper than their key.
            "model:\n  notes:\n    prompt: 'hello\n  provider: world'\n",
            "model:\n  note: 'a\n# b'\n  provider: c\n",
            "model:\n  note: 'never closed\n",
            "list:\n- 'a\n- b'\n",
            // Anchors, aliases, tags, merge keys.
            "model: &m\n  default: x\n",
            "model:\n  default: &saved old\nother: *saved\n",
            "model:\n  note: &n 'hello'\n",
            "model:\n  note: !!str x\n",
            "list:\n- *a\n",
            "<<: *defaults\nmodel:\n  default: x\n",
            // Non-empty flow collections.
            "model: {provider: old, reasoning_effort: high}\n",
            "model: {provider: old,\n  reasoning_effort: high}\n",
            "providers:\n  r: {\"base_url\":&saved \"x\"}\nother: *saved\n",
            "list: [a]\n",
            "list:\n- {a: 1}\n",
            "list: []\n  - a\n",
            // Document markers, directives, tabs.
            "---\nmodel: x\n",
            "model: x\n...\n",
            "%YAML 1.2\n---\nmodel: x\n",
            "model:\n\tdefault: x\n",
            // Values that are not plain scalars (already invalid YAML).
            "model:\n  provider: old\nnote: - foo\n",
            "model: - foo\nnote: keep\n",
            "model:\n  provider: old\nnote: a: b\n",
            "model:\n  provider: old\nnote: wrapped\n    and: more\n",
            "model:\n  provider: old\nnote: ? x\n",
            "model: ]\n",
            "model: }\n",
            "model: old # comment\n  more\n",
            "model: old\n  # comment\n  more\n",
            "model: old\n  more # comment\n\n  still more\n",
            "model: old\u{0}\n",
            "model: old\u{7f}\n",
            // Line breaks `str::lines` does not split on.
            "note: x\rmodel:\r  effort: high\r",
            "note: x\u{85}model: y\n",
            "note: x\u{2028}model: y\n",
            "note: x\u{2029}model: y\n",
            // Other shapes `safe_dump` does not write.
            "fallback_providers: []\nfallback_providers:\n- other\n",
            "? complex\n: key\n",
            "- a\n- b\n",
            "list:\n- - a\n",
            "list:\n-\n  a: 1\n",
            "model:\n  provider: 'a' b\n",
            "model: 'a'\n  b\n",
            "  SECRET-VALUE\n",
            "  indented: start\n",
            "just a scalar\n",
        ] {
            let error = merge(text, &values(), PATHS).unwrap_err();
            assert!(!error.contains("SECRET"), "{error}");
        }
    }
}
