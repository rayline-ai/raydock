//! `routing.policy`: a versioned preset and validated overrides.

use serde::Serialize;
use serde_json::Value;

use crate::check::{Bad, Checked, array, bad, float, integer, join, member, object, one_of};
use crate::error::Refusal;

const PRESETS: &[&str] = &[
    "arc-policy.package-default.v1",
    "arc-policy.hybrid-default.v1",
    "arc-policy.prefer-local.v1",
];
/// The presets that select by a rule with thresholds a caller may override.
const SELECTION_PRESETS: &[&str] = &["arc-policy.prefer-local.v1"];
const SCHEDULE_CLASSES: &[&str] = &["cache_boundaries", "user_messages", "all_turns"];

/// Per dimension, the switch-point classes at which it may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Schedule {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<Vec<String>>,
}

/// A selection rule's thresholds.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Selection {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_p_pass: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_rest_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Overrides {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<Schedule>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<Selection>,
}

/// A validated `routing.policy`. It serializes as the decide request's
/// `policy`: the object as sent, with omitted members omitted.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Policy {
    pub preset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overrides: Option<Overrides>,
}

/// An optional member: omitted, never null.
fn optional<'a, T>(
    map: &'a serde_json::Map<String, Value>,
    at: &str,
    key: &str,
    read: impl FnOnce(&'a Value, &str) -> Checked<T>,
) -> Checked<Option<T>> {
    match map.get(key) {
        None => Ok(None),
        Some(Value::Null) => bad(&join(at, key), "is omitted, never null"),
        Some(value) => read(value, &join(at, key)).map(Some),
    }
}

fn classes(value: &Value, at: &str) -> Checked<Vec<String>> {
    let mut names = Vec::new();
    for (i, entry) in array(value, at, SCHEDULE_CLASSES.len())?.iter().enumerate() {
        let name = one_of(entry, &format!("{at}.{i}"), SCHEDULE_CLASSES)?;
        if names.contains(&name) {
            return bad(at, "names a class once");
        }
        names.push(name);
    }
    Ok(names)
}

fn policy(value: &Value) -> Checked<Policy> {
    let map = object(value, "", &["preset"], &["overrides"])?;
    let preset = one_of(member(map, "preset"), "preset", PRESETS)?;
    let overrides = optional(map, "", "overrides", |value, at| {
        let map = object(value, at, &[], &["schedule", "selection"])?;
        Ok(Overrides {
            schedule: optional(map, at, "schedule", |value, at| {
                let map = object(value, at, &[], &["model", "thinking"])?;
                Ok(Schedule {
                    model: optional(map, at, "model", classes)?,
                    thinking: optional(map, at, "thinking", classes)?,
                })
            })?,
            selection: optional(map, at, "selection", |value, at| {
                let map = object(value, at, &[], &["min_p_pass", "max_rest_tokens"])?;
                Ok(Selection {
                    min_p_pass: optional(map, at, "min_p_pass", |v, at| {
                        float(v, at, 0.0, 1.0, false)
                    })?,
                    max_rest_tokens: optional(map, at, "max_rest_tokens", |v, at| {
                        integer(v, at, 1, u64::MAX)
                    })?,
                })
            })?,
        })
    })?;
    let selects = overrides.as_ref().is_some_and(|o| o.selection.is_some());
    if selects && !SELECTION_PRESETS.contains(&preset.as_str()) {
        return bad(
            "",
            format!(
                "selection overrides apply only to a preset with a selection rule ({})",
                SELECTION_PRESETS.join(", ")
            ),
        );
    }
    Ok(Policy { preset, overrides })
}

pub fn parse_policy(value: &Value) -> Result<Policy, Refusal> {
    policy(value).map_err(|Bad { at, why }| {
        let at = if at.is_empty() { "routing.policy" } else { &at };
        Refusal::new(
            "policy_invalid",
            format!("routing.policy is malformed at {at}: {why}."),
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_preset_alone_is_sent_on_as_it_came() {
        let policy = parse_policy(&json!({"preset": "arc-policy.hybrid-default.v1"})).unwrap();
        assert_eq!(
            serde_json::to_value(&policy).unwrap(),
            json!({"preset": "arc-policy.hybrid-default.v1"})
        );
    }

    #[test]
    fn overrides_keep_only_what_was_sent() {
        let sent = json!({"preset": "arc-policy.prefer-local.v1", "overrides": {
            "schedule": {"model": ["user_messages", "cache_boundaries"]},
            "selection": {"min_p_pass": 1, "max_rest_tokens": 5000}}});
        let policy = parse_policy(&sent).unwrap();
        assert_eq!(
            serde_json::to_value(&policy).unwrap(),
            json!({"preset": "arc-policy.prefer-local.v1", "overrides": {
                "schedule": {"model": ["user_messages", "cache_boundaries"]},
                "selection": {"min_p_pass": 1.0, "max_rest_tokens": 5000}}})
        );
        let empty = json!({"preset": "arc-policy.package-default.v1", "overrides": {}});
        assert_eq!(
            serde_json::to_value(parse_policy(&empty).unwrap()).unwrap(),
            empty
        );
    }

    #[test]
    fn malformed_policies_are_refused() {
        for sent in [
            json!("arc-policy.hybrid-default.v1"),
            json!({}),
            json!({"preset": "arc-policy.unknown.v1"}),
            json!({"preset": "arc-policy.hybrid-default.v1", "extra": 1}),
            json!({"preset": "arc-policy.hybrid-default.v1", "overrides": null}),
            json!({"preset": "arc-policy.hybrid-default.v1", "overrides": {"schedule": {"model": null}}}),
            json!({"preset": "arc-policy.hybrid-default.v1", "overrides": {"schedule": {"model": ["all_turns", "all_turns"]}}}),
            json!({"preset": "arc-policy.hybrid-default.v1", "overrides": {"schedule": {"thinking": ["sometimes"]}}}),
            json!({"preset": "arc-policy.hybrid-default.v1", "overrides": {"selection": {"min_p_pass": 0.5}}}),
            json!({"preset": "arc-policy.prefer-local.v1", "overrides": {"selection": {"min_p_pass": 1.5}}}),
            json!({"preset": "arc-policy.prefer-local.v1", "overrides": {"selection": {"max_rest_tokens": 0}}}),
            json!({"preset": "arc-policy.prefer-local.v1", "overrides": {"selection": {"max_rest_tokens": 2.0}}}),
        ] {
            let refusal = parse_policy(&sent).unwrap_err();
            assert_eq!(
                (refusal.status, refusal.code.as_str()),
                (400, "policy_invalid"),
                "{sent}"
            );
        }
    }
}
