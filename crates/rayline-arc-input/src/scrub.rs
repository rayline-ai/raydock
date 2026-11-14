//! The `scrub_v2` environment ruleset.
//!
//! Harness text states facts about the machine a request ran on: the working
//! directory, git state, platform, shell, date, per-run paths and the model
//! serving the session. The encoder was fit with each such value replaced by
//! a fixed placeholder, so the same rewrite is applied before tokenizing. It
//! covers system text, harness sections inside user text, a harness time
//! stamp opening a user text block and every string of the tool definitions;
//! other user text, assistant text and tool results are left alone.
//!
//! The patterns are the ruleset's published Python expressions, verbatim;
//! `pyre` runs them with Python's matching rules.

use std::sync::LazyLock;

use fancy_regex::Regex;

use crate::ProjectionError;
use crate::canonical::Message;
use crate::json::{Json, Object};
use crate::pyre::{self, Template};

/// Patterns whose first group names the working directory. Its value is then
/// also replaced wherever it recurs in scope.
const CWD: &[&str] = &[
    r"(?im)^[ \t]*(?:[-*]|\d+\.)?[ \t]*(?:Primary |Current )?working directory(?: is)?:[ \t]*(\S[^\n]*?)[ \t]*$",
    r#"(?i)your current working directory is:?[ \t]+(/[^\s`'\"]*)"#,
    r#"(?i)workspace directories:\**[ \t]*\n[ \t]*-[ \t]*(/[^\s`'\"]*)"#,
    r#"(?i)following folders:[ \t]*\n[ \t]*-[ \t]*(/[^\s`'\"]*)"#,
    r#"(?m)^(/[^\s`'\"]+) -> \S+$"#,
    r#"(?i)(?:base|workspace|project) directory(?: is)?:?[ \t]*\(?'?(/[^\s'`\")]+)"#,
    r#"(?i)operating from '(/[^\s'`\"]+)'"#,
    r"(?im)^[ \t]*[-*]?[ \t]*Workspace root folder:[ \t]*(\S[^\n]*?)[ \t]*$",
    r"<cwd>\s*(.*?)\s*</cwd>",
    r"<working-directory>\s*(.*?)\s*</working-directory>",
    r#"\"?workspace_path\"?\s*[:=]\s*\"?([^\"\n]+?)\"?\s*$"#,
];

/// The most distinct working directories one conversation may name.
const MAX_DIRECTORIES: usize = 64;

const MONTH: &str = "(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)";

