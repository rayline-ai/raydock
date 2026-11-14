//! The ARC router's input projection: an Anthropic Messages request in, the
//! token ids and readout offsets the ARC encoder reads out.
//!
//! ARC routes on a readout of the conversation. The request is first put in
//! one canonical form, so that what the encoder reads does not depend on wire
//! details or on the machine the request came from:
//!
//! 1. the system prompt, the tool definitions and the messages are
//!    canonicalized (`canonical`);
//! 2. environment values in harness text are replaced by placeholders
//!    (`scrub`, the `scrub_v2` ruleset);
//! 3. each canonical message becomes one role block,
//!    `<|im_start|>{role}\n{content JSON}<|im_end|>\n`, the tool definitions
//!    first when the request has any;
//! 4. each block is tokenized on its own, and the encoder is read at each
//!    block's last token.
//!
//! This is the encoding profile of the published ARC 1.0 weights
//! (`canonical_v2`, `scrub_v2`, harness shell included, `arc-role-blocks-v2`,
//! `final_layer_block_last_token`). Output must match the reference
//! implementation token for token; the parity tests hold it to that.
//!
//! Not handled yet: images, documents and audio. A request carrying one is
//! reported as [`ProjectionError::Unsupported`], never encoded differently.
//!
//! One known difference: the environment rules classify characters with this
//! crate's Unicode tables, the reference with Unicode 15.1. A character
//! assigned since then (a letter new in Unicode 16, say) directly beside a
//! value a rule replaces can move that rule's word boundary.

mod canonical;
mod json;
mod pyre;
mod scrub;

use std::fmt;

use tokenizers::pre_tokenizers::byte_level::ByteLevel;
use tokenizers::pre_tokenizers::sequence::Sequence;
use tokenizers::pre_tokenizers::split::{Split, SplitPattern};
use tokenizers::{AddedToken, SplitDelimiterBehavior, Tokenizer};

use json::{Json, Object, ParseError};

/// The three tokens that frame an image in the encoder's input. Text that
/// spells one must never be read as the token itself.
const VISION_START: &str = "<|vision_start|>";
const IMAGE_PAD: &str = "<|image_pad|>";
const VISION_END: &str = "<|vision_end|>";
/// Every vision token the encoder's tokenizer defines.
const VISION_TOKENS: [&str; 5] = [
    VISION_START,
    VISION_END,
    "<|vision_pad|>",
    IMAGE_PAD,
    "<|video_pad|>",
];
/// How an image is marked inside a block before tokenization; block text
/// that already contains the marker cannot be told from an image.
const IMAGE_SENTINEL: &str = "\"\\u0000arc-image:";
/// Tool types sent without a name that are left out of the tool definitions
/// rather than refused.
const OMITTED_NAMELESS_TOOLS: [&str; 1] = ["web_search"];

/// The most text (bytes of strings, keys and numbers, images not counted)
/// a request's conversation and tool definitions may hold: 16 bytes for
/// each token of the encoder's largest context (270,336 tokens), far more
/// than text ever takes per token. Above it the request is refused before
/// the scrub and tokenizer read it, so their work stays bounded.
pub const MAX_TEXT_BYTES: usize = 270_336 * 16;
/// The most JSON values a request body may hold. Checked as soon as it is
/// parsed, before anything is copied.
pub const MAX_NODES: usize = 1_000_000;
/// The most tool definitions a request may carry.
pub const MAX_TOOLS: usize = 1_024;

/// Why a request has no encoder input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionError {
    /// The request body is not JSON.
    Json(String),
    /// The request has a shape the projection does not encode.
    Refused(String),
    /// The request is valid but this crate cannot encode it yet.
    Unsupported(String),
    /// The tokenizer failed or is not the ARC encoder's.
    Tokenizer(String),
    /// The request holds more than the projection reads ([`MAX_TEXT_BYTES`],
    /// [`MAX_TOOLS`], or the working directories it names).
    TooLarge(String),
    /// A built-in rule failed to compile or run.
    Internal(String),
}

