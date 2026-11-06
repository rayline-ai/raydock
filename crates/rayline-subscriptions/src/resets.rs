use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(crate) const PROGRAMS: [&str; 2] = ["cedar_ember", "juniper_tide"];

/// Read-only OAuth observations, not a complete inventory of web-only offers.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LimitResetStatus {
    pub checked_at_unix_ms: i64,
    pub fresh: bool,
    pub programs: Vec<LimitResetProgram>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LimitResetProgram {
    pub program: String,
    pub eligible: bool,
    pub reason: Option<String>,
    pub cooldown_until: Option<String>,
    pub next_available_at: Option<String>,
    pub grants: Vec<LimitResetGrant>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct LimitResetGrant {
    pub id: String,
    pub title: String,
    pub remaining_count: u64,
    pub usable_now: bool,
    pub requires_limit: bool,
    pub clears: Vec<String>,
    pub starts_at: Option<String>,
    pub expires_at: Option<String>,
    pub paused: bool,
}

fn optional_text(value: Option<&Value>) -> Option<Option<String>> {
    match value {
        None | Some(Value::Null) => Some(None),
        Some(Value::String(s))
            if !s.trim().is_empty() && s.len() <= 256 && !s.chars().any(char::is_control) =>
        {
            Some(Some(s.trim().to_owned()))
        }
        _ => None,
    }
}
fn timestamp(value: Option<&Value>) -> Option<Option<String>> {
    let text = optional_text(value)?;
    if let Some(text) = &text {
        OffsetDateTime::parse(text, &Rfc3339).ok()?;
    }
    Some(text)
}
fn future(value: &Option<String>, now_ms: i64) -> bool {
    value
        .as_deref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
        .is_some_and(|t| t.unix_timestamp_nanos() / 1_000_000 > i128::from(now_ms))
}
fn expired(value: &Option<String>, now_ms: i64) -> bool {
    value.is_some() && !future(value, now_ms)
}
fn boolean(value: Option<&Value>, default: bool) -> Option<bool> {
    match value {
        None => Some(default),
        Some(value) => value.as_bool(),
    }
}
fn valid_window(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 80
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-')
}
fn windows(value: &Value) -> Option<Vec<String>> {
    value
        .as_array()?
        .iter()
        .map(|v| {
            let s = v.as_str()?;
            valid_window(s).then(|| s.to_owned())
        })
        .collect()
}

/// Null, absent and malformed blocks are unevaluated, never authoritative zero.
pub(crate) fn parse_program(
    program: &str,
    value: Option<&Value>,
    now_ms: i64,
) -> Option<LimitResetProgram> {
    let object = value?.as_object()?;
    let eligible = object.get("eligible")?.as_bool()?;
    let reason = optional_text(object.get("ineligible_reason"))?;
    let mut result = LimitResetProgram {
        program: program.to_owned(),
        eligible,
        reason,
        cooldown_until: None,
        next_available_at: None,
        grants: Vec::new(),
    };
    match program {
        "cedar_ember" => {
            result.cooldown_until = timestamp(object.get("cooldown_until"))?;
            let next = optional_text(object.get("next_grant_id"))?;
            if next
                .as_deref()
                .is_some_and(|id| id.len() > 40 || !valid_window(id))
            {
                return None;
            }
            if let Some(at_limit) = object.get("at_limit") {
                at_limit.as_bool()?;
            }
            if let Some(exhausted) = object.get("exhausted") {
                if !exhausted
                    .as_array()?
                    .iter()
                    .all(|v| v.as_str().is_some_and(valid_window))
                {
                    return None;
                }
            }
            // Eligible responses must carry the evaluated grant list.
            let grants = match object.get("grants") {
                Some(value) => value.as_array()?.as_slice(),
                None if !eligible => &[],
                None => return None,
            };
            for raw in grants {
                let grant = raw.as_object()?;
                let id = grant.get("id")?.as_str()?;
                if id.is_empty()
                    || id.len() > 40
                    || !id.bytes().all(|c| {
                        c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_' || c == b'-'
                    })
                {
                    return None;
                }
                let remaining_count = grant.get("resets_left")?.as_u64()?;
                if let Some(total) = grant.get("resets_total") {
                    total.as_u64()?;
                }
                let label = match grant.get("label") {
                    Some(Value::String(s)) if s.trim().is_empty() => None,
                    value => optional_text(value)?,
                };
                let title = label.unwrap_or_else(|| "Claude limit reset".to_owned());
                let starts_at = timestamp(grant.get("starts_at"))?;
                let expires_at = timestamp(grant.get("ends_at"))?;
                let clears = windows(grant.get("clears")?)?;
                if !grant
                    .get("blocking")?
                    .as_array()?
                    .iter()
                    .all(|v| v.as_str().is_some_and(valid_window))
                {
                    return None;
                }
                if let Some(percent_used) = grant.get("percent_used") {
                    for (key, percent) in percent_used.as_object()? {
                        if !valid_window(key) {
                            return None;
                        }
                        if percent.as_u64()? > 100 {
                            return None;
                        }
                    }
                }
                let paused = boolean(grant.get("paused"), false)?;
                let usable_now = boolean(grant.get("usable_now"), false)?;
                let requires_limit = boolean(grant.get("use_requires_limit"), true)?;
                result.grants.push(LimitResetGrant {
                    id: id.to_owned(),
                    title,
                    remaining_count,
                    usable_now: eligible
                        && next.as_deref() == Some(id)
                        && usable_now
                        && !paused
                        && remaining_count > 0
                        && !future(&starts_at, now_ms)
                        && !expired(&expires_at, now_ms)
                        && !future(&result.cooldown_until, now_ms),
                    requires_limit,
                    clears,
                    starts_at,
                    expires_at,
                    paused,
                });
            }
        }
        "juniper_tide" => {
            let arm = optional_text(object.get("arm"))?;
            if arm
                .as_deref()
                .is_some_and(|s| s != "control" && s != "reset")
            {
                return None;
            }
            if eligible && arm.is_none() {
                return None;
            }
            if arm.as_deref() == Some("reset") && !object.contains_key("available") {
                return None;
            }
            let available = boolean(object.get("available"), false)?;
            result.next_available_at = timestamp(object.get("next_available_at"))?;
            let expires_at = timestamp(object.get("weekly_resets_at"))?;
            if let Some(count) = object.get("resets_per_week") {
                count.as_u64()?;
            }
            result.eligible = eligible && arm.as_deref() == Some("reset");
            if arm.as_deref() == Some("reset") {
                result.grants.push(LimitResetGrant {
                    id: program.to_owned(),
                    title: "Claude session limit reset".to_owned(),
                    remaining_count: u64::from(available),
                    usable_now: result.eligible
                        && available
                        && !future(&result.next_available_at, now_ms)
                        && !expired(&expires_at, now_ms),
                    requires_limit: true,
                    clears: vec!["five_hour".to_owned()],
                    starts_at: None,
                    expires_at,
                    paused: false,
                });
            } else if result.reason.is_none() && arm.as_deref() == Some("control") {
                result.reason = Some("control".to_owned());
            }
        }
        _ => return None,
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unevaluated_and_malformed_are_not_negative_answers() {
        for value in [
            Value::Null,
            json!({}),
            json!({"eligible":true,"grants":"bad"}),
        ] {
            assert!(parse_program("cedar_ember", Some(&value), 0).is_none());
        }
        let surface = parse_program(
            "cedar_ember",
            Some(&json!({"eligible":false,"ineligible_reason":"surface"})),
            0,
        )
        .expect("evaluated answer");
        assert_eq!(surface.reason.as_deref(), Some("surface"));
        assert!(surface.grants.is_empty());
    }

    #[test]
    fn banked_grants_are_not_necessarily_usable() {
        let mut value = json!({"eligible":true,"next_grant_id":"saved","grants":[{
            "id":"saved","label":"Saved reset","resets_left":3,"clears":["five_hour","seven_day"],
            "blocking":[],"percent_used":{"five_hour":100},"usable_now":true,"paused":false,
            "starts_at":"2026-01-01T00:00:00Z","ends_at":"2026-12-01T00:00:00Z"
        }]});
        let now = 1_780_000_000_000;
        let parse = |v: &Value| {
            parse_program("cedar_ember", Some(v), now)
                .expect("program")
                .grants
                .remove(0)
        };
        assert!(parse(&value).usable_now);
        value["grants"][0]["paused"] = json!(true);
        assert!(!parse(&value).usable_now);
        assert_eq!(parse(&value).remaining_count, 3);
        value["grants"][0]["paused"] = json!(false);
        value["cooldown_until"] = json!("2026-12-01T00:00:00Z");
        assert!(!parse(&value).usable_now);
        value["cooldown_until"] = Value::Null;
        value["grants"][0]["ends_at"] = json!("2026-01-01T00:00:00Z");
        assert!(!parse(&value).usable_now);
    }
}