/// `(pattern, replacement)` pairs applied in order to in-scope text.
const RULES: &[(&str, &str)] = &[
    // Model identity.
    (
        r"(You are powered by the model named )[^.\n]+(?:\.[^\s.][^.\n]*)*(\. The exact model ID is )[^\s\n]+?(?=\.?(?:\s|$))",
        r"\1<MODEL>\2<MODEL>",
    ),
    (
        r"(You are powered by the model )[^\n]+?(?=\.(?:\s|$))",
        r"\1<MODEL>",
    ),
    (
        r"(Assistant knowledge cutoff is )[^\n]+?(?=\.(?:\s|$))",
        r"\1<MODEL_CUTOFF>",
    ),
    (
        r"(?im)^([ \t]*[-*]?[ \t]*(?:model|model id|model name)[ \t]*:[ \t]*)\S[^\n]*$",
        r"\1<MODEL>",
    ),
    (r"<model>.*?</model>", r"<model><MODEL></model>"),
    (
        concat!(
            r"\b(?:claude-(?!code\b)[a-z0-9]+(?:[.-][a-z0-9]+)*|gpt-\d[a-z0-9.-]*|o[134]-[a-z0-9.-]+|gemini-\d[a-z0-9.-]*",
            r"|glm-\d[a-z0-9.-]*|kimi-k\d[a-z0-9.-]*|qwen\d[a-z0-9.-]*|deepseek-[a-z0-9.-]+|mimo-[a-z0-9.-]+",
            r"|grok-\d[a-z0-9.-]*|minimax-[a-z0-9.-]+)\b"
        ),
        "<MODEL>",
    ),
    // Git.
    (
        r"(Is a git repository:[ \t]*)(?:true|false)",
        r"\1<GIT_REPO>",
    ),
    (
        r"(?i)(Is directory a git repo:[ \t]*)(?:yes|no)",
        r"\1<GIT_REPO>",
    ),
    (
        concat!(
            r"gitStatus: This is the git status[^\n]*(?:\n\n?(?:Current branch|Main branch|Git user|Status|",
            r"Recent commits|\(clean\)|[0-9a-f]{7,}|[ ?MADRCU]{1,2} )[^\n]*)*"
        ),
        "gitStatus: <GIT_STATUS>",
    ),
    (
        r"(?im)^([ \t]*[-*]?[ \t]*(?:Current branch|Git branch|Branch|Main branch[^:\n]*)[ \t]*:[ \t]*)\S[^\n]*$",
        r"\1<GIT_STATUS>",
    ),
    (
        r"(?m)^(Status:\n)(?:\(clean\)|(?:[ ?MADRCU!]{1,2} [^\n]+\n?)+)",
        r"\1<GIT_STATUS>",
    ),
    (
        r"(?m)^(Recent commits:\n)(?:[0-9a-f]{7,40} [^\n]*(?:\n|$))+",
        r"\1<GIT_STATUS>\n",
    ),
    (
        r"(?im)^([ \t]*[-*]?[ \t]*(?:Git status|Status)[ \t]*:[ \t]*)\S[^\n]*$",
        r"\1<GIT_STATUS>",
    ),
    // Date and time.
    (
        r"(Today's date is(?: now)? )[^\n]+?(?=\.(?:\s|$)|\n|$)",
        r"\1<DATE>",
    ),
    (r"(Current date is:?[ \t]*)[^\n<]+", r"\1<DATE>"),
    (
        concat!(
            r"(?im)^([ \t]*[-*]?[ \t]*(?:current )?(?:date|time|date and time|datetime|local time|time ?zone)",
            r"[ \t]*:[ \t]*)\S[^\n]*$"
        ),
        r"\1<DATE>",
    ),
    (
        r"<current-time>.*?</current-time>",
        r"<current-time><DATE></current-time>",
    ),
    (
        r"\b\d{4}-\d{2}-\d{2}(?:[T ]\d{2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?:[ ]?(?:Z|UTC|[+-]\d{2}:?\d{2}))?)?\b",
        "<DATE>",
    ),
    (
        r"\b\d{4}-(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*-\d{1,2}\b",
        "<DATE>",
    ),
    (
        concat!(
            r"\b(?:(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun)[a-z]*,? )?",
            r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)[a-z]*\.? \d{1,2},? \d{4}\b"
        ),
        "<DATE>",
    ),
    (r"\b\d{1,2}/\d{1,2}/\d{4}\b", "<DATE>"),
    (
        r"\[(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun) <DATE>[^\]]*\]",
        "[<DATE>]",
    ),
    (
        r"(?im)^([ \t]*Day of (?:the )?Week:[ \t]*)\S+[ \t]*$",
        r"\1<DATE>",
    ),
    // Platform, OS, shell, host.
    (
        concat!(
            r"(?im)^([ \t]*[-*]?[ \t]*(?:platform|os version|os|operating system|runtime|kernel|architecture|arch)",
            r"[ \t]*:[ \t]*)\S[^\n]*$"
        ),
        r"\1<PLATFORM>",
    ),
    (
        r"(?im)^([ \t]*[-*]?[ \t]*(?:shell|default shell)[ \t]*:[ \t]*)\S[^\n]*$",
        r"\1<SHELL>",
    ),
    (
        r"(?im)^([ \t]*[-*]?[ \t]*(?:hostname|host)[ \t]*:[ \t]*)\S[^\n]*$",
        r"\1<HOST>",
    ),
    (
        r"\b(?:Linux|Darwin) \d+\.\d+(?:\.\d+)?[^\s,);]*",
        "<PLATFORM>",
    ),
    (
        r"\b(?:UNAME_VERSION|x86_64|amd64|arm64|aarch64)\b",
        "<PLATFORM>",
    ),
    (
        r"(?i)((?:My )?operating system(?: is)?:?[ \t]*)(?:linux|darwin|macos|windows|win32)\b",
        r"\1<PLATFORM>",
    ),
    (
        r"(?i)(\bShell:[ \t]*)(?:/bin/)?(?:ba|z|fi|da|k)?sh\b",
        r"\1<SHELL>",
    ),
    // Per-run paths and ids.
    (
        r"(?i)((?:system |project's )?temp(?:orary)? directory(?: is)?:[ \t]*)[^\s<]+",
        r"\1<PATH>",
    ),
    (r"\S*/projects/[^/\s]+/memory/?", "<MEMORY_DIR>"),
    (r"\S*/\.nvm/versions/\S*|\S*/node_modules/\S*", "<PATH>"),
    (r#"(?<![\w.~-])/(?:home|Users)/[^/\s`'\")\]]+"#, "<HOME>"),
    (r"(?<![\w./~-])/root(?=/|\b)", "<HOME>"),
    (
        r"\bT-[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\b",
        "<ID>",
    ),
    // The OS and shell lines some harness wrappers carry.
    (
        r"(The user's current OS is:?[ \t]*)\S[^\n]*",
        r"\1<PLATFORM>",
    ),
    (
        r"(The user's default shell is:?[ \t]*)\x22[^\x22\n]*\x22",
        r"\1<SHELL>",
    ),
    (r"<shell>[^<\n]*</shell>", r"<shell><SHELL></shell>"),
    (
        r"(?m)^(Terminal:[ \t]*)(?:/bin/)?(?:(?:ba|z|fi|da|k)?sh|pwsh|powershell|cmd)\b",
        r"\1<SHELL>",
    ),
];