impl fmt::Display for ProjectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(reason) => write!(f, "request is not JSON: {reason}"),
            Self::Refused(reason) => write!(f, "request cannot be projected: {reason}"),
            Self::Unsupported(reason) => write!(f, "not supported yet: {reason}"),
            Self::Tokenizer(reason) => write!(f, "tokenizer: {reason}"),
            Self::TooLarge(reason) => write!(f, "request too large to project: {reason}"),
            Self::Internal(reason) => write!(f, "projection rule failed: {reason}"),
        }
    }
}

impl std::error::Error for ProjectionError {}

fn refused(reason: &str) -> ProjectionError {
    ProjectionError::Refused(reason.to_owned())
}

/// One block of the encoder's input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleBlock {
    /// `system`, `user` or `assistant`.
    pub role: &'static str,
    /// The block's canonical content, as JSON text with sorted keys.
    pub content: String,
    /// The attribution of the assistant messages this block holds (the
    /// `attribution` entry they share), `None` for every other block.
    pub source: Option<String>,
}

impl RoleBlock {
    /// The text the encoder's tokenizer reads for this block.
    pub fn serialized(&self) -> String {
        format!("<|im_start|>{}\n{}<|im_end|>\n", self.role, self.content)
    }
}

/// What the encoder is given for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncoderInput {
    pub tokens: Vec<u32>,
    /// The index of each block's last token, in block order: where the
    /// encoder's final layer is read. The last one is the decision readout.
    pub readout_offsets: Vec<usize>,
}

