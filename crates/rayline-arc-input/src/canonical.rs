//! The canonical conversation: one form for a conversation's messages and
//! tool definitions, with request metadata removed and every piece of text a
//! model can read kept.
//!
//! - String content becomes one text block.
//! - Consecutive messages of one role merge (system messages never do), unless
//!   their attribution differs.
//! - Tool ids are renamed `t1`, `t2`, ... in call order, and each message's
//!   tool results are put in that order.
//! - `cache_control`, thinking signatures, `tool_use.caller`, an empty
//!   `citations` list and `is_error: false` are dropped.
//! - The system prompt ahead of the first user message loses its per-request
//!   billing header line.
//! - A tool definition becomes `{"type": "function", "name", "description",
//!   "parameters"}` plus whatever else it carries.

use std::collections::HashMap;

use crate::ProjectionError;
use crate::json::{self, Json, Object, ParseError};
use crate::pyre;

/// Message members that are read; any other member is refused.
const MESSAGE_KEYS: &[&str] = &[
    "role",
    "content",
    "tool_calls",
    "tool_call_id",
    "name",
    "reasoning",
    "reasoning_content",
    "refusal",
    "reasoning_details",
];
/// Members some client SDKs always send; accepted only when null.
const NULL_DEFAULTS: &[&str] = &["annotations", "audio", "function_call"];
const BILLING_HEADER: &str = "x-anthropic-billing-header:";

/// One canonical message.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// `system`, `user` or `assistant`.
    pub role: &'static str,
    pub content: Vec<Object>,
    /// The caller's attribution; consecutive messages merge only within one.
    pub group: Option<String>,
}

fn refused(reason: impl Into<String>) -> ProjectionError {
    ProjectionError::Refused(reason.into())
}

