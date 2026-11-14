//! The decide request and response, in the JSON shapes the ARC policy
//! service uses. Members this crate does not read are ignored on input.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

use crate::release::Thinking;

#[derive(Debug, Clone, Deserialize)]
pub struct DecideInput {
    /// Names the caller's session to the readout source; decisions do not
    /// depend on it.
    #[serde(default, alias = "episode_id_hash")]
    pub episode: Option<String>,
    #[serde(default)]
    pub context_epoch: Value,
    /// `anthropic_messages` (the default) is the one format decided.
    #[serde(default)]
    pub request_format: Option<String>,
    /// The model request (`system`, `tools`, `messages`), kept as the text
    /// sent: the encoder input depends on the key order of the system prompt
    /// and the tool definitions.
    pub request: Box<RawValue>,
    /// Which action (and arm) produced each attributed assistant message.
    #[serde(default)]
    pub attribution: Vec<Attribution>,
    pub selection: Selection,
    #[serde(default)]
    pub policy: Option<Policy>,
    #[serde(default)]
    pub event: Option<Event>,
    #[serde(default)]
    pub capacity: Vec<Capacity>,
    #[serde(default)]
    pub turn_key: Option<String>,
    #[serde(default)]
    pub reconsult: Option<String>,
    #[serde(default)]
    pub cache_context: Option<CacheContext>,
    #[serde(default)]
    pub routing_prices: Option<RoutingPrices>,
    /// The model the session holds when the request itself names none (no
    /// attributed message, no last-served action): what a session's memory
    /// would supply. This crate keeps no state between calls.
    #[serde(default)]
    pub stage_one_held: Option<HeldModel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Attribution {
    pub message: usize,
    pub action_id: String,
    #[serde(default)]
    pub arm_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Selection {
    pub available_action_ids: Vec<String>,
    #[serde(default)]
    pub operating_point: Option<OperatingPoint>,
    #[serde(default)]
    pub tier_points: Option<Vec<TierPoint>>,
    #[serde(default)]
    pub held_action_id: Option<String>,
    #[serde(default)]
    pub last_served_action_id: Option<String>,
    #[serde(default)]
    pub attested_held_action_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OperatingPoint {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cost_weight: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TierPoint {
    pub name: String,
    pub cost_weight: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Policy {
    pub preset: String,
    #[serde(default)]
    pub overrides: Option<PolicyOverrides>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct PolicyOverrides {
    #[serde(default)]
    pub schedule: Option<ScheduleOverride>,
    #[serde(default)]
    pub selection: Option<SelectionOverride>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScheduleOverride {
    #[serde(default)]
    pub model: Option<Vec<String>>,
    #[serde(default)]
    pub thinking: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SelectionOverride {
    #[serde(default)]
    pub min_p_pass: Option<f64>,
    #[serde(default)]
    pub max_rest_tokens: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Event {
    #[serde(default)]
    pub kind: Option<String>,
    pub basis: String,
    #[serde(default)]
    pub compaction: Option<Compaction>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Compaction {
    #[serde(default)]
    pub new: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Capacity {
    pub action_id: String,
    pub placement: String,
    #[serde(default)]
    pub fits: Option<bool>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CacheContext {
    #[serde(default)]
    pub actions: Vec<CacheEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CacheEntry {
    pub action_id: String,
    #[serde(default)]
    pub gap_s: Option<f64>,
    #[serde(default)]
    pub ttl_s: Option<f64>,
    #[serde(default)]
    pub residency: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoutingPrices {
    /// `caller` for a caller's own prices; absent for a price feed.
    #[serde(default)]
    pub basis: Option<String>,
    #[serde(default)]
    pub exclusive: Option<bool>,
    #[serde(default)]
    pub as_of: Option<String>,
    /// Keyed by a model's price id, or `<id>@<endpoint>`.
    pub rates: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct HeldModel {
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecideOutput {
    pub decision: Decision,
    pub actions: Vec<ActionScore>,
    pub encoding: Encoding,
    /// What stage one predicted, when it decided a boundary on this request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage_one: Option<StageOneEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Encoding {
    pub token_count: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Decision {
    pub selected_action_id: String,
    pub selected_arm_id: String,
    /// `supported_prediction`, or `scheduled_model_hold` when the caller's
    /// held action kept its arm.
    pub reason: &'static str,
    pub operating_point: PointEcho,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier_picks: Option<BTreeMap<String, TierPick>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<Receipt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PointEcho {
    pub name: String,
    pub cost_weight: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct TierPick {
    pub selected_action_id: String,
    pub selected_arm_id: String,
    pub model: String,
    pub thinking: Thinking,
}

#[derive(Debug, Clone, Serialize)]
pub struct Receipt {
    pub action_id: String,
    pub control_id: Option<String>,
    pub policy: Option<String>,
    pub decided_by: DecidedBy,
    pub eligible: Eligible,
    pub held_from: Option<String>,
    pub excluded: Vec<Excluded>,
    pub capacity_unknown: Vec<String>,
    pub basis: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selection: Option<SelectionRecord>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecidedBy {
    pub model: Option<&'static str>,
    pub thinking: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Eligible {
    pub model: Option<&'static str>,
    pub thinking: Option<&'static str>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Excluded {
    pub action_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SelectionRecord {
    pub rule: &'static str,
    pub min_p_pass: f64,
    pub max_rest_tokens: Option<u64>,
    pub local: Vec<LocalCandidate>,
    pub outcome: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct LocalCandidate {
    pub action_id: String,
    pub p_pass: Option<f64>,
    pub rest_tokens: Option<i64>,
    pub eligible: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActionScore {
    pub action_id: String,
    pub available: bool,
    /// Whether one of its arms could be chosen on this request.
    pub supported: bool,
    pub selected: bool,
    pub score: f32,
    pub predicted_quality: Option<f32>,
    pub predicted_cost_usd: Option<f32>,
    pub price_per_mtok: PricePerMtok,
    pub thinking: Thinking,
}

#[derive(Debug, Clone, Serialize)]
pub struct PricePerMtok {
    pub input: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StageOneEvidence {
    /// `task_start`, `subagent_start`, `compaction` or `user_message`.
    pub boundary: &'static str,
    pub cost_weight: f64,
    pub chosen: String,
    pub candidates: Vec<CandidateEvidence>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CandidateEvidence {
    pub model: String,
    pub offered: bool,
    pub p_pass: Option<f64>,
    pub predicted_usage: [Option<f64>; 4],
    pub predicted_cost_usd: Option<f64>,
    pub score: Option<f64>,
}
