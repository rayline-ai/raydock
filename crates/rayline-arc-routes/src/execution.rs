//! `routing.execution` version 2: the envelope an executor writes, its checks
//! against the request and the serving package, and its mapping onto a decide.

use std::collections::BTreeSet;

use serde::Serialize;
use serde_json::{Map, Value};

use crate::catalog::Catalog;
use crate::check::{
    Bad, Checked, array, bad, boolean, float, integer, join, member, nullable, object, one_of,
    opaque, sha256, ttl_source, units,
};
use crate::error::Refusal;

pub const VERSION: &str = "arc-execution-context.v2";
const MAX_TARGETS: usize = 16;
const MAX_HISTORY: usize = 4096;
const MAX_GAP_S: f64 = 7.0 * 24.0 * 3600.0;
const MAX_TTL_S: f64 = 24.0 * 3600.0;
const MAX_COUNT: u64 = 100_000_000;
const MAX_ATTEMPT: u64 = 64;

const EVENT_KINDS: &[&str] = &[
    "task_start",
    "user_message",
    "tool_result",
    "compaction",
    "subagent_start",
    "sidecall",
];
const EVENT_BASES: &[&str] = &["explicit_client", "marker_inferred", "unknown"];
const PROVENANCE: &[&str] = &["provider_reported", "local_measured", "caller_estimated"];
const CAPACITY_PROVENANCE: &[&str] = &[
    "provider_reported",
    "local_measured",
    "caller_bounded",
    "caller_estimated",
];
const CAPACITY_REASONS: &[&str] = &["context_capacity", "runtime_unavailable"];

/// Retry identity plus the epoch. Echoed in the answer exactly as sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Turn {
    pub id: String,
    pub attempt: u64,
    pub epoch: String,
    pub retry_of: Option<String>,
    pub reconsult: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Compaction {
    pub summary_message: Option<u64>,
    pub retained_from: Option<u64>,
    pub new: bool,
}

/// What just happened, as the executor infers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub kind: Option<String>,
    pub basis: String,
    pub compaction: Option<Compaction>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastServed {
    pub action_id: String,
    pub worker: String,
    pub message: Option<u64>,
}