fn object(entries: &[(&str, Json)]) -> Object {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

/// A value Python can hash: a list or an object as a block's `type` is a
/// request the reference fails on.
fn kind(block: &Object) -> Result<Option<&str>, ProjectionError> {
    match block.get("type") {
        Some(Json::Array(_) | Json::Object(_)) => Err(refused("a block type is not a string")),
        Some(Json::String(kind)) => Ok(Some(kind)),
        _ => Ok(None),
    }
}

/// Canonicalizes `messages` (each already checked to be an object with one of
/// the four wire roles). `groups` names one attribution per message. A system
/// message ahead of the first user message is the harness's system prompt.
pub fn canonical_conversation(
    messages: &[Object],
    groups: &[Option<String>],
) -> Result<Vec<Message>, ProjectionError> {
    let mut merged: Vec<Message> = Vec::new();
    let mut seen_user = false;
    for (message, group) in messages.iter().zip(groups) {
        let (role, mut blocks) = message_blocks(message)?;
        if role == "system" && !seen_user {
            blocks = shell_text(blocks)?;
        }
        seen_user = seen_user || role == "user";
        match merged.last_mut() {
            Some(previous)
                if role != "system" && previous.role == role && previous.group == *group =>
            {
                previous.content.extend(blocks);
            }
            _ => merged.push(Message {
                role,
                content: blocks,
                group: group.clone(),
            }),
        }
    }
    renumber_tools(&mut merged);
    merged.retain(|message| !message.content.is_empty());
    Ok(merged)
}

fn message_blocks(message: &Object) -> Result<(&'static str, Vec<Object>), ProjectionError> {
    let role = match message.str("role") {
        Some("system") => "system",
        Some("user") => "user",
        Some("assistant") => "assistant",
        Some("tool") => "tool",
        _ => return Err(refused("unsupported message role")),
    };
    let canonical_role = if role == "tool" { "user" } else { role };
    if message.str("type") == Some("message") {
        return Ok((canonical_role, content_blocks(message.get("content"))?));
    }
    // Per-turn output control on a system message is metadata; anywhere else
    // it is an unknown member.
    let control = role == "system" && matches!(message.get("output_config"), Some(Json::Object(_)));
    for (key, value) in message.iter() {
        let known = MESSAGE_KEYS.contains(&key)
            || (control && key == "output_config")
            || (NULL_DEFAULTS.contains(&key) && *value == Json::Null);
        if !known {
            return Err(refused(format!("unsupported message member {key:?}")));
        }
    }
    if role == "tool" {
        let Some(id) = message.str("tool_call_id") else {
            return Err(refused("a tool message requires its tool_call_id"));
        };
        let result = object(&[
            ("type", Json::str("tool_result")),
            ("tool_use_id", Json::str(id)),
            (
                "content",
                message.get("content").cloned().unwrap_or(Json::Null),
            ),
        ]);
        return Ok(("user", vec![clean(&result)?]));
    }
    let mut blocks: Vec<Object> = ["reasoning_content", "reasoning"]
        .iter()
        .filter_map(|key| message.str(key))
        .filter(|text| !text.is_empty())
        .map(thinking)
        .collect();
    if blocks.is_empty() {
        // Structured reasoning beside a string field restates it.
        blocks.extend(reasoning_detail_blocks(message.get("reasoning_details"))?);
    }
    blocks.extend(content_blocks(message.get("content"))?);
    if let Some(text) = message.str("refusal").filter(|text| !text.is_empty()) {
        blocks.push(text_block(text));
    }
    match message.get("tool_calls") {
        Some(Json::Array(calls)) => {
            for call in calls {
                let Json::Object(call) = call else {
                    return Err(refused("a tool call is not an object"));
                };
                let empty = Object::new();
                let function = match call.get("function") {
                    Some(Json::Object(function)) => function,
                    Some(other) if other.truthy() => {
                        return Err(refused("a tool call's function is not an object"));
                    }
                    _ => &empty,
                };
                let field = |from: &Object, key: &str| from.get(key).cloned().unwrap_or(Json::Null);
                blocks.push(clean(&object(&[
                    ("type", Json::str("tool_use")),
                    ("id", field(call, "id")),
                    ("name", field(function, "name")),
                    ("input", field(function, "arguments")),
                ]))?);
            }
        }
        Some(other) if other.truthy() => return Err(refused("tool_calls is not a list")),
        _ => {}
    }
    Ok((canonical_role, blocks))
}

fn thinking(text: &str) -> Object {
    object(&[
        ("type", Json::str("thinking")),
        ("thinking", Json::str(text)),
    ])
}

fn text_block(text: &str) -> Object {
    object(&[("type", Json::str("text")), ("text", Json::str(text))])
}

/// Thinking blocks from structured reasoning items: the text of each text and
/// summary item, in order. Encrypted items are metadata.
fn reasoning_detail_blocks(details: Option<&Json>) -> Result<Vec<Object>, ProjectionError> {
    let items = match details {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(items)) => items,
        Some(_) => return Err(refused("reasoning_details must be a list of items")),
    };
    let mut out = Vec::new();
    for item in items {
        let Json::Object(item) = item else {
            return Err(refused("reasoning_details must be a list of items"));
        };
        let field = match kind(item)? {
            Some("reasoning.encrypted") => continue,
            Some("reasoning.text") => "text",
            Some("reasoning.summary") => "summary",
            _ => return Err(refused("unsupported reasoning_details item type")),
        };
        match item.str(field) {
            Some("") => {}
            Some(text) => out.push(thinking(text)),
            None => return Err(refused("a reasoning_details item has no text")),
        }
    }
    Ok(out)
}

fn content_blocks(content: Option<&Json>) -> Result<Vec<Object>, ProjectionError> {
    match content {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::String(text)) if text.is_empty() => Ok(Vec::new()),
        Some(Json::String(text)) => Ok(vec![text_block(text)]),
        Some(Json::Array(blocks)) => blocks
            .iter()
            .map(|block| match block {
                Json::Object(block) => clean(block),
                _ => Err(refused(
                    "message content must be a string or a list of blocks",
                )),
            })
            .collect(),
        Some(_) => Err(refused(
            "message content must be a string or a list of blocks",
        )),
    }
}