/// Harness sections inside user text: a block in one of these tags, which
/// only harnesses inject, from `<tag>` to the first `</tag>` after it (the
/// reference's `<(tag|...)>.*?</\1>`, dot matching newline).
const SECTION_TAGS: [&str; 11] = [
    "system-reminder",
    "system_information",
    "environment_details",
    "environment_context",
    "environment_info",
    "session_context",
    "turn-context",
    "workspace_info",
    "user_information",
    "current-time",
    "working-directory",
];

/// The other harness section: a leading `<context>` block opening with a
/// date line. It is tried only at the start, after a tag section.
const CONTEXT_SECTION: &str = r"\A\s*<context>[ \t]*\nThe current date is [^\n]*\n.*?</context>";

/// A harness time stamp opening a user text block, such as
/// `[Thu 2026-01-01 00:00 UTC] `: a weekday, then a date and/or a clock time
/// and an optional zone.
fn turn_stamp() -> String {
    let date = format!(
        r"(?:\d{{4}}-\d{{2}}-\d{{2}}|\d{{1,2}}/\d{{1,2}}/\d{{4}}|{MONTH}[a-z]*\.? \d{{1,2}},? \d{{4}}|\d{{1,2}} {MONTH}[a-z]*,? \d{{4}})"
    );
    let time = r"\d{1,2}:\d{2}(?::\d{2}(?:\.\d+)?)?(?: ?[AaPp]\.?[Mm]\.?)?";
    let zone = concat!(
        r"(?: ?(?:Z|UTC|GMT|[A-Z]{1,4}T)(?:[+-]\d{1,2}(?::?\d{2})?)?| ?[+-]\d{2}:?\d{2}",
        r"| [A-Z][A-Za-z_]+(?:/[A-Za-z_+-]+)+)?"
    );
    format!(
        r"^\[(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun)(?:day|sday|nesday|rsday|urday|s|r|rs)?,? (?:{date}(?:(?:,? | at |T){time})?|{time}){zone}\][ \t]*"
    )
}

struct Ruleset {
    cwd: Vec<Regex>,
    rules: Vec<(Regex, Template)>,
    context_section: Regex,
    turn_stamp: Regex,
}

static RULESET: LazyLock<Result<Ruleset, String>> = LazyLock::new(|| {
    Ok(Ruleset {
        cwd: CWD
            .iter()
            .map(|pattern| pyre::python(pattern, false))
            .collect::<Result<_, _>>()?,
        rules: RULES
            .iter()
            .map(|(pattern, template)| Ok((pyre::python(pattern, false)?, Template::new(template))))
            .collect::<Result<_, String>>()?,
        context_section: pyre::python(CONTEXT_SECTION, true)?,
        turn_stamp: pyre::python(&turn_stamp(), false)?,
    })
});