/// Which executed action produced one assistant message of the request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attribution {
    pub message: u64,
    pub action_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Expect {
    pub package_sha256: Option<String>,
    pub thinking_controls_sha256: Option<String>,
    pub offer_sha256: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Ttl {
    pub seconds: f64,
    pub source: String,
    pub basis: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Residency {
    pub state: String,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub units: String,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefix {
    pub tokens: u64,
    pub overlap_tokens: Option<u64>,
    pub units: String,
    pub provenance: String,
}

/// Inference-cache evidence for one target.
#[derive(Debug, Clone, PartialEq)]
pub struct Cache {
    pub context_epoch: Option<String>,
    pub gap_s: Option<f64>,
    pub ttl: Option<Ttl>,
    pub residency: Option<Residency>,
    pub usage: Option<Usage>,
    pub current_prefix: Option<Prefix>,
}

/// This request's capacity verdict on one target. `fits: None` is unknown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capacity {
    pub fits: Option<bool>,
    pub reason: Option<String>,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub action_id: String,
    pub worker: String,
    pub placement: String,
    pub cache: Option<Cache>,
    pub capacity: Option<Capacity>,
}

/// A validated `routing.execution` version 2.
#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionContext {
    pub turn: Turn,
    pub event: Event,
    pub last_served: Option<LastServed>,
    pub history: Vec<Attribution>,
    /// The only action the executor can run this turn, when it holds one.
    pub hold: Option<String>,
    pub expect: Option<Expect>,
    pub targets: Vec<Target>,
}

fn turn(value: &Value, at: &str) -> Checked<Turn> {
    let map = object(
        value,
        at,
        &["id", "attempt", "epoch", "retry_of", "reconsult"],
        &[],
    )?;
    let turn = Turn {
        id: opaque(member(map, "id"), &join(at, "id"))?,
        attempt: integer(member(map, "attempt"), &join(at, "attempt"), 1, MAX_ATTEMPT)?,
        epoch: opaque(member(map, "epoch"), &join(at, "epoch"))?,
        retry_of: nullable(member(map, "retry_of"), |v| {
            opaque(v, &join(at, "retry_of"))
        })?,
        reconsult: nullable(member(map, "reconsult"), |v| {
            one_of(v, &join(at, "reconsult"), &["capacity", "veto"])
        })?,
    };
    if turn.reconsult.is_some() && turn.retry_of.is_none() {
        return bad(at, "a re-consult names the route it retries");
    }
    Ok(turn)
}

fn index(value: &Value, at: &str) -> Checked<u64> {
    integer(value, at, 0, u64::MAX)
}

fn event(value: &Value, at: &str) -> Checked<Event> {
    let map = object(value, at, &["kind", "basis", "compaction"], &[])?;
    let event = Event {
        kind: nullable(member(map, "kind"), |v| {
            one_of(v, &join(at, "kind"), EVENT_KINDS)
        })?,
        basis: one_of(member(map, "basis"), &join(at, "basis"), EVENT_BASES)?,
        compaction: nullable(member(map, "compaction"), |v| {
            let at = join(at, "compaction");
            let map = object(v, &at, &["summary_message", "retained_from", "new"], &[])?;
            Ok(Compaction {
                summary_message: nullable(member(map, "summary_message"), |v| {
                    index(v, &join(&at, "summary_message"))
                })?,
                retained_from: nullable(member(map, "retained_from"), |v| {
                    index(v, &join(&at, "retained_from"))
                })?,
                new: boolean(member(map, "new"), &join(&at, "new"))?,
            })
        })?,
    };
    if (event.basis == "unknown") != event.kind.is_none() {
        return bad(at, "an unknown event has no kind, and a known one has one");
    }
    if (event.kind.as_deref() == Some("compaction")) != event.compaction.is_some() {
        return bad(
            at,
            "a compaction event carries its compaction block, and only it",
        );
    }
    Ok(event)
}

fn count(map: &Map<String, Value>, at: &str, key: &str) -> Checked<Option<u64>> {
    nullable(member(map, key), |v| {
        integer(v, &join(at, key), 0, MAX_COUNT)
    })
}

fn cache(value: &Value, at: &str) -> Checked<Cache> {
    let map = object(
        value,
        at,
        &[
            "context_epoch",
            "gap_s",
            "ttl",
            "residency",
            "usage",
            "current_prefix",
        ],
        &[],
    )?;
    Ok(Cache {
        context_epoch: nullable(member(map, "context_epoch"), |v| {
            opaque(v, &join(at, "context_epoch"))
        })?,
        gap_s: nullable(member(map, "gap_s"), |v| {
            float(v, &join(at, "gap_s"), 0.0, MAX_GAP_S, false)
        })?,
        ttl: nullable(member(map, "ttl"), |v| {
            let at = join(at, "ttl");
            let map = object(v, &at, &["seconds", "source", "basis"], &[])?;
            Ok(Ttl {
                seconds: float(
                    member(map, "seconds"),
                    &join(&at, "seconds"),
                    0.0,
                    MAX_TTL_S,
                    true,
                )?,
                source: ttl_source(member(map, "source"), &join(&at, "source"))?,
                basis: one_of(
                    member(map, "basis"),
                    &join(&at, "basis"),
                    &["wire_declared", "provider_documented", "caller_estimated"],
                )?,
            })
        })?,
        residency: nullable(member(map, "residency"), |v| {
            let at = join(at, "residency");
            let map = object(v, &at, &["state", "provenance"], &[])?;
            Ok(Residency {
                state: one_of(
                    member(map, "state"),
                    &join(&at, "state"),
                    &["resident", "evicted"],
                )?,
                provenance: one_of(
                    member(map, "provenance"),
                    &join(&at, "provenance"),
                    PROVENANCE,
                )?,
            })
        })?,
        usage: nullable(member(map, "usage"), |v| {
            let at = join(at, "usage");
            let map = object(
                v,
                &at,
                &[
                    "prompt_tokens",
                    "cache_read_tokens",
                    "cache_write_tokens",
                    "units",
                    "provenance",
                ],
                &[],
            )?;
            Ok(Usage {
                prompt_tokens: count(map, &at, "prompt_tokens")?,
                cache_read_tokens: count(map, &at, "cache_read_tokens")?,
                cache_write_tokens: count(map, &at, "cache_write_tokens")?,
                units: units(member(map, "units"), &join(&at, "units"))?,
                provenance: one_of(
                    member(map, "provenance"),
                    &join(&at, "provenance"),
                    PROVENANCE,
                )?,
            })
        })?,
        current_prefix: nullable(member(map, "current_prefix"), |v| {
            let at = join(at, "current_prefix");
            let map = object(
                v,
                &at,
                &["tokens", "overlap_tokens", "units", "provenance"],
                &[],
            )?;
            Ok(Prefix {
                tokens: integer(member(map, "tokens"), &join(&at, "tokens"), 1, MAX_COUNT)?,
                overlap_tokens: count(map, &at, "overlap_tokens")?,
                units: units(member(map, "units"), &join(&at, "units"))?,
                provenance: one_of(
                    member(map, "provenance"),
                    &join(&at, "provenance"),
                    PROVENANCE,
                )?,
            })
        })?,
    })
}

fn capacity(value: &Value, at: &str) -> Checked<Capacity> {
    let map = object(
        value,
        at,
        &[
            "fits",
            "reason",
            "required",
            "available",
            "available_source",
            "units",
            "provenance",
        ],
        &[],
    )?;
    count(map, at, "required")?;
    count(map, at, "available")?;
    nullable(member(map, "available_source"), |v| {
        one_of(
            v,
            &join(at, "available_source"),
            &["configured", "runtime_observed"],
        )
    })?;
    nullable(member(map, "units"), |v| units(v, &join(at, "units")))?;
    let capacity = Capacity {
        fits: nullable(member(map, "fits"), |v| boolean(v, &join(at, "fits")))?,
        reason: nullable(member(map, "reason"), |v| {
            one_of(v, &join(at, "reason"), CAPACITY_REASONS)
        })?,
        provenance: one_of(
            member(map, "provenance"),
            &join(at, "provenance"),
            CAPACITY_PROVENANCE,
        )?,
    };
    if (capacity.fits == Some(false)) != capacity.reason.is_some() {
        return bad(
            at,
            "a target that does not fit says why, and only it gives a reason",
        );
    }
    Ok(capacity)
}

fn target(value: &Value, at: &str) -> Checked<Target> {
    let map = object(
        value,
        at,
        &["action_id", "worker", "placement", "cache", "capacity"],
        &[],
    )?;
    Ok(Target {
        action_id: sha256(member(map, "action_id"), &join(at, "action_id"))?,
        worker: opaque(member(map, "worker"), &join(at, "worker"))?,
        placement: one_of(
            member(map, "placement"),
            &join(at, "placement"),
            &["cloud", "local"],
        )?,
        cache: nullable(member(map, "cache"), |v| cache(v, &join(at, "cache")))?,
        capacity: nullable(member(map, "capacity"), |v| {
            capacity(v, &join(at, "capacity"))
        })?,
    })
}

fn envelope(value: &Value) -> Checked<ExecutionContext> {
    let map = object(
        value,
        "",
        &[
            "version",
            "turn",
            "event",
            "last_served",
            "history",
            "hold",
            "expect",
            "targets",
        ],
        &[],
    )?;
    one_of(member(map, "version"), "version", &[VERSION])?;
    let mut history = Vec::new();
    for (i, entry) in array(member(map, "history"), "history", MAX_HISTORY)?
        .iter()
        .enumerate()
    {
        let at = format!("history.{i}");
        let map = object(entry, &at, &["message", "action_id"], &[])?;
        history.push(Attribution {
            message: index(member(map, "message"), &join(&at, "message"))?,
            action_id: sha256(member(map, "action_id"), &join(&at, "action_id"))?,
        });
    }
    let mut targets = Vec::new();
    for (i, entry) in array(member(map, "targets"), "targets", MAX_TARGETS)?
        .iter()
        .enumerate()
    {
        targets.push(target(entry, &format!("targets.{i}"))?);
    }
    let context = ExecutionContext {
        turn: turn(member(map, "turn"), "turn")?,
        event: event(member(map, "event"), "event")?,
        last_served: nullable(member(map, "last_served"), |v| {
            let map = object(v, "last_served", &["action_id", "worker", "message"], &[])?;
            Ok(LastServed {
                action_id: sha256(member(map, "action_id"), "last_served.action_id")?,
                worker: opaque(member(map, "worker"), "last_served.worker")?,
                message: nullable(member(map, "message"), |v| index(v, "last_served.message"))?,
            })
        })?,
        history,
        hold: nullable(member(map, "hold"), |v| {
            let map = object(v, "hold", &["scope", "action_id"], &[])?;
            one_of(member(map, "scope"), "hold.scope", &["action"])?;
            sha256(member(map, "action_id"), "hold.action_id")
        })?,
        expect: nullable(member(map, "expect"), |v| {
            let keys = ["package_sha256", "thinking_controls_sha256", "offer_sha256"];
            let map = object(v, "expect", &keys, &[])?;
            let pin = |key: &str| nullable(member(map, key), |v| sha256(v, &join("expect", key)));
            Ok(Expect {
                package_sha256: pin(keys[0])?,
                thinking_controls_sha256: pin(keys[1])?,
                offer_sha256: pin(keys[2])?,
            })
        })?,
        targets,
    };
    let messages: BTreeSet<u64> = context.history.iter().map(|h| h.message).collect();
    if messages.len() != context.history.len() {
        return bad("", "an assistant message is attributed once");
    }
    let actions: BTreeSet<&str> = context
        .targets
        .iter()
        .map(|t| t.action_id.as_str())
        .collect();
    if actions.len() != context.targets.len() {
        return bad("", "a target is stated once per action");
    }
    Ok(context)
}

/// The wire format a body is read as when no header names one.
pub(crate) fn is_responses(body: &Map<String, Value>) -> bool {
    body.contains_key("input") && !body.contains_key("messages")
}

/// Whether a request message or item can carry attribution: an assistant
/// message, or in Responses any assistant-output item.
fn attributable(responses: bool, item: &Map<String, Value>) -> bool {
    let assistant = item.get("role").and_then(Value::as_str) == Some("assistant");
    if !responses {
        return assistant;
    }
    match item.get("type") {
        Some(Value::String(kind)) if kind == "function_call" || kind == "reasoning" => true,
        Some(Value::String(kind)) => kind == "message" && assistant,
        Some(_) => false,
        None => assistant,
    }
}

/// The validated member, or the refusal: shape and bounds, the counts, then
/// history, `last_served` and a compaction's indices against the body's own
/// messages.
pub fn parse_execution(
    value: &Value,
    body: &Map<String, Value>,
) -> Result<ExecutionContext, Refusal> {
    let context = envelope(value).map_err(|Bad { at, why }| {
        let at = if at.is_empty() {
            "routing.execution"
        } else {
            &at
        };
        Refusal::new(
            "execution_invalid",
            format!("routing.execution is malformed at {at}: {why}."),
        )
    })?;
    for target in &context.targets {
        let Some(cache) = &target.cache else { continue };
        if let Some(usage) = &cache.usage {
            let cached =
                usage.cache_read_tokens.unwrap_or(0) + usage.cache_write_tokens.unwrap_or(0);
            if usage.prompt_tokens.is_some_and(|prompt| cached > prompt) {
                return Err(Refusal::new(
                    "execution_counts_inconsistent",
                    format!(
                        "routing.execution target {:?}: the cache read and write tokens exceed the prompt they are part of.",
                        target.worker
                    ),
                ));
            }
        }
        if let Some(prefix) = &cache.current_prefix {
            if prefix
                .overlap_tokens
                .is_some_and(|overlap| overlap > prefix.tokens)
            {
                return Err(Refusal::new(
                    "execution_counts_inconsistent",
                    format!(
                        "routing.execution target {:?}: the overlap exceeds the current prefix it is part of.",
                        target.worker
                    ),
                ));
            }
        }
    }
    let responses = is_responses(body);
    let empty = Vec::new();
    let messages = body
        .get(if responses { "input" } else { "messages" })
        .and_then(Value::as_array)
        .unwrap_or(&empty);
    let at = |index: u64| usize::try_from(index).ok().and_then(|i| messages.get(i));
    let named = context
        .history
        .iter()
        .map(|h| h.message)
        .chain(context.last_served.as_ref().and_then(|s| s.message));
    for index in named {
        let assistant = at(index)
            .and_then(Value::as_object)
            .is_some_and(|item| attributable(responses, item));
        if !assistant {
            return Err(Refusal::new(
                "execution_attribution_invalid",
                format!(
                    "routing.execution names message {index}, which is not an assistant message of this request."
                ),
            ));
        }
    }
    if let Some(compaction) = &context.event.compaction {
        let past = |index: Option<u64>| index.is_some_and(|i| at(i).is_none());
        if past(compaction.summary_message) || past(compaction.retained_from) {
            return Err(Refusal::new(
                "execution_invalid",
                "routing.execution.event.compaction indexes past the request.",
            ));
        }
    }
    Ok(context)
}

/// Whether the caller pinned an identity and it equals the serving one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PinStatus {
    Matched,
    Unpinned,
}