/// Projects an Anthropic Messages request body (`system`, `tools`,
/// `messages`; other members are not read) onto the encoder's role blocks.
///
/// `attribution` names, per message, which routed arm produced it (`None`
/// for user and system messages and for unattributed assistant messages), or
/// is empty when nothing is attributed. It only decides whether consecutive
/// assistant messages merge: they do when their attribution is the same.
pub fn project_request(
    body: &str,
    attribution: &[Option<String>],
) -> Result<Vec<RoleBlock>, ProjectionError> {
    let body = match json::loads(body) {
        Ok(Json::Object(body)) => {
            if body.iter().map(|(_, value)| value.nodes()).sum::<usize>() > MAX_NODES {
                return Err(ProjectionError::TooLarge(format!(
                    "more than {MAX_NODES} JSON values"
                )));
            }
            body
        }
        Ok(_) => return Err(refused("the request body is not an object")),
        Err(ParseError::Syntax(reason)) => return Err(ProjectionError::Json(reason)),
        Err(ParseError::Unsupported(reason)) => return Err(ProjectionError::Unsupported(reason)),
    };
    let Some(Json::Array(wire_messages)) = body.get("messages") else {
        return Err(refused("the request has no messages list"));
    };
    if !attribution.is_empty() && attribution.len() != wire_messages.len() {
        return Err(refused("attribution must name every message or none"));
    }

    // The system prompt leads the conversation as a system message.
    let mut messages: Vec<Object> = Vec::new();
    let mut groups: Vec<Option<String>> = Vec::new();
    match body.get("system") {
        None | Some(Json::Null) => {}
        Some(system) => {
            messages.push(envelope("system", system)?);
            groups.push(None);
        }
    }
    for (index, message) in wire_messages.iter().enumerate() {
        let Json::Object(message) = message else {
            return Err(refused("a message is not an object"));
        };
        let role = match message.str("role") {
            Some(role @ ("system" | "user" | "assistant" | "tool")) => role,
            _ => return Err(refused("unsupported message role")),
        };
        // A message with only role and content is its content; one with
        // other members is read whole.
        if matches!(message.get("content"), Some(Json::Object(_))) {
            return Err(refused(
                "message content must be a string or a list of blocks",
            ));
        }
        let only_content = message
            .iter()
            .all(|(key, _)| matches!(key, "role" | "content"))
            && message.contains("content");
        let rest: Object = message
            .iter()
            .filter(|(key, _)| *key != "role")
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect();
        let content = match rest.get("content") {
            Some(content) if only_content => content.clone(),
            _ => Json::Object(rest),
        };
        // Message content reaches the canonical form with its keys sorted at
        // every level; the system prompt and tool definitions keep theirs.
        messages.push(envelope(role, &content.sorted())?);
        groups.push(match attribution.get(index) {
            Some(group) if role == "assistant" => group.clone(),
            _ => None,
        });
    }

    let tools = match body.get("tools") {
        Some(Json::Array(tools)) => tools
            .iter()
            .filter(|tool| !omitted_tool(tool))
            .cloned()
            .collect(),
        Some(other) if other.truthy() => return Err(refused("tools is not a list")),
        _ => Vec::new(),
    };
    let has_tools = !tools.is_empty();
    if tools.len() > MAX_TOOLS {
        return Err(ProjectionError::TooLarge(format!(
            "more than {MAX_TOOLS} tool definitions"
        )));
    }
    let mut definitions = canonical::canonical_tool_definitions(&tools)?;
    let mut conversation = canonical::canonical_conversation(&messages, &groups)?;
    let text: usize = definitions.iter().map(Json::text_len).sum::<usize>()
        + conversation
            .iter()
            .flat_map(|message| &message.content)
            .map(Object::text_len)
            .sum::<usize>();
    if text > MAX_TEXT_BYTES {
        return Err(ProjectionError::TooLarge(format!(
            "more than {MAX_TEXT_BYTES} bytes of text"
        )));
    }
    scrub::scrub_environment(&mut conversation, &mut definitions)?;

    let mut blocks = Vec::new();
    if has_tools {
        let mut shell = Object::new();
        shell.insert("type", Json::str("tool_definitions"));
        shell.insert("tools", Json::Array(definitions));
        blocks.push(RoleBlock {
            role: "system",
            content: Json::Array(vec![Json::Object(shell)]).dumps(true),
            source: None,
        });
    }
    for message in conversation {
        let parts = message
            .content
            .into_iter()
            .map(|part| media_part(Json::Object(part)))
            .collect::<Result<Vec<_>, _>>()?;
        blocks.push(RoleBlock {
            role: message.role,
            content: Json::Array(parts).dumps(true),
            source: message.group,
        });
    }
    if blocks
        .iter()
        .any(|block| spells_image_marker(&block.content))
    {
        return Err(refused("block text collides with the image marker"));
    }
    Ok(blocks)
}

/// A message as the canonical conversation reads it: `content` under `role`,
/// or, when `content` is an object, that object's members under `role`.
fn envelope(role: &str, content: &Json) -> Result<Object, ProjectionError> {
    let mut message = match content {
        Json::Object(members) => members.clone(),
        Json::String(_) | Json::Array(_) => {
            let mut message = Object::new();
            message.insert("role", Json::str(role));
            message.insert("content", content.clone());
            message
        }
        _ => {
            return Err(refused(
                "message content is not a string, a list or an object",
            ));
        }
    };
    message.insert("role", Json::str(role));
    Ok(message)
}

/// Whether serialized block text holds an image marker: a JSON string that
/// is exactly NUL, `arc-image:`, 64 lowercase hex digits, NUL.
fn spells_image_marker(text: &str) -> bool {
    const CLOSE: &str = "\\u0000\"";
    text.match_indices(IMAGE_SENTINEL).any(|(at, _)| {
        let rest = &text.as_bytes()[at + IMAGE_SENTINEL.len()..];
        rest.len() >= 64 + CLOSE.len()
            && rest[..64]
                .iter()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            && rest[64..].starts_with(CLOSE.as_bytes())
    })
}

fn omitted_tool(tool: &Json) -> bool {
    tool.as_object().is_some_and(|tool| {
        tool.str("type")
            .is_some_and(|kind| OMITTED_NAMELESS_TOOLS.contains(&kind))
            && !tool.contains("name")
            && !tool.contains("function")
    })
}