impl Ruleset {
    /// The harness sections of `text` as byte ranges, left to right and not
    /// overlapping: what the reference's section pattern matches, found in
    /// linear time. A tag with no closing tag after one position has none
    /// after any later one, so each tag's closing tag is searched for past a
    /// given point at most once.
    fn sections(&self, text: &str) -> Result<Vec<std::ops::Range<usize>>, ProjectionError> {
        let mut found = Vec::new();
        // Per tag, a position from which there is no closing tag.
        let mut unclosed: [Option<usize>; SECTION_TAGS.len()] = [None; SECTION_TAGS.len()];
        let mut at = 0;
        while at < text.len() {
            let rest = &text[at..];
            let tag = SECTION_TAGS.iter().enumerate().find_map(|(index, tag)| {
                let open_len = tag.len() + 2;
                (rest.starts_with('<')
                    && rest[1..].starts_with(tag)
                    && rest[1 + tag.len()..].starts_with('>'))
                .then_some((index, *tag, open_len))
            });
            let mut matched = None;
            if let Some((index, tag, open_len)) = tag {
                let from = at + open_len;
                if unclosed[index].is_none_or(|none_from| from < none_from) {
                    let close = format!("</{tag}>");
                    match text[from..].find(&close) {
                        Some(offset) => matched = Some(from + offset + close.len()),
                        None => unclosed[index] = Some(from),
                    }
                }
            }
            if matched.is_none() && at == 0 {
                matched = self
                    .context_section
                    .find(text)
                    .map_err(regex_error)?
                    .map(|whole| whole.end());
            }
            match matched {
                Some(end) => {
                    found.push(at..end);
                    at = end;
                }
                None => {
                    // The next place a section can start: a `<`, or never.
                    let next = at + rest.chars().next().map_or(1, char::len_utf8);
                    at = match text[next..].find('<') {
                        Some(offset) => next + offset,
                        None => text.len(),
                    };
                }
            }
        }
        Ok(found)
    }
}

fn ruleset() -> Result<&'static Ruleset, ProjectionError> {
    RULESET
        .as_ref()
        .map_err(|error| ProjectionError::Internal(error.clone()))
}

fn internal(error: String) -> ProjectionError {
    ProjectionError::Internal(error)
}

/// A match that spent the backtracking budget is input too large to read,
/// like one past the projection's caps; any other failure is internal.
fn regex_error(error: fancy_regex::Error) -> ProjectionError {
    if pyre::budget_spent(&error) {
        ProjectionError::TooLarge("a pattern's backtracking budget".to_owned())
    } else {
        ProjectionError::Internal(error.to_string())
    }
}

/// One conversation's scrub: the ruleset plus this conversation's working
/// directories, longest first.
struct Scrubber {
    rules: &'static Ruleset,
    cwds: Vec<Regex>,
    /// The working directories `cwds` match, NUL-separated: with the text,
    /// all a scrub depends on.
    cwds_key: String,
}

/// Texts shorter than this are scrubbed again rather than remembered.
const MEMO_MIN_BYTES: usize = 256;
/// Remembered scrubs are dropped together past this many bytes.
const MEMO_MAX_BYTES: usize = 32 << 20;

/// Scrubbed texts by working directories and text. A conversation resends
/// its system prompt and harness sections unchanged on every turn.
#[derive(Default)]
struct Memo {
    scrubbed: std::collections::HashMap<(String, String), String>,
    bytes: usize,
}

static MEMO: LazyLock<std::sync::Mutex<Memo>> = LazyLock::new(Default::default);

impl Scrubber {
    fn scrub(&self, text: &str) -> Result<String, ProjectionError> {
        if text.len() < MEMO_MIN_BYTES {
            return self.scrub_text(text);
        }
        let key = (self.cwds_key.clone(), text.to_owned());
        if let Some(found) = MEMO
            .lock()
            .ok()
            .and_then(|memo| memo.scrubbed.get(&key).cloned())
        {
            return Ok(found);
        }
        let scrubbed = self.scrub_text(text)?;
        if let Ok(mut memo) = MEMO.lock() {
            let size = key.0.len() + key.1.len() + scrubbed.len();
            if memo.bytes + size > MEMO_MAX_BYTES {
                *memo = Memo::default();
            }
            memo.bytes += size;
            memo.scrubbed.insert(key, scrubbed.clone());
        }
        Ok(scrubbed)
    }