/// One content block with its metadata removed.
fn clean(block: &Object) -> Result<Object, ProjectionError> {
    let mut out: Object = block
        .iter()
        .filter(|(key, _)| *key != "cache_control")
        .map(|(key, value)| (key.to_owned(), value.clone()))
        .collect();
    match kind(&out)? {
        Some("text" | "input_text" | "output_text") => {
            out.insert("type", Json::str("text"));
            for empty in ["citations", "annotations"] {
                if !out.get(empty).is_some_and(Json::truthy) {
                    out.remove(empty);
                }
            }
        }
        Some("refusal") => {
            let Some(text) = out.str("refusal") else {
                return Err(refused("a refusal part requires its refusal text"));
            };
            out = text_block(text);
        }
        Some("image_url") => {
            let url = match out.get("image_url") {
                Some(Json::Object(image)) => image.get("url").cloned().unwrap_or(Json::Null),
                Some(other) if other.truthy() => {
                    return Err(refused("an image_url part is not an object"));
                }
                _ => Json::Null,
            };
            let source = object(&[("type", Json::str("url")), ("url", url)]);
            out = object(&[
                ("type", Json::str("image")),
                ("source", Json::Object(source)),
            ]);
        }
        Some("tool_use" | "server_tool_use") => {
            out.remove("caller");
            if let Some(arguments) = out.str("input") {
                match json::loads(arguments) {
                    Ok(parsed) => out.insert("input", parsed),
                    // Arguments that are not JSON stay as their text.
                    Err(ParseError::Syntax(_)) => {}
                    Err(ParseError::Unsupported(reason)) => {
                        return Err(ProjectionError::Unsupported(reason));
                    }
                }
            }
        }
        Some("thinking") => {
            out.remove("signature");
        }
        Some("tool_result") => {
            if out.get("is_error") == Some(&Json::Bool(false)) {
                out.remove("is_error");
            }
            let content = content_blocks(out.get("content"))?;
            out.insert(
                "content",
                Json::Array(content.into_iter().map(Json::Object).collect()),
            );
        }
        _ => {}
    }
    Ok(out)
}

/// The system prompt's blocks without the per-request billing header line; a
/// text block left blank is dropped.
fn shell_text(blocks: Vec<Object>) -> Result<Vec<Object>, ProjectionError> {
    let mut out = Vec::new();
    for mut block in blocks {
        if block.str("type") != Some("text") {
            out.push(block);
            continue;
        }
        let Some(text) = block.str("text") else {
            return Err(refused("a text block has no text string"));
        };
        let kept: String = text
            .split_inclusive('\n')
            .filter(|line| !line.starts_with(BILLING_HEADER))
            .collect();
        if !pyre::strip(&kept).is_empty() {
            block.insert("text", Json::String(kept));
            out.push(block);
        }
    }
    Ok(out)
}

/// Renames tool ids to call order and sorts each message's tool results into it.
fn renumber_tools(messages: &mut [Message]) {
    let mut numbers: HashMap<String, usize> = HashMap::new();
    let mut number = |id: Option<&Json>| -> usize {
        let key = id.unwrap_or(&Json::Null).dumps(false);
        let next = numbers.len() + 1;
        *numbers.entry(key).or_insert(next)
    };
    for message in messages {
        for block in &mut message.content {
            if matches!(block.str("type"), Some("tool_use" | "server_tool_use")) {
                let value = number(block.get("id"));
                block.insert("id", Json::String(format!("t{value}")));
            }
        }
        let slots: Vec<usize> = (0..message.content.len())
            .filter(|index| message.content[*index].contains("tool_use_id"))
            .collect();
        let mut results: Vec<(usize, Object)> = slots
            .iter()
            .map(|slot| {
                let block = message.content[*slot].clone();
                (number(block.get("tool_use_id")), block)
            })
            .collect();
        // Stable: results of one call keep their order.
        results.sort_by_key(|(value, _)| *value);
        for (slot, (value, mut block)) in slots.into_iter().zip(results) {
            block.insert("tool_use_id", Json::String(format!("t{value}")));
            message.content[slot] = block;
        }
    }
}