/// One content part as the encoder reads it. Redacted thinking is reduced to
/// its length; media is not supported yet.
fn media_part(part: Json) -> Result<Json, ProjectionError> {
    let Json::Object(mut part) = part else {
        return Ok(part);
    };
    match part.str("type") {
        Some(
            kind @ ("image" | "input_image" | "document" | "input_file" | "file" | "input_audio"),
        ) => Err(ProjectionError::Unsupported(format!(
            "a {kind} content part"
        ))),
        Some("redacted_thinking") => {
            let Some(data) = part.str("data") else {
                return Err(refused("redacted_thinking carries no data string"));
            };
            let mut reduced = Object::new();
            reduced.insert("chars", Json::Number(data.chars().count().to_string()));
            reduced.insert("type", Json::str("redacted_thinking"));
            Ok(Json::Object(reduced))
        }
        Some("tool_result") => {
            if let Some(Json::Array(content)) = part.get("content") {
                let content = content
                    .iter()
                    .cloned()
                    .map(media_part)
                    .collect::<Result<Vec<_>, _>>()?;
                part.insert("content", Json::Array(content));
            }
            Ok(Json::Object(part))
        }
        _ => Ok(Json::Object(part)),
    }
}

/// The ARC encoder's tokenizer.
pub struct ArcTokenizer {
    tokenizer: Tokenizer,
    reserved: [u32; 3],
    /// Each long block's ids by its serialized text: a conversation resends
    /// all but its newest blocks unchanged, and blocks tokenize independently.
    memo: std::sync::Mutex<BlockMemo>,
}

/// Blocks shorter than this are tokenized again rather than remembered.
const MEMO_MIN_BYTES: usize = 256;
/// Remembered blocks are dropped together past this many bytes of text.
const MEMO_MAX_BYTES: usize = 32 << 20;

#[derive(Default)]
struct BlockMemo {
    ids: std::collections::HashMap<String, Vec<u32>>,
    bytes: usize,
}

/// How the reference splits text ahead of BPE. It is the Qwen2 pattern, not
/// the one in the release's `tokenizer.json`: that one also keeps combining
/// marks with the letters before them, which gives other ids for text in
/// scripts that use them.
const PRE_TOKENIZE: &str = concat!(
    r"(?i:'s|'t|'re|'ve|'m|'ll|'d)|[^\r\n\p{L}\p{N}]?\p{L}+|\p{N}| ?[^\s\p{L}\p{N}]+[\r\n]*",
    r"|\s*[\r\n]+|\s+(?!\S)|\s+"
);

impl ArcTokenizer {
    /// Builds the tokenizer from the release's `tokenizer.json` and
    /// `tokenizer_config.json`, the way the reference does (the Qwen2
    /// tokenizer of Hugging Face `transformers` 5): the vocabulary and merges
    /// of `tokenizer.json`, NFC, the Qwen2 split pattern, and every token of
    /// the config's `added_tokens_decoder` (it lists seven that
    /// `tokenizer.json` does not).
    ///
    /// Refuses files without the encoder's vision tokens.
    pub fn from_release(
        tokenizer_json: &[u8],
        tokenizer_config_json: &str,
    ) -> Result<Self, ProjectionError> {
        let failed = |error: tokenizers::Error| ProjectionError::Tokenizer(error.to_string());
        let mut tokenizer = Tokenizer::from_bytes(tokenizer_json).map_err(failed)?;
        // Inputs are never truncated or padded, whatever the file says.
        tokenizer.with_truncation(None).map_err(failed)?;
        tokenizer.with_padding(None);
        let split = Split::new(
            SplitPattern::Regex(PRE_TOKENIZE.to_owned()),
            SplitDelimiterBehavior::Isolated,
            false,
        )
        .map_err(failed)?;
        tokenizer.with_pre_tokenizer(Some(Sequence::new(vec![
            split.into(),
            ByteLevel::new(false, true, false).into(),
        ])));

        for (id, token) in added_tokens(tokenizer_config_json)? {
            if tokenizer.token_to_id(token.content.as_str()) != Some(id) {
                let added = if token.special {
                    tokenizer.add_special_tokens([token.clone()])
                } else {
                    tokenizer.add_tokens([token.clone()])
                };
                added.map_err(failed)?;
            }
            if tokenizer.token_to_id(token.content.as_str()) != Some(id) {
                return Err(ProjectionError::Tokenizer(format!(
                    "added token {} does not get id {id}; the two files do not belong together",
                    token.content
                )));
            }
        }

        let mut arc = Self {
            tokenizer,
            reserved: [0; 3],
            memo: std::sync::Mutex::default(),
        };
        for (slot, token) in [VISION_START, IMAGE_PAD, VISION_END]
            .into_iter()
            .enumerate()
        {
            let id = arc.tokenizer.token_to_id(token);
            match id {
                Some(id) if arc.ids(token)? == [id] => arc.reserved[slot] = id,
                _ => {
                    return Err(ProjectionError::Tokenizer(format!(
                        "tokenizer has no single special token {token}"
                    )));
                }
            }
        }
        Ok(arc)
    }