    fn scrub_text(&self, text: &str) -> Result<String, ProjectionError> {
        let mut text = text.to_owned();
        for cwd in &self.cwds {
            text = pyre::sub(cwd, &text, 0, |_, out| out.push_str("<CWD>")).map_err(regex_error)?;
        }
        for pattern in &self.rules.cwd {
            text = pyre::sub(pattern, &text, 0, |captures, out| {
                let whole = captures.get(0).map_or("", |found| found.as_str());
                match captures.get(1).map(|found| found.as_str()) {
                    Some(value) if !value.is_empty() => {
                        out.push_str(&whole.replace(value, "<CWD>"));
                    }
                    _ => out.push_str(whole),
                }
            })
            .map_err(regex_error)?;
        }
        for (pattern, template) in &self.rules.rules {
            text = pyre::sub(pattern, &text, 0, |captures, out| {
                template.expand(captures, out);
            })
            .map_err(regex_error)?;
        }
        Ok(text)
    }

    /// A user text block: its leading time stamp, then each harness section.
    fn user_text(&self, text: &str) -> Result<String, ProjectionError> {
        let stamped = pyre::sub(&self.rules.turn_stamp, text, 1, |_, out| {
            out.push_str("[<DATE>] ");
        })
        .map_err(regex_error)?;
        let mut scrubbed = String::with_capacity(stamped.len());
        let mut cursor = 0;
        for section in self.rules.sections(&stamped)? {
            scrubbed.push_str(&stamped[cursor..section.start]);
            scrubbed.push_str(&self.scrub(&stamped[section.clone()])?);
            cursor = section.end;
        }
        scrubbed.push_str(&stamped[cursor..]);
        Ok(scrubbed)
    }

    fn strings(&self, value: &Json) -> Result<Json, ProjectionError> {
        Ok(match value {
            Json::String(text) => Json::String(self.scrub(text)?),
            Json::Array(items) => Json::Array(
                items
                    .iter()
                    .map(|item| self.strings(item))
                    .collect::<Result<_, _>>()?,
            ),
            Json::Object(object) => Json::Object(
                object
                    .iter()
                    .map(|(key, item)| Ok((key.to_owned(), self.strings(item)?)))
                    .collect::<Result<Object, ProjectionError>>()?,
            ),
            other => other.clone(),
        })
    }
}

/// A text block's text; a text block without a string `text` is refused.
fn block_text(block: &Object) -> Result<&str, ProjectionError> {
    block
        .str("text")
        .ok_or_else(|| ProjectionError::Refused("a text block has no text string".into()))
}

fn is_text(block: &Object) -> bool {
    block.str("type") == Some("text")
}