/// The package and controls pins, checked before the decide.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pinned {
    pub package_sha256: PinStatus,
    pub thinking_controls_sha256: PinStatus,
}

fn pin(name: &str, stated: Option<&String>, serving: Option<&str>) -> Result<PinStatus, Refusal> {
    match stated {
        None => Ok(PinStatus::Unpinned),
        Some(stated) if Some(stated.as_str()) == serving => Ok(PinStatus::Matched),
        Some(stated) => Err(Refusal::new(
            "execution_identity_mismatch",
            format!(
                "routing.execution.expect.{name} is {stated}; the serving value is {}.",
                serving.unwrap_or("null")
            ),
        )),
    }
}

/// The member against the serving package, in this order: every named action
/// in the catalog; the package and controls pins; `last_served` consistent
/// with `history`; the hold feasible, then offered.
pub fn bind(context: &ExecutionContext, catalog: &Catalog) -> Result<Pinned, Refusal> {
    let named = context
        .history
        .iter()
        .map(|h| &h.action_id)
        .chain(context.targets.iter().map(|t| &t.action_id))
        .chain(context.last_served.iter().map(|s| &s.action_id))
        .chain(context.hold.iter());
    let unknown: BTreeSet<&str> = named
        .filter(|id| !catalog.actions.contains_key(*id))
        .map(String::as_str)
        .collect();
    if !unknown.is_empty() {
        let unknown: Vec<&str> = unknown.into_iter().collect();
        return Err(Refusal::new(
            "execution_unknown_action",
            format!(
                "routing.execution names action(s) outside the serving package: {}.",
                unknown.join(", ")
            ),
        ));
    }
    let expect = context.expect.as_ref();
    let pinned = Pinned {
        package_sha256: pin(
            "package_sha256",
            expect.and_then(|e| e.package_sha256.as_ref()),
            Some(&catalog.package_sha256),
        )?,
        thinking_controls_sha256: pin(
            "thinking_controls_sha256",
            expect.and_then(|e| e.thinking_controls_sha256.as_ref()),
            catalog.thinking_controls_sha256.as_deref(),
        )?,
    };
    let model = |id: &str| catalog.actions.get(id).map(|a| a.model.as_str());
    if let (Some(served), Some(last)) = (
        &context.last_served,
        context.history.iter().max_by_key(|h| h.message),
    ) {
        let same = served
            .message
            .and_then(|m| context.history.iter().find(|h| h.message == m));
        let conflict = match same {
            Some(same) => same.action_id != served.action_id,
            None => model(&last.action_id) != model(&served.action_id),
        };
        if conflict {
            return Err(Refusal::new(
                "execution_last_served_conflict",
                "routing.execution.last_served contradicts the history it should agree with.",
            ));
        }
    }
    if let Some(hold) = &context.hold {
        let held = context.targets.iter().find(|t| &t.action_id == hold);
        if held
            .and_then(|t| t.capacity.as_ref())
            .is_some_and(|c| c.fits == Some(false))
        {
            return Err(Refusal::new(
                "execution_hold_infeasible",
                format!(
                    "routing.execution.hold action {hold} is marked not to fit by the request's own capacity facts."
                ),
            ));
        }
        if held.is_none() {
            return Err(Refusal::new(
                "execution_hold_unavailable",
                format!("routing.execution.hold action {hold} is not offered to this request."),
            ));
        }
    }
    Ok(pinned)
}