    fn ids(&self, text: &str) -> Result<Vec<u32>, ProjectionError> {
        self.tokenizer
            .encode_fast(text, false)
            .map(|encoding| encoding.get_ids().to_vec())
            .map_err(|error| ProjectionError::Tokenizer(error.to_string()))
    }

    /// Tokenizes role blocks into the encoder's input.
    pub fn encode(&self, blocks: &[RoleBlock]) -> Result<EncoderInput, ProjectionError> {
        let mut tokens = Vec::new();
        let mut readout_offsets = Vec::with_capacity(blocks.len());
        for block in blocks {
            tokens.extend(self.block_ids(block.serialized())?);
            match tokens.len().checked_sub(1) {
                Some(last) if readout_offsets.last() != Some(&last) => readout_offsets.push(last),
                _ => return Err(refused("a block produced no tokens")),
            }
        }
        if tokens.is_empty() {
            return Err(refused("cannot encode an empty input"));
        }
        Ok(EncoderInput {
            tokens,
            readout_offsets,
        })
    }

    /// One block's ids, from its serialized text.
    fn block_ids(&self, text: String) -> Result<Vec<u32>, ProjectionError> {
        let remember = text.len() >= MEMO_MIN_BYTES;
        if remember {
            if let Some(ids) = self
                .memo
                .lock()
                .ok()
                .and_then(|memo| memo.ids.get(&text).cloned())
            {
                return Ok(ids);
            }
        }
        let mut tokens = Vec::new();
        for piece in text_pieces(&text) {
            let ids = self.ids(piece)?;
            if ids.iter().any(|id| self.reserved.contains(id)) {
                return Err(refused("text tokenizes to a vision special token"));
            }
            tokens.extend(ids);
        }
        if remember {
            if let Ok(mut memo) = self.memo.lock() {
                if memo.bytes + text.len() > MEMO_MAX_BYTES {
                    *memo = BlockMemo::default();
                }
                memo.bytes += text.len();
                memo.ids.insert(text, tokens.clone());
            }
        }
        Ok(tokens)
    }

    /// [`project_request`], then [`ArcTokenizer::encode`].
    pub fn encode_request(
        &self,
        body: &str,
        attribution: &[Option<String>],
    ) -> Result<EncoderInput, ProjectionError> {
        self.encode(&project_request(body, attribution)?)
    }
}