/// Tool definitions in one form, in the order sent. `cache_control` and
/// `strict` are metadata; a definition without a name is refused.
pub fn canonical_tool_definitions(tools: &[Json]) -> Result<Vec<Json>, ProjectionError> {
    let mut out = Vec::new();
    for tool in tools {
        let Json::Object(tool) = tool else {
            return Err(refused("a tool definition must be an object"));
        };
        let item: Object = tool
            .iter()
            .filter(|(key, _)| !matches!(*key, "cache_control" | "strict"))
            .map(|(key, value)| (key.to_owned(), value.clone()))
            .collect();
        let function = Json::str("function");
        let item: Object = match (item.get("type"), item.get("function")) {
            (Some(kind), Some(Json::Object(inner))) if *kind == function => {
                std::iter::once(("type".to_owned(), function))
                    .chain(
                        inner
                            .iter()
                            .filter(|(key, _)| *key != "strict")
                            .map(|(key, value)| (key.to_owned(), value.clone())),
                    )
                    .collect()
            }
            _ if item.contains("input_schema") => {
                let schema = item.get("input_schema").cloned().unwrap_or(Json::Null);
                std::iter::once(("type".to_owned(), function))
                    .chain(
                        item.iter()
                            .filter(|(key, _)| *key != "input_schema")
                            .map(|(key, value)| (key.to_owned(), value.clone())),
                    )
                    .chain(std::iter::once(("parameters".to_owned(), schema)))
                    .collect()
            }
            _ if !item.contains("type") => std::iter::once(("type".to_owned(), function))
                .chain(
                    item.iter()
                        .map(|(key, value)| (key.to_owned(), value.clone())),
                )
                .collect(),
            _ => item,
        };
        if item.str("name").is_none() {
            return Err(refused("a tool definition requires its name"));
        }
        out.push(Json::Object(item));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::loads;

    fn conversation(messages: &str) -> Vec<(String, String)> {
        let Json::Array(messages) = loads(messages).unwrap() else {
            panic!("not a list");
        };
        let messages: Vec<Object> = messages
            .into_iter()
            .map(|message| message.as_object().unwrap().clone())
            .collect();
        let groups = vec![None; messages.len()];
        canonical_conversation(&messages, &groups)
            .unwrap()
            .into_iter()
            .map(|message| {
                let content = Json::Array(message.content.into_iter().map(Json::Object).collect());
                (message.role.to_owned(), content.dumps(true))
            })
            .collect()
    }

    #[test]
    fn user_messages_merge_and_strings_become_text_blocks() {
        assert_eq!(
            conversation(
                r#"[{"role": "user", "content": "a"},
                    {"role": "user", "content": [{"type": "text", "text": "b", "cache_control": {"type": "ephemeral"}, "citations": []}]},
                    {"role": "assistant", "content": ""},
                    {"role": "system", "content": "s1"},
                    {"role": "system", "content": "s2"}]"#
            ),
            [
                (
                    "user".to_owned(),
                    r#"[{"text": "a", "type": "text"}, {"text": "b", "type": "text"}]"#.to_owned()
                ),
                (
                    "system".to_owned(),
                    r#"[{"text": "s1", "type": "text"}]"#.to_owned()
                ),
                (
                    "system".to_owned(),
                    r#"[{"text": "s2", "type": "text"}]"#.to_owned()
                ),
            ]
        );
    }

    #[test]
    fn tool_ids_are_renumbered_and_results_put_in_call_order() {
        assert_eq!(
            conversation(
                r#"[{"role": "user", "content": "go"},
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "hm", "signature": "sig"},
                        {"type": "tool_use", "id": "toolu_b", "name": "ls", "input": "{\"path\": \".\"}", "caller": {"type": "direct"}},
                        {"type": "tool_use", "id": "toolu_a", "name": "cat", "input": "not json"}]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "toolu_a", "content": "A", "is_error": false},
                        {"type": "tool_result", "tool_use_id": "toolu_b", "is_error": true}]}]"#
            )[1..],
            [
                (
                    "assistant".to_owned(),
                    concat!(
                        r#"[{"thinking": "hm", "type": "thinking"}, "#,
                        r#"{"id": "t1", "input": {"path": "."}, "name": "ls", "type": "tool_use"}, "#,
                        r#"{"id": "t2", "input": "not json", "name": "cat", "type": "tool_use"}]"#
                    )
                    .to_owned()
                ),
                (
                    "user".to_owned(),
                    concat!(
                        r#"[{"content": [], "is_error": true, "tool_use_id": "t1", "type": "tool_result"}, "#,
                        r#"{"content": [{"text": "A", "type": "text"}], "tool_use_id": "t2", "type": "tool_result"}]"#
                    )
                    .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn the_leading_system_prompt_loses_its_billing_header() {
        assert_eq!(
            conversation(
                r#"[{"role": "system", "content": [
                        {"type": "text", "text": "x-anthropic-billing-header: cc_version=1; cch=0;\n"},
                        {"type": "text", "text": "You are an agent.\nx-anthropic-billing-header: a\nBe brief."}]},
                    {"role": "user", "content": "hi"},
                    {"role": "system", "content": "x-anthropic-billing-header: kept after the first user turn"}]"#
            ),
            [
                (
                    "system".to_owned(),
                    r#"[{"text": "You are an agent.\nBe brief.", "type": "text"}]"#.to_owned()
                ),
                (
                    "user".to_owned(),
                    r#"[{"text": "hi", "type": "text"}]"#.to_owned()
                ),
                (
                    "system".to_owned(),
                    r#"[{"text": "x-anthropic-billing-header: kept after the first user turn", "type": "text"}]"#
                        .to_owned()
                ),
            ]
        );
    }

    #[test]
    fn differently_attributed_assistant_messages_stay_apart() {
        let Json::Array(messages) = loads(
            r#"[{"role": "assistant", "content": "a"}, {"role": "assistant", "content": "b"},
                {"role": "assistant", "content": "c"}]"#,
        )
        .unwrap() else {
            panic!("not a list");
        };
        let messages: Vec<Object> = messages
            .into_iter()
            .map(|message| message.as_object().unwrap().clone())
            .collect();
        let groups = [Some("x".to_owned()), Some("x".to_owned()), None];
        let merged = canonical_conversation(&messages, &groups).unwrap();
        assert_eq!(
            merged
                .iter()
                .map(|message| message.content.len())
                .collect::<Vec<_>>(),
            [2, 1]
        );
    }

    #[test]
    fn unknown_message_members_are_refused() {
        let message = loads(r#"{"role": "user", "content": "a", "metadata": {}}"#).unwrap();
        let message = message.as_object().unwrap().clone();
        assert!(matches!(
            canonical_conversation(&[message], &[None]),
            Err(ProjectionError::Refused(_))
        ));
    }

    #[test]
    fn tool_definitions_take_one_form() {
        let Json::Array(tools) = loads(
            r#"[{"name": "a", "description": "d", "input_schema": {"type": "object"}, "cache_control": {}, "defer_loading": true},
                {"type": "function", "function": {"name": "b", "parameters": {}, "strict": true}, "strict": true},
                {"type": "bash_20250124", "name": "bash"}]"#,
        )
        .unwrap() else {
            panic!("not a list");
        };
        let canonical = canonical_tool_definitions(&tools).unwrap();
        assert_eq!(
            Json::Array(canonical).dumps(false),
            concat!(
                r#"[{"type": "function", "name": "a", "description": "d", "defer_loading": true, "parameters": {"type": "object"}}, "#,
                r#"{"type": "function", "name": "b", "parameters": {}}, "#,
                r#"{"type": "bash_20250124", "name": "bash"}]"#
            )
        );
        assert!(
            canonical_tool_definitions(&[loads(r#"{"type": "web_search"}"#).unwrap()]).is_err()
        );
    }
}