/// One target's cache evidence after admission, as the decide request reads it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ActionCache {
    pub action_id: String,
    pub gap_s: Option<f64>,
    pub ttl_s: Option<f64>,
    pub ttl_source: Option<String>,
    pub prompt_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_epoch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_prefix_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlap_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub residency: Option<String>,
}

/// One target's capacity verdict after admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CapacityFact {
    pub action_id: String,
    pub placement: String,
    pub fits: Option<bool>,
    pub reason: Option<String>,
}

/// Admission of cache evidence. A value not admitted is sent unknown, never
/// relabelled: an estimated TTL, usage or prefix; a residency not measured
/// locally on a local target; a usage whose units differ from the prefix's.
pub fn admit_cache(context: &ExecutionContext) -> Vec<ActionCache> {
    let admitted = |provenance: &str| provenance != "caller_estimated";
    let mut entries = Vec::new();
    for target in &context.targets {
        let Some(cache) = &target.cache else { continue };
        let ttl = cache
            .ttl
            .as_ref()
            .filter(|ttl| ttl.basis != "caller_estimated");
        let residency = cache
            .residency
            .as_ref()
            .filter(|r| r.provenance == "local_measured" && target.placement == "local");
        let prefix = cache
            .current_prefix
            .as_ref()
            .filter(|p| admitted(&p.provenance));
        let usage = cache
            .usage
            .as_ref()
            .filter(|u| admitted(&u.provenance) && prefix.is_none_or(|p| p.units == u.units));
        entries.push(ActionCache {
            action_id: target.action_id.clone(),
            gap_s: cache.gap_s,
            ttl_s: ttl.map(|t| t.seconds),
            ttl_source: ttl.map(|t| t.source.clone()),
            prompt_tokens: usage.and_then(|u| u.prompt_tokens),
            cache_read_tokens: usage.and_then(|u| u.cache_read_tokens),
            cache_write_tokens: usage.and_then(|u| u.cache_write_tokens),
            context_epoch: cache.context_epoch.clone(),
            current_prefix_tokens: prefix.map(|p| p.tokens),
            overlap_tokens: prefix.and_then(|p| p.overlap_tokens),
            residency: residency.map(|r| r.state.clone()),
        });
    }
    entries
}

/// Every target as one capacity entry. `fits: true` is admitted only from a
/// measured, reported or bounded size, else it is unknown; `fits: false` is
/// admitted from any provenance.
pub fn admit_capacity(context: &ExecutionContext) -> Vec<CapacityFact> {
    context
        .targets
        .iter()
        .map(|target| {
            let fact = target.capacity.as_ref();
            let fits = fact.and_then(|fact| match fact.fits {
                Some(true) if fact.provenance == "caller_estimated" => None,
                fits => fits,
            });
            CapacityFact {
                action_id: target.action_id.clone(),
                placement: target.placement.clone(),
                fits,
                reason: fact.and_then(|fact| fact.reason.clone()),
            }
        })
        .collect()
}