/// The config's `added_tokens_decoder` as `(id, token)`, in id order.
fn added_tokens(tokenizer_config_json: &str) -> Result<Vec<(u32, AddedToken)>, ProjectionError> {
    let malformed = || ProjectionError::Tokenizer("malformed tokenizer_config.json".into());
    let config = json::loads(tokenizer_config_json).map_err(|_| malformed())?;
    let decoder = config
        .as_object()
        .and_then(|config| config.get("added_tokens_decoder"))
        .and_then(Json::as_object)
        .ok_or_else(malformed)?;
    let mut tokens = Vec::new();
    for (id, entry) in decoder.iter() {
        let id: u32 = id.parse().map_err(|_| malformed())?;
        let entry = entry.as_object().ok_or_else(malformed)?;
        let content = entry.str("content").ok_or_else(malformed)?;
        let flag = |name: &str| entry.get(name) == Some(&Json::Bool(true));
        tokens.push((
            id,
            AddedToken::from(content, flag("special"))
                .single_word(flag("single_word"))
                .lstrip(flag("lstrip"))
                .rstrip(flag("rstrip"))
                .normalized(flag("normalized")),
        ));
    }
    tokens.sort_by_key(|(id, _)| *id);
    Ok(tokens)
}

/// The pieces of `text` that are tokenized separately: all of it, unless it
/// spells a vision token. Each such literal is cut after its `<|`, so neither
/// piece spells the token and the tokenizer reads it as ordinary text.
fn text_pieces(text: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut cursor = 0;
    // Every literal opens with `<|`: one left-to-right pass over those.
    let mut from = 0;
    while let Some(at) = text[from..].find("<|") {
        let start = from + at;
        match VISION_TOKENS
            .iter()
            .find(|token| text[start..].starts_with(*token))
        {
            Some(token) => {
                pieces.extend([&text[cursor..start], &text[start..start + 2]]);
                cursor = start + token.len();
                pieces.push(&text[start + 2..cursor]);
                from = cursor;
            }
            None => from = start + 1,
        }
    }
    pieces.push(&text[cursor..]);
    pieces.retain(|piece| !piece.is_empty());
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pathological requests of a few MB are refused, or projected, within
    /// a bounded time: the caps are checked before the scrub's work.
    #[test]
    fn pathological_inputs_are_bounded() {
        use std::time::{Duration, Instant};
        let body = |system: &str, user: &str| {
            format!(
                r#"{{"system": "{system}", "messages": [{{"role": "user", "content": "{user}"}}]}}"#
            )
        };
        let within = |what: &str, body: &str| {
            let started = Instant::now();
            let result = project_request(body, &[]);
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "{what} took {:?}",
                started.elapsed()
            );
            result
        };
        // 5 MB of distinct working directories: over the text cap.
        let dirs = |count: usize| {
            (0..count)
                .map(|index| format!("<cwd>/d{index:07}</cwd>"))
                .collect::<String>()
        };
        let big = body(&dirs(5 * 1024 * 1024 / 20), "hi");
        assert!(big.len() >= 5 * 1024 * 1024);
        assert!(matches!(
            within("5 MB of directories", &big),
            Err(ProjectionError::TooLarge(reason)) if reason.contains("bytes of text")
        ));
        // Under the text cap, 100k distinct directories: over the directory cap.
        assert!(matches!(
            within("100k directories", &body(&dirs(100_000), "hi")),
            Err(ProjectionError::TooLarge(reason)) if reason.contains("working directories")
        ));
        // Under every cap, repeated unclosed sections and vision literals
        // project in linear time.
        let user = "<system-reminder>x<|vision_start|>".repeat(100_000);
        assert!(within("unclosed sections", &body("s", &user)).is_ok());
        // Two million nulls in a tool input: over the value cap, refused
        // before anything is copied.
        let nulls = vec!["null"; 2_000_000].join(",");
        let tool_use = format!(
            r#"{{"messages": [{{"role": "user", "content": "hi"}}, {{"role": "assistant", "content": [{{"type": "tool_use", "id": "t", "name": "n", "input": {{"a": [{nulls}]}}}}]}}]}}"#
        );
        assert!(matches!(
            within("2M nulls", &tool_use),
            Err(ProjectionError::TooLarge(reason)) if reason.contains("JSON values")
        ));
        // Spaces around a `workspace_path:` value: the reference pattern
        // backtracks without bound on these; the budget stops it.
        let spaces = " ".repeat(20_000);
        let system = format!("workspace_path:{spaces}x{spaces}\ny");
        let crafted =
            serde_json::json!({"system": system, "messages": [{"role": "user", "content": "hi"}]})
                .to_string();
        assert!(matches!(
            within("backtracking", &crafted),
            Err(ProjectionError::TooLarge(reason)) if reason.contains("backtracking")
        ));
        // Too many tools.
        let tools: Vec<String> = (0..=MAX_TOOLS)
            .map(|index| format!(r#"{{"name": "t{index}", "input_schema": {{}}}}"#))
            .collect();
        let with_tools = format!(
            r#"{{"tools": [{}], "messages": [{{"role": "user", "content": "hi"}}]}}"#,
            tools.join(",")
        );
        assert!(matches!(
            within("too many tools", &with_tools),
            Err(ProjectionError::TooLarge(reason)) if reason.contains("tool definitions")
        ));
    }

    #[test]
    fn vision_literals_are_cut_in_one_pass() {
        assert_eq!(
            text_pieces("a<|vision_start|>b<|x<|image_pad|>"),
            ["a", "<|", "vision_start|>", "b<|x", "<|", "image_pad|>"]
        );
        assert_eq!(text_pieces("plain"), ["plain"]);
        let many = "<|vision_start|>".repeat(200_000);
        let started = std::time::Instant::now();
        assert_eq!(text_pieces(&many).len(), 400_000);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
    }

    fn contents(body: &str) -> Vec<(&'static str, String)> {
        project_request(body, &[])
            .unwrap()
            .into_iter()
            .map(|block| (block.role, block.content))
            .collect()
    }

    #[test]
    fn tools_lead_then_the_system_prompt_then_the_messages() {
        let body = r#"{
            "model": "any", "max_tokens": 1,
            "system": [{"type": "text", "text": "Be brief.", "cache_control": {"type": "ephemeral"}}],
            "tools": [{"name": "ls", "description": "List", "input_schema": {"type": "object"}},
                      {"type": "web_search", "max_uses": 1}],
            "messages": [{"role": "user", "content": "hi"}]
        }"#;
        assert_eq!(
            contents(body),
            [
                (
                    "system",
                    r#"[{"tools": [{"description": "List", "name": "ls", "parameters": {"type": "object"}, "type": "function"}], "type": "tool_definitions"}]"#
                        .to_owned()
                ),
                ("system", r#"[{"text": "Be brief.", "type": "text"}]"#.to_owned()),
                ("user", r#"[{"text": "hi", "type": "text"}]"#.to_owned()),
            ]
        );
    }

    #[test]
    fn a_coding_turn_is_canonicalized_scrubbed_and_renumbered() {
        let body = r#"{
            "system": "You are a coding agent.\n - Primary working directory: /Users/sam/code/app\nToday's date is 2026-11-13.",
            "tools": [{"name": "Bash", "description": "Run a command in /Users/sam/code/app.",
                       "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}}],
            "messages": [
                {"role": "user", "content": "List the files."},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_01", "name": "Bash", "input": {"command": "ls"}}]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_01", "content": "Cargo.toml\nsrc"}]}]
        }"#;
        assert_eq!(
            contents(body),
            [
                (
                    "system",
                    concat!(
                        r#"[{"tools": [{"description": "Run a command in <CWD>.", "name": "Bash", "#,
                        r#""parameters": {"properties": {"command": {"type": "string"}}, "type": "object"}, "#,
                        r#""type": "function"}], "type": "tool_definitions"}]"#
                    )
                    .to_owned()
                ),
                (
                    "system",
                    r#"[{"text": "You are a coding agent.\n - Primary working directory: <CWD>\nToday's date is <DATE>.", "type": "text"}]"#
                        .to_owned()
                ),
                (
                    "user",
                    r#"[{"text": "List the files.", "type": "text"}]"#.to_owned()
                ),
                (
                    "assistant",
                    r#"[{"id": "t1", "input": {"command": "ls"}, "name": "Bash", "type": "tool_use"}]"#
                        .to_owned()
                ),
                (
                    "user",
                    r#"[{"content": [{"text": "Cargo.toml\nsrc", "type": "text"}], "tool_use_id": "t1", "type": "tool_result"}]"#
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn a_message_with_extra_members_is_read_whole() {
        // An OpenAI-style assistant turn inside an Anthropic request.
        let body = r#"{"messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": null, "reasoning": "think",
             "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "ls", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "out"}]}"#;
        assert_eq!(
            contents(body)[1..],
            [
                (
                    "assistant",
                    r#"[{"thinking": "think", "type": "thinking"}, {"id": "t1", "input": {}, "name": "ls", "type": "tool_use"}]"#
                        .to_owned()
                ),
                (
                    "user",
                    r#"[{"content": [{"text": "out", "type": "text"}], "tool_use_id": "t1", "type": "tool_result"}]"#
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn redacted_thinking_is_reduced_to_its_length() {
        let body = r#"{"messages": [{"role": "assistant", "content": [
            {"type": "redacted_thinking", "data": "abcé"}]}]}"#;
        assert_eq!(
            contents(body),
            [(
                "assistant",
                r#"[{"chars": 4, "type": "redacted_thinking"}]"#.to_owned()
            )]
        );
    }

    #[test]
    fn media_is_reported_as_unsupported() {
        for part in [
            r#"{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}"#,
            r#"{"type": "document", "source": {}}"#,
            r#"{"type": "tool_result", "tool_use_id": "x", "content": [{"type": "image", "source": {}}]}"#,
        ] {
            let body = format!(r#"{{"messages": [{{"role": "user", "content": [{part}]}}]}}"#);
            assert!(
                matches!(
                    project_request(&body, &[]),
                    Err(ProjectionError::Unsupported(_))
                ),
                "{part}"
            );
        }
    }

    #[test]
    fn malformed_requests_are_errors_not_panics() {
        for body in [
            "",
            "[]",
            "{}",
            r#"{"messages": {}}"#,
            r#"{"messages": [1]}"#,
            r#"{"messages": [{"role": "developer", "content": "x"}]}"#,
            r#"{"messages": [{"role": "user", "content": 1}]}"#,
            r#"{"messages": [{"role": "user", "content": [1]}]}"#,
            r#"{"messages": [{"role": "user", "content": [{"type": ["text"]}]}]}"#,
            r#"{"messages": [{"role": "user", "content": [{"type": "text"}]}]}"#,
            r#"{"messages": [], "system": 1}"#,
            r#"{"messages": [], "tools": "ls"}"#,
            r#"{"messages": [], "tools": [{"description": "nameless"}]}"#,
        ] {
            assert!(project_request(body, &[]).is_err(), "{body}");
        }
        assert!(
            project_request(
                r#"{"messages": [{"role": "user", "content": "x"}]}"#,
                &[None, None]
            )
            .is_err()
        );
    }

    #[test]
    fn only_a_whole_image_marker_collides() {
        let hex = "0123456789abcdef".repeat(4);
        assert!(spells_image_marker(&format!(
            r#"[{{"text": "\u0000arc-image:{hex}\u0000"}}]"#
        )));
        assert!(!spells_image_marker(&format!(
            r#"[{{"text": "\u0000arc-image:{hex}\u0000 trailing"}}]"#
        )));
        assert!(!spells_image_marker(r#"[{"text": "\u0000arc-image:zz"}]"#));
    }

    #[test]
    fn a_spelled_vision_token_is_cut_so_it_stays_text() {
        assert_eq!(text_pieces("plain"), ["plain"]);
        assert_eq!(
            text_pieces("a<|image_pad|>b<|vision_end|>"),
            ["a", "<|", "image_pad|>", "b", "<|", "vision_end|>"]
        );
        assert_eq!(text_pieces(""), Vec::<&str>::new());
    }
}