/// Rewrites `messages` and `tools` in place under `scrub_v2`.
pub fn scrub_environment(
    messages: &mut [Message],
    tools: &mut [Json],
) -> Result<(), ProjectionError> {
    let rules = ruleset()?;
    // The working directories this conversation names, read from every text
    // in scope, the tool definitions included.
    let mut texts: Vec<String> = Vec::new();
    for message in messages.iter().filter(|message| message.role == "system") {
        for block in message.content.iter().filter(|block| is_text(block)) {
            texts.push(block_text(block)?.to_owned());
        }
    }
    for message in messages.iter().filter(|message| message.role == "user") {
        for block in message.content.iter().filter(|block| is_text(block)) {
            let text = block_text(block)?;
            for section in rules.sections(text)? {
                texts.push(text[section].to_owned());
            }
        }
    }
    let tools_text = Json::Array(tools.to_vec()).dumps(false);
    texts.push(tools_text);
    let mut values: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for text in &texts {
        for pattern in &rules.cwd {
            for captures in pattern.captures_iter(text) {
                let captures = captures.map_err(regex_error)?;
                let value = pyre::strip(captures.get(1).map_or("", |found| found.as_str()));
                if value.chars().count() >= 2 && seen.insert(value.to_owned()) {
                    // Each directory is a pattern run over every text.
                    if values.len() == MAX_DIRECTORIES {
                        return Err(ProjectionError::TooLarge(format!(
                            "more than {MAX_DIRECTORIES} working directories"
                        )));
                    }
                    values.push(value.to_owned());
                }
            }
        }
    }
    // Longest first, so a directory is replaced before any of its parents.
    // Values of one length are unordered upstream; ordered here so the
    // result never depends on discovery order.
    values.sort_by(|left, right| {
        right
            .chars()
            .count()
            .cmp(&left.chars().count())
            .then_with(|| left.cmp(right))
    });
    let cwds = values
        .iter()
        .map(|value| {
            // Whole path occurrences only: `/app` must not rewrite `/application`.
            let pattern = format!(
                "{}{}{}",
                pyre::translate(r"(?<![\w.-])", false),
                fancy_regex::escape(value),
                pyre::translate(r"(?![\w-])(?!\.\w)", false)
            );
            pyre::compile(&pattern).map_err(internal)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let scrubber = Scrubber {
        rules,
        cwds,
        cwds_key: values.join("\0"),
    };

    for message in messages.iter_mut() {
        if message.role != "system" && message.role != "user" {
            continue;
        }
        for block in message.content.iter_mut().filter(|block| is_text(block)) {
            let text = block_text(block)?;
            let scrubbed = if message.role == "system" {
                scrubber.scrub(text)?
            } else {
                scrubber.user_text(text)?
            };
            block.insert("text", Json::String(scrubbed));
        }
    }
    // The tool definitions repeat unchanged on every turn of a conversation
    // and are most of the scrubbing work; their result depends only on them
    // and the working directories found.
    let tools_text = texts.pop().unwrap_or_default();
    if let Some(scrubbed) = cached_tools(&tools_text, &values) {
        tools.clone_from_slice(&scrubbed);
        return Ok(());
    }
    for tool in tools.iter_mut() {
        *tool = scrubber.strings(tool)?;
    }
    remember_tools(tools_text, values, tools.to_vec());
    Ok(())
}

/// Scrubbed tool definitions by their text and working directories.
struct ToolsEntry {
    text: String,
    cwds: Vec<String>,
    scrubbed: Vec<Json>,
}

/// The most tool sets remembered (one per harness configuration in use).
const TOOLS_CACHE: usize = 8;

static TOOLS: std::sync::Mutex<Vec<ToolsEntry>> = std::sync::Mutex::new(Vec::new());

fn cached_tools(text: &str, cwds: &[String]) -> Option<Vec<Json>> {
    let mut cache = TOOLS.lock().ok()?;
    let at = cache
        .iter()
        .position(|entry| entry.text == text && entry.cwds == cwds)?;
    // Most recently used last.
    let entry = cache.remove(at);
    let scrubbed = entry.scrubbed.clone();
    cache.push(entry);
    Some(scrubbed)
}

fn remember_tools(text: String, cwds: Vec<String>, scrubbed: Vec<Json>) {
    if let Ok(mut cache) = TOOLS.lock() {
        if cache.len() >= TOOLS_CACHE {
            cache.remove(0);
        }
        cache.push(ToolsEntry {
            text,
            cwds,
            scrubbed,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::loads;

    fn message(role: &'static str, text: &str) -> Message {
        let mut block = Object::new();
        block.insert("type", Json::str("text"));
        block.insert("text", Json::str(text));
        Message {
            role,
            content: vec![block],
            group: None,
        }
    }

    fn scrubbed(role: &'static str, text: &str) -> String {
        let mut messages = vec![message(role, text)];
        scrub_environment(&mut messages, &mut []).unwrap();
        messages[0].content[0].str("text").unwrap().to_owned()
    }

    #[test]
    fn every_pattern_compiles() {
        assert!(ruleset().is_ok());
    }

    #[test]
    fn system_text_loses_its_environment_values() {
        let text = "# Environment\n - Primary working directory: /Users/sam/code/app\n - Is a git repository: true\n \
                    - Platform: darwin\n - Shell: zsh\n - OS Version: Darwin 25.0.0\n \
                    - You are powered by the model named Opus 4.7. The exact model ID is claude-opus-4-7.\n\
                    Assistant knowledge cutoff is January 2026.\nToday's date is 2026-11-13.\n\
                    Read /Users/sam/code/app/src and /Users/sam/code/application.";
        assert_eq!(
            scrubbed("system", text),
            "# Environment\n - Primary working directory: <CWD>\n - Is a git repository: <GIT_REPO>\n \
             - Platform: <PLATFORM>\n - Shell: <SHELL>\n - OS Version: <PLATFORM>\n \
             - You are powered by the model <MODEL>. The exact model ID is <MODEL>.\n\
             Assistant knowledge cutoff is <MODEL_CUTOFF>.\nToday's date is <DATE>.\n\
             Read <CWD>/src and <HOME>/code/application."
        );
    }

    #[test]
    fn sections_match_the_reference_pattern_and_stay_linear_on_unclosed_tags() {
        // The reference's pattern, run by the backtracking engine.
        let reference = pyre::python(
            concat!(
                r"<(system-reminder|system_information|environment_details|environment_context|environment_info|",
                r"session_context|turn-context|workspace_info|user_information|current-time|working-directory)>.*?</\1>",
                r"|\A\s*<context>[ \t]*\nThe current date is [^\n]*\n.*?</context>"
            ),
            true,
        )
        .unwrap();
        let rules = ruleset().unwrap();
        for text in [
            "",
            "plain",
            "<system-reminder>a</system-reminder>",
            "x<system-reminder>a\nb</system-reminder>y<current-time>t</current-time>",
            "<system-reminder>open <system-reminder>inner</system-reminder> tail</system-reminder>",
            "<system-reminder>never closed <current-time>t</current-time>",
            "<current-time>a</system-reminder></current-time>",
            "  <context>\nThe current date is today\nbody</context> rest",
            "<context>\nThe current date is today\nbody</context><system-reminder>s</system-reminder>",
            "<system-reminder>s</system-reminder><context>\nThe current date is d\n</context>",
            "<<system-reminder>>x</system-reminder>>",
            "é<working-directory>/tmp</working-directory>ü",
        ] {
            let expected: Vec<std::ops::Range<usize>> = reference
                .find_iter(text)
                .map(|found| found.unwrap().range())
                .collect();
            assert_eq!(rules.sections(text).unwrap(), expected, "{text:?}");
        }
        let hostile = "<system-reminder>x".repeat(200_000);
        let started = std::time::Instant::now();
        assert!(rules.sections(&hostile).unwrap().is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    #[test]
    fn user_text_is_scrubbed_only_inside_harness_sections_and_a_leading_stamp() {
        assert_eq!(
            scrubbed(
                "user",
                "[Thu 2026-01-01 00:00 UTC] deploy on 2026-01-02 <system-reminder>\nToday's date is 2026-01-01.\n</system-reminder> with gpt-5"
            ),
            "[<DATE>] deploy on 2026-01-02 <system-reminder>\nToday's date is <DATE>.\n</system-reminder> with gpt-5"
        );
        assert_eq!(
            scrubbed("user", "[Monday TODO] ship <env>Darwin 25.0</env>"),
            "[Monday TODO] ship <env>Darwin 25.0</env>"
        );
    }

    #[test]
    fn assistant_text_is_untouched() {
        let text = "Today's date is 2026-01-01 on Darwin 25.0.0";
        assert_eq!(scrubbed("assistant", text), text);
    }

    #[test]
    fn tool_definition_strings_are_scrubbed_and_name_working_directories() {
        let mut tools = vec![
            loads(r#"{"name": "run", "description": "Your current working directory is /srv/job and model: x", "n": 1}"#)
                .unwrap(),
        ];
        let mut messages = vec![message("system", "cd /srv/job/src then /srv/jobs")];
        scrub_environment(&mut messages, &mut tools).unwrap();
        assert_eq!(
            messages[0].content[0].str("text").unwrap(),
            "cd <CWD>/src then /srv/jobs"
        );
        assert_eq!(
            tools[0].dumps(false),
            r#"{"name": "run", "description": "Your current working directory is <CWD> and model: x", "n": 1}"#
        );
    }
}
