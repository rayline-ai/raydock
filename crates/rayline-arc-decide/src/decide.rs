//! One decision: stage one chooses the model at a boundary and holds it
//! otherwise; stage two chooses the thinking control among that model's
//! offered arms.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rayline_arc_input::project_request;
use serde_json::Value;

use crate::error::{DecideError, invalid, refuse, unsupported};
use crate::prices::{self, ResolvedPrices};
use crate::release::{Release, ToolFree, Withheld};
use crate::stage_one::{LEGS, Prediction};
use crate::stage_two::{self, ArmScore};
use crate::wire::{
    ActionScore, CandidateEvidence, DecideInput, DecideOutput, DecidedBy, Decision, Eligible,
    Encoding, Event, Excluded, LocalCandidate, PointEcho, PricePerMtok, Receipt, SelectionRecord,
    StageOneEvidence, TierPick,
};

const PRESET_PACKAGE_DEFAULT: &str = "arc-policy.package-default.v1";
const PRESET_HYBRID_DEFAULT: &str = "arc-policy.hybrid-default.v1";
const PRESET_PREFER_LOCAL: &str = "arc-policy.prefer-local.v1";
/// prefer-local's default threshold on the local model's P(pass).
const PREFER_LOCAL_MIN_P_PASS: f64 = 0.65;
/// The switch-point classes, most specific first.
const SCHEDULE_CLASSES: [&str; 3] = ["cache_boundaries", "user_messages", "all_turns"];
const EVENT_KINDS: [&str; 6] = [
    "task_start",
    "user_message",
    "tool_result",
    "compaction",
    "subagent_start",
    "sidecall",
];
const MAX_TIER_POINTS: usize = 8;

/// What the encoder is asked for one request.
#[derive(Debug, Clone, Copy)]
pub struct ReadoutRequest<'a> {
    /// The caller's session, for a source that reuses a session's prefix.
    pub episode: Option<&'a str>,
    pub tokens: &'a [u32],
    /// The index of each block's last token.
    pub offsets: &'a [usize],
}

/// Where readouts come from: the encoder's final-layer state at each
/// requested token offset, one row per offset, each the encoder's width.
pub trait Readouts {
    fn readouts(&self, request: &ReadoutRequest<'_>) -> Result<Vec<Vec<f32>>, String>;
}

#[derive(Debug, Clone, PartialEq)]
struct Schedule {
    /// `None`: stage one's own boundaries.
    model: Option<Vec<String>>,
    thinking: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SelectionRule {
    min_p_pass: f64,
    max_rest_tokens: Option<u64>,
}

/// A request's cost weight for stage one.
#[derive(Debug, Clone, PartialEq)]
struct Point {
    name: String,
    cost_weight: f64,
}

/// The terms a tool-free request decides a start boundary under.
#[derive(Debug, Clone, PartialEq)]
struct ToolFreeTerms {
    cost_weight: f64,
    excluded: Vec<usize>,
    latency_weight: f64,
}

/// Stage one's settings for one request.
#[derive(Debug, Clone)]
struct OnePolicy {
    offered: Vec<bool>,
    log_latency: Option<Vec<f64>>,
    cost_weight: f64,
    latency_weight: f64,
}

struct Receipted {
    decided_by: DecidedBy,
    eligible: Eligible,
    basis: String,
    selection: Option<SelectionRecord>,
}

struct OneOutcome {
    arms: Vec<usize>,
    evidence: Option<StageOneEvidence>,
    receipt: Option<Receipted>,
}

/// What every stage-one call of one request shares.
struct OneContext<'a> {
    release: &'a Release,
    input: &'a DecideInput,
    predictions: &'a [Prediction],
    /// Each candidate's four rates, `NaN` unknown; `Err` for a candidate
    /// whose arms are priced differently.
    candidate_rates: &'a [Result<[f64; LEGS], ()>],
    arm_candidate: &'a [Option<usize>],
    /// Whether an assistant block follows the first user block.
    answered: bool,
    caller_holds: bool,
    last_served: Option<(usize, Option<usize>)>,
    schedule: Option<&'a Schedule>,
    infeasible: &'a BTreeMap<usize, String>,
    rule: Option<SelectionRule>,
    placements: &'a HashMap<usize, &'a str>,
    wants_receipt: bool,
}

fn known(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn class_matched(
    matched: &BTreeSet<&'static str>,
    allowed: Option<&[String]>,
) -> Option<&'static str> {
    SCHEDULE_CLASSES.into_iter().find(|class| {
        matched.contains(class) && allowed.is_none_or(|a| a.iter().any(|c| c == class))
    })
}

fn turn_classes(
    kind: Option<&str>,
    typed: bool,
    cold: impl FnOnce() -> bool,
) -> BTreeSet<&'static str> {
    let mut classes = BTreeSet::from(["all_turns"]);
    if typed {
        classes.insert("user_messages");
    }
    if matches!(kind, Some("task_start" | "subagent_start" | "compaction")) || (typed && cold()) {
        classes.insert("cache_boundaries");
    }
    classes
}

fn explicit(event: Option<&Event>) -> Option<(&str, &Event)> {
    let event = event?;
    match (&event.kind, event.basis.as_str()) {
        (Some(kind), basis) if basis != "unknown" => Some((kind.as_str(), event)),
        _ => None,
    }
}

impl OneContext<'_> {
    fn boundary(
        &self,
        starting: bool,
        subagent: bool,
        compaction: bool,
        user_message: bool,
    ) -> Option<&'static str> {
        let kind = if starting {
            if subagent {
                "subagent_start"
            } else {
                "task_start"
            }
        } else if compaction {
            "compaction"
        } else if user_message {
            "user_message"
        } else {
            return None;
        };
        self.release
            .settings
            .boundaries
            .iter()
            .any(|b| b == kind)
            .then_some(kind)
    }

    /// The one candidate `arms` name, whether or not this request offers it.
    fn held_candidate(&self, arms: &[usize]) -> Option<usize> {
        let first = self.arm_candidate[*arms.first()?]?;
        arms.iter()
            .all(|arm| self.arm_candidate[*arm] == Some(first))
            .then_some(first)
    }

    fn served_candidate(&self) -> Option<usize> {
        let (action, named) = self.last_served?;
        match named {
            Some(arm) => self.held_candidate(&[arm]),
            None => self.held_candidate(&self.release.actions[action].arms),
        }
    }

    /// The held candidate when capacity leaves it no offered arm.
    fn capacity_forced(&self, available: &[usize]) -> Option<usize> {
        if self.infeasible.is_empty() {
            return None;
        }
        let held = self.served_candidate()?;
        let ran_out = self
            .infeasible
            .keys()
            .any(|action| self.held_candidate(&self.release.actions[*action].arms) == Some(held));
        let still_served = available
            .iter()
            .any(|arm| self.arm_candidate[*arm] == Some(held));
        (ran_out && !still_served).then_some(held)
    }

    /// Whether the held model's cache is observed cold: past its TTL, or a
    /// local slot observed evicted. Missing evidence is not cold.
    fn cache_cold(&self, policy: &OnePolicy, held: usize) -> bool {
        let Some(context) = &self.input.cache_context else {
            return false;
        };
        let mine = context.actions.iter().filter(|entry| {
            self.release
                .action_index
                .get(&entry.action_id)
                .is_some_and(|action| {
                    let arms = &self.release.actions[*action].arms;
                    !arms.is_empty()
                        && arms.iter().all(|arm| {
                            self.arm_candidate[*arm] == Some(held) && policy.offered[held]
                        })
                })
        });
        let Some(entry) = mine.min_by(|a, b| {
            (a.gap_s.is_none(), a.gap_s.unwrap_or(0.0))
                .partial_cmp(&(b.gap_s.is_none(), b.gap_s.unwrap_or(0.0)))
                .unwrap_or(std::cmp::Ordering::Equal)
        }) else {
            return false;
        };
        match (entry.residency.as_deref(), entry.gap_s, entry.ttl_s) {
            (Some(residency), _, _) => residency == "evicted",
            (None, Some(gap), Some(ttl)) => gap >= ttl,
            _ => false,
        }
    }

    /// The request's policy: the release's, with its cost weight, tool-free
    /// terms and, where the release applies it, the latency exemption of
    /// models the caller runs locally.
    fn policy(&self, cost_weight: Option<f64>, available: &[usize]) -> OnePolicy {
        let release = self.release;
        let mut policy = OnePolicy {
            offered: (0..release.candidates.len())
                .map(|c| release.withheld[c].is_none() && !release.candidate_arms[c].is_empty())
                .collect(),
            log_latency: release.settings.log_latency.clone(),
            cost_weight: cost_weight.unwrap_or(release.settings.cost_weight),
            latency_weight: release.settings.latency_weight,
        };
        // A model the caller runs locally was never in the latency table's
        // comparison of cloud round trips: it carries no latency term, and a
        // model withheld only for having no factor becomes offered. Only a
        // model whose every offered arm is local is exempt. The rule is the
        // release's to turn on (`Release::with_local_placement_exemption`).
        if !release.local_placement_exempt
            || self.placements.is_empty()
            || policy.latency_weight == 0.0
        {
            return policy;
        }
        let Some(log_latency) = policy.log_latency.as_mut() else {
            return policy;
        };
        let mut placed: HashMap<usize, bool> = HashMap::new();
        for arm in available {
            if let Some(candidate) = self.arm_candidate[*arm] {
                let local = release.arms[*arm]
                    .action
                    .and_then(|action| self.placements.get(&action))
                    .is_some_and(|placement| *placement == "local");
                *placed.entry(candidate).or_insert(true) &= local;
            }
        }
        for (candidate, local) in placed {
            if !local {
                continue;
            }
            log_latency[candidate] = 0.0;
            let weighable = policy.cost_weight == 0.0
                || (release.usage_known[candidate]
                    && self.candidate_rates[candidate]
                        .is_ok_and(|r| r.iter().all(|r| r.is_finite())));
            if release.withheld[candidate] == Some(Withheld::NoLatencyFactor)
                && !release.unmapped[candidate]
                && weighable
            {
                policy.offered[candidate] = true;
            }
        }
        policy
    }

    /// Scores every candidate at a boundary and chooses: the best offered
    /// one at a start, the held one elsewhere.
    fn decide_boundary(
        &self,
        policy: &OnePolicy,
        kind: &'static str,
        held: Option<usize>,
    ) -> Result<(usize, StageOneEvidence), DecideError> {
        let release = self.release;
        let mut rows = Vec::with_capacity(release.candidates.len());
        let mut best: Option<(usize, f64)> = None;
        for (c, prediction) in self.predictions.iter().enumerate() {
            let cost = match self.candidate_rates[c] {
                Ok(rates) => {
                    prediction
                        .usage
                        .iter()
                        .zip(rates)
                        .map(|(tokens, rate)| tokens * rate)
                        .sum::<f64>()
                        / 1_000_000.0
                }
                Err(()) if policy.offered[c] => {
                    return Err(unsupported("stage_one_prices_inconsistent"));
                }
                Err(()) => f64::NAN,
            };
            let mut score = if !policy.offered[c] {
                f64::NAN
            } else if policy.cost_weight == 0.0 {
                prediction.p_pass
            } else {
                prediction.p_pass - policy.cost_weight * cost
            };
            if policy.latency_weight != 0.0 {
                let log_latency = policy.log_latency.as_ref().map_or(f64::NAN, |l| l[c]);
                score -= policy.latency_weight * log_latency;
            }
            // With no bootstrap draws and a stay margin, a held model stays.
            if held.is_some_and(|held| held != c) {
                score = f64::NAN;
            }
            if score.is_finite() && best.is_none_or(|(_, top)| score > top) {
                best = Some((c, score));
            }
            rows.push(CandidateEvidence {
                model: release.candidates[c].clone(),
                offered: policy.offered[c],
                p_pass: known(prediction.p_pass),
                predicted_usage: prediction.usage.map(known),
                predicted_cost_usd: known(cost),
                score: known(score),
            });
        }
        let (chosen, _) = best.ok_or(refuse("no_supported_selection"))?;
        Ok((
            chosen,
            StageOneEvidence {
                boundary: kind,
                cost_weight: policy.cost_weight,
                chosen: release.candidates[chosen].clone(),
                candidates: rows,
            },
        ))
    }

    /// prefer-local at a decided boundary: a local action whose model clears
    /// the thresholds is chosen (the highest P(pass)); otherwise the utility
    /// pick stands.
    fn prefer_local(
        &self,
        policy: &OnePolicy,
        greedy: usize,
        evidence: &StageOneEvidence,
        available: &[usize],
        rule: SelectionRule,
    ) -> (usize, Vec<usize>, SelectionRecord) {
        let release = self.release;
        let offered: HashSet<usize> = available.iter().copied().collect();
        let mut local: Vec<usize> = available
            .iter()
            .filter_map(|arm| release.arms[*arm].action)
            .filter(|action| self.placements.get(action).is_some_and(|p| *p == "local"))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        local.sort_by(|a, b| release.actions[*a].id.cmp(&release.actions[*b].id));
        let mut compared = Vec::new();
        let mut passing: Vec<(usize, f64, usize)> = Vec::new();
        for action in local {
            let mine: Vec<usize> = release.actions[action]
                .arms
                .iter()
                .copied()
                .filter(|arm| offered.contains(arm))
                .collect();
            let candidate = self
                .held_candidate(&mine)
                .filter(|candidate| policy.offered[*candidate]);
            let row = candidate.map(|c| &evidence.candidates[c]);
            let p_pass = row.and_then(|row| row.p_pass);
            let rest_tokens = row.and_then(|row| {
                let mut total = 0.0;
                for leg in row.predicted_usage {
                    total += leg?;
                }
                Some(total.round_ties_even() as i64)
            });
            let eligible = match (candidate, p_pass, row.and_then(|row| row.score)) {
                (Some(_), Some(p), Some(_)) => {
                    p >= rule.min_p_pass
                        && rule.max_rest_tokens.is_none_or(|budget| {
                            rest_tokens.is_some_and(|tokens| {
                                tokens <= i64::try_from(budget).unwrap_or(i64::MAX)
                            })
                        })
                }
                _ => false,
            };
            if let (true, Some(candidate), Some(p)) = (eligible, candidate, p_pass) {
                passing.push((candidate, p, action));
            }
            compared.push(LocalCandidate {
                action_id: release.actions[action].id.clone(),
                p_pass,
                rest_tokens,
                eligible,
            });
        }
        let record = |outcome| SelectionRecord {
            rule: "prefer_local",
            min_p_pass: rule.min_p_pass,
            max_rest_tokens: rule.max_rest_tokens,
            local: compared.clone(),
            outcome,
        };
        let mut best: Option<(usize, f64)> = None;
        for (candidate, p, _) in &passing {
            if best.is_none_or(|(top, top_p)| *p > top_p || (*p == top_p && *candidate < top)) {
                best = Some((*candidate, *p));
            }
        }
        match best {
            Some((best, _)) => {
                let served: HashSet<usize> = passing
                    .iter()
                    .filter(|(candidate, _, _)| *candidate == best)
                    .map(|(_, _, action)| *action)
                    .collect();
                let arms = available
                    .iter()
                    .copied()
                    .filter(|arm| {
                        release.arms[*arm]
                            .action
                            .is_some_and(|a| served.contains(&a))
                    })
                    .collect();
                (best, arms, record("prefer_local_threshold"))
            }
            None => (greedy, available.to_vec(), record("utility_fallback")),
        }
    }

    /// The thinking dimension: on a turn its schedule does not admit, the
    /// last-served action's control holds where its arms remain offered.
    fn thinking_hold(
        &self,
        arms: Vec<usize>,
        matched: &BTreeSet<&'static str>,
    ) -> (Vec<usize>, &'static str, Option<&'static str>) {
        let all_turns = ["all_turns".to_owned()];
        let classes = self
            .schedule
            .and_then(|schedule| schedule.thinking.as_deref())
            .unwrap_or(&all_turns);
        let eligible = class_matched(matched, Some(classes));
        let Some((action, named)) = self.last_served.filter(|_| eligible.is_none()) else {
            return (arms, "schedule", eligible);
        };
        let keep: Vec<usize> = arms
            .iter()
            .copied()
            .filter(|arm| match named {
                Some(named) => *arm == named,
                None => self.release.actions[action].arms.contains(arm),
            })
            .collect();
        if keep.is_empty() {
            (arms, "schedule", None)
        } else {
            (keep, "held", None)
        }
    }

    /// Stage one for one cost weight: the arms stage two may choose among.
    fn stage_one(
        &self,
        available: &[usize],
        cost_weight: Option<f64>,
        tool_free: Option<&ToolFreeTerms>,
    ) -> Result<OneOutcome, DecideError> {
        let release = self.release;
        let mut policy = self.policy(cost_weight, available);
        let event = explicit(self.input.event.as_ref());
        let basis =
            || event.map_or_else(|| "service_inferred".to_owned(), |(_, e)| e.basis.clone());
        let candidate_of = |arm: usize| self.arm_candidate[arm].filter(|c| policy.offered[*c]);
        let forced_from = if self.caller_holds {
            None
        } else {
            self.capacity_forced(available)
        };

        // The caller holds the model, or offers one model: nothing to decide.
        let models: HashSet<Option<&str>> = available
            .iter()
            .map(|arm| {
                release.arms[*arm]
                    .served
                    .then_some(release.arms[*arm].model.as_str())
            })
            .collect();
        if forced_from.is_none() && (self.caller_holds || models.len() == 1) {
            let mut receipt = None;
            if self.wants_receipt {
                let only: HashSet<Option<usize>> =
                    available.iter().map(|arm| candidate_of(*arm)).collect();
                let single = match only.iter().next() {
                    Some(Some(candidate)) if only.len() == 1 => Some(*candidate),
                    _ => None,
                };
                let last = self.served_candidate();
                let moved =
                    !self.caller_holds && last.is_some() && single.is_some() && single != last;
                let serving = last.or(single);
                let (kind, typed) = match event {
                    Some((kind, event)) => (
                        match kind {
                            "task_start" => Some("task_start"),
                            "subagent_start" => Some("subagent_start"),
                            "compaction" if event.compaction.as_ref().is_some_and(|c| c.new) => {
                                Some("compaction")
                            }
                            _ => None,
                        },
                        kind == "user_message",
                    ),
                    None => ((!self.answered).then_some("task_start"), false),
                };
                let matched = turn_classes(kind, typed, || {
                    self.schedule.is_some()
                        && serving.is_some_and(|held| self.cache_cold(&policy, held))
                });
                let (arms, thinking, thinking_class) =
                    self.thinking_hold(available.to_vec(), &matched);
                receipt = Some(Receipted {
                    decided_by: DecidedBy {
                        model: Some(if moved { "held_unavailable" } else { "held" }),
                        thinking,
                    },
                    eligible: Eligible {
                        model: None,
                        thinking: thinking_class,
                    },
                    basis: basis(),
                    selection: None,
                });
                return Ok(OneOutcome {
                    arms,
                    evidence: None,
                    receipt,
                });
            }
            return Ok(OneOutcome {
                arms: available.to_vec(),
                evidence: None,
                receipt,
            });
        }

        // The model the session holds, and whether this request offers it.
        let mut dropped_hold = None;
        let prior = match (self.last_served, &self.input.stage_one_held) {
            (Some(_), _) => self.served_candidate(),
            (None, Some(remembered)) => Some(
                release
                    .candidate_of_model(&remembered.model)
                    .filter(|_| remembered.effort.is_none())
                    .ok_or(refuse("stage_one_held_unknown"))?,
            ),
            (None, None) => None,
        };
        let mut held = prior.filter(|held| policy.offered[*held]);
        if prior.is_some() && held.is_none() {
            dropped_hold = prior;
        }
        if let (None, None, Some(attested), true) = (
            held,
            dropped_hold,
            self.input.selection.attested_held_action_id.as_ref(),
            self.answered,
        ) {
            let action = *release
                .action_index
                .get(attested)
                .ok_or(invalid("selection.attested_held_action_id"))?;
            let prior = self.held_candidate(&release.actions[action].arms);
            held = prior.filter(|held| policy.offered[*held]);
            if prior.is_some() && held.is_none() {
                dropped_hold = prior;
            }
        }
        let subagent = false;
        let mut typed = false;
        let mut kind = self.boundary(held.is_none(), subagent, false, false);
        let mut decided_model = dropped_hold.map(|_| "held_unavailable");
        if let Some(forced) = forced_from {
            kind = Some(
                self.boundary(true, subagent, false, false)
                    .ok_or(refuse("stage_one_model_unavailable"))?,
            );
            held = held.or(Some(forced));
            decided_model = Some("capacity_handoff");
        } else if let Some((event_kind, event)) = event {
            kind = if matches!(event_kind, "task_start" | "subagent_start") || held.is_none() {
                self.boundary(true, event_kind == "subagent_start", false, false)
            } else if event_kind == "compaction" && event.compaction.as_ref().is_some_and(|c| c.new)
            {
                self.boundary(false, subagent, true, false)
            } else if event_kind == "user_message" {
                self.boundary(false, subagent, false, true)
            } else {
                None
            };
            typed = event_kind == "user_message";
        }
        let matched = turn_classes(kind, typed, || {
            self.schedule.is_some() && held.is_some_and(|held| self.cache_cold(&policy, held))
        });
        let model_classes = self.schedule.and_then(|schedule| schedule.model.as_deref());
        let eligible_model = class_matched(&matched, model_classes);
        if forced_from.is_none() && model_classes.is_some() && held.is_some() {
            if !matches!(kind, None | Some("task_start" | "subagent_start"))
                && eligible_model.is_none()
            {
                kind = None;
            }
            if kind.is_none() && eligible_model == Some("cache_boundaries") {
                kind = self.boundary(true, subagent, false, false);
            }
        }
        if let (false, Some(candidate)) =
            (matches!(kind, Some("task_start" | "subagent_start")), held)
        {
            if !available
                .iter()
                .any(|arm| self.arm_candidate[*arm] == Some(candidate))
            {
                kind = Some(
                    self.boundary(true, subagent, false, false)
                        .ok_or(refuse("stage_one_model_unavailable"))?,
                );
                decided_model = Some("held_unavailable");
            }
        }

        let mut arms = available.to_vec();
        let mut selection = None;
        let mut evidence = None;
        let chosen = match kind {
            None => {
                decided_model = Some("held");
                held.ok_or(refuse("stage_one_held_unknown"))?
            }
            Some(kind) => {
                let starting = matches!(kind, "task_start" | "subagent_start");
                if let (Some(terms), true) = (tool_free, starting) {
                    if terms.latency_weight != 0.0 && policy.log_latency.is_none() {
                        return Err(unsupported("tool_free_override_latency_unsupported"));
                    }
                    for candidate in &terms.excluded {
                        policy.offered[*candidate] = false;
                    }
                    if !policy.offered.iter().any(|offered| *offered) {
                        return Err(refuse("tool_free_override_excludes_every_offered_model"));
                    }
                    policy.cost_weight = terms.cost_weight;
                    policy.latency_weight = terms.latency_weight;
                }
                let (mut chosen, mut record) =
                    self.decide_boundary(&policy, kind, if starting { None } else { held })?;
                if let Some(rule) = self.rule {
                    let (picked, narrowed, outcome) =
                        self.prefer_local(&policy, chosen, &record, &arms, rule);
                    if decided_model.is_none() {
                        decided_model = Some(outcome.outcome);
                    }
                    (chosen, arms, selection) = (picked, narrowed, Some(outcome));
                }
                // A pick no offered arm serves becomes the best one that is
                // offered, from the same scores.
                if !arms
                    .iter()
                    .any(|arm| self.arm_candidate[*arm] == Some(chosen))
                {
                    let mut best: Option<(usize, f64)> = None;
                    for (c, row) in record.candidates.iter().enumerate() {
                        let servable = arms.iter().any(|arm| self.arm_candidate[*arm] == Some(c));
                        if let (true, true, Some(score)) = (policy.offered[c], servable, row.score)
                        {
                            if best.is_none_or(|(_, top)| score > top) {
                                best = Some((c, score));
                            }
                        }
                    }
                    chosen = best.ok_or(refuse("stage_one_model_unavailable"))?.0;
                }
                record.chosen = release.candidates[chosen].clone();
                evidence = Some(record);
                decided_model = decided_model.or(Some("schedule"));
                chosen
            }
        };
        let narrowed: Vec<usize> = arms
            .into_iter()
            .filter(|arm| self.arm_candidate[*arm] == Some(chosen))
            .collect();
        if narrowed.is_empty() {
            return Err(refuse("stage_one_model_unavailable"));
        }
        if !self.wants_receipt {
            return Ok(OneOutcome {
                arms: narrowed,
                evidence,
                receipt: None,
            });
        }
        let (narrowed, thinking, thinking_class) = self.thinking_hold(narrowed, &matched);
        let by_schedule = matches!(
            decided_model,
            Some("schedule" | "prefer_local_threshold" | "utility_fallback")
        );
        Ok(OneOutcome {
            arms: narrowed,
            evidence,
            receipt: Some(Receipted {
                decided_by: DecidedBy {
                    model: decided_model,
                    thinking,
                },
                eligible: Eligible {
                    model: eligible_model.filter(|_| by_schedule),
                    thinking: thinking_class,
                },
                basis: basis(),
                selection,
            }),
        })
    }
}

/// A request's cost weight, checked against the release's bounds.
fn point_at(release: &Release, name: &str, cost_weight: f64) -> Result<Point, DecideError> {
    let (low, high) = release
        .settings
        .cost_weight_bounds
        .ok_or(refuse("operating_point_not_adjustable"))?;
    if !(low..=high).contains(&cost_weight) {
        return Err(refuse("operating_point_out_of_bounds"));
    }
    Ok(Point {
        name: name.to_owned(),
        cost_weight,
    })
}

/// The tool-free override a decision at `name` and `cost_weight` takes: at a
/// declared tier's own weight, on a request with no tool definitions.
fn tool_free(
    release: &Release,
    has_tools: bool,
    name: &str,
    cost_weight: Option<f64>,
) -> Option<ToolFreeTerms> {
    let ToolFree {
        v_multiplier,
        excluded_models,
        latency_weight,
    } = release.settings.tool_free.get(name)?;
    let preset = *release.settings.presets.get(name)?;
    if cost_weight != Some(preset) || has_tools {
        return None;
    }
    Some(ToolFreeTerms {
        cost_weight: preset / v_multiplier,
        excluded: excluded_models
            .iter()
            .filter_map(|model| release.candidate_of_model(model))
            .collect(),
        latency_weight: *latency_weight,
    })
}

fn schedule_of(
    release: &Release,
    input: &DecideInput,
) -> Result<(Option<Schedule>, Option<SelectionRule>), DecideError> {
    let Some(policy) = &input.policy else {
        return Ok((None, None));
    };
    let base = match policy.preset.as_str() {
        PRESET_PACKAGE_DEFAULT => None,
        PRESET_HYBRID_DEFAULT | PRESET_PREFER_LOCAL => Some(Schedule {
            model: Some(vec!["cache_boundaries".to_owned()]),
            thinking: Some(vec!["all_turns".to_owned()]),
        }),
        _ => return Err(invalid("policy")),
    };
    let overrides = policy.overrides.clone().unwrap_or_default();
    let prefer_local = policy.preset == PRESET_PREFER_LOCAL;
    if let Some(rule) = &overrides.selection {
        let p_ok = rule.min_p_pass.is_none_or(|p| (0.0..=1.0).contains(&p));
        if !prefer_local || !p_ok || rule.max_rest_tokens == Some(0) {
            return Err(invalid("policy"));
        }
    }
    let rule = prefer_local.then(|| {
        let rule = overrides.selection.clone().unwrap_or_default();
        SelectionRule {
            min_p_pass: rule.min_p_pass.unwrap_or(PREFER_LOCAL_MIN_P_PASS),
            max_rest_tokens: rule.max_rest_tokens,
        }
    });
    let stated = overrides
        .schedule
        .filter(|s| s.model.is_some() || s.thinking.is_some());
    for classes in stated
        .iter()
        .flat_map(|s| [&s.model, &s.thinking])
        .flatten()
    {
        let distinct: HashSet<&String> = classes.iter().collect();
        if distinct.len() != classes.len()
            || classes
                .iter()
                .any(|c| !SCHEDULE_CLASSES.contains(&c.as_str()))
        {
            return Err(invalid("policy"));
        }
    }
    let schedule = match (base, stated) {
        (None, None) => None,
        (base, stated) => {
            let base = base.unwrap_or(Schedule {
                model: None,
                thinking: Some(vec!["all_turns".to_owned()]),
            });
            let stated = stated.unwrap_or_default();
            Some(Schedule {
                model: stated.model.or(base.model),
                thinking: stated.thinking.or(base.thinking),
            })
        }
    };
    // Stage one decides only at its boundaries: a model schedule may name
    // cache boundaries, and user messages where the release decides there.
    let user_messages = release
        .settings
        .boundaries
        .iter()
        .any(|b| b == "user_message");
    if let Some(classes) = schedule.as_ref().and_then(|s| s.model.as_ref()) {
        if classes
            .iter()
            .any(|c| !(c == "cache_boundaries" || (c == "user_messages" && user_messages)))
        {
            return Err(refuse("policy_schedule_not_admitted"));
        }
    }
    Ok((schedule, rule))
}

fn validate(input: &DecideInput) -> Result<(), DecideError> {
    if let Some(event) = &input.event {
        let kind_ok = event
            .kind
            .as_deref()
            .is_none_or(|k| EVENT_KINDS.contains(&k));
        let basis_ok = matches!(
            event.basis.as_str(),
            "explicit_client" | "marker_inferred" | "unknown"
        );
        if !kind_ok || !basis_ok || (event.basis == "unknown") != event.kind.is_none() {
            return Err(invalid("event"));
        }
    }
    let mut seen = HashSet::new();
    for entry in &input.capacity {
        let reason_ok = matches!(
            entry.reason.as_deref(),
            None | Some("context_capacity" | "runtime_unavailable")
        );
        if !matches!(entry.placement.as_str(), "cloud" | "local")
            || !reason_ok
            || (entry.fits == Some(false)) != entry.reason.is_some()
            || !seen.insert(entry.action_id.as_str())
        {
            return Err(invalid("capacity"));
        }
    }
    if !matches!(input.reconsult.as_deref(), None | Some("capacity" | "veto")) {
        return Err(invalid("reconsult"));
    }
    let selection = &input.selection;
    if let Some(attested) = &selection.attested_held_action_id {
        if selection.held_action_id.is_some() || !selection.available_action_ids.contains(attested)
        {
            return Err(invalid("selection.attested_held_action_id"));
        }
    }
    let weight_ok = |weight: f64| weight.is_finite() && weight >= 0.0;
    if selection
        .operating_point
        .as_ref()
        .and_then(|point| point.cost_weight)
        .is_some_and(|weight| !weight_ok(weight))
    {
        return Err(invalid("selection.operating_point"));
    }
    if let Some(tiers) = &selection.tier_points {
        let names: HashSet<&str> = tiers.iter().map(|tier| tier.name.as_str()).collect();
        if tiers.is_empty()
            || tiers.len() > MAX_TIER_POINTS
            || names.len() != tiers.len()
            || tiers
                .iter()
                .any(|tier| tier.name.is_empty() || !weight_ok(tier.cost_weight))
        {
            return Err(invalid("selection.tier_points"));
        }
    }
    Ok(())
}

/// The best valid arm of `pool`, the first on a tie.
fn best_arm(pool: &[usize], valid: &[bool], scores: &[f32]) -> Option<usize> {
    let mut members: Vec<usize> = pool.iter().copied().filter(|arm| valid[*arm]).collect();
    members.sort_unstable();
    let mut best: Option<usize> = None;
    for arm in members {
        if best.is_none_or(|top| scores[arm] > scores[top]) {
            best = Some(arm);
        }
    }
    best
}

/// Decides one request: the action (a model at a thinking control) to serve
/// it with, each tier point's pick, and every action's score.
///
/// The call keeps no state: the model a session holds is read from the
/// request (its attributed messages, `selection.last_served_action_id`,
/// `selection.held_action_id`, or `stage_one_held`).
pub fn decide(
    release: &Release,
    input: &DecideInput,
    readouts: &dyn Readouts,
) -> Result<DecideOutput, DecideError> {
    if !matches!(
        input.request_format.as_deref(),
        None | Some("anthropic_messages")
    ) {
        return Err(unsupported("request_format_unsupported"));
    }
    validate(input)?;
    let selection = &input.selection;
    let point_name = selection
        .operating_point
        .as_ref()
        .and_then(|point| point.name.clone())
        .unwrap_or_else(|| "default".to_owned());
    let primary = selection
        .operating_point
        .as_ref()
        .and_then(|point| point.cost_weight)
        .map(|weight| point_at(release, &point_name, weight))
        .transpose()?;
    let tiers = selection
        .tier_points
        .iter()
        .flatten()
        .map(|tier| point_at(release, &tier.name, tier.cost_weight))
        .collect::<Result<Vec<_>, _>>()?;

    let action_of = |id: &str, reason| release.action_index.get(id).copied().ok_or(refuse(reason));
    let mut available_actions = selection
        .available_action_ids
        .iter()
        .map(|id| action_of(id, "action_not_in_catalog"))
        .collect::<Result<Vec<_>, _>>()?;
    let held_action = selection
        .held_action_id
        .as_deref()
        .map(|id| action_of(id, "held_action_unavailable"))
        .transpose()?;
    let (schedule, rule) = schedule_of(release, input)?;
    let capacity: Vec<(Option<usize>, &crate::wire::Capacity)> = input
        .capacity
        .iter()
        .map(|entry| (release.action_index.get(&entry.action_id).copied(), entry))
        .collect();
    let placements: HashMap<usize, &str> = capacity
        .iter()
        .filter_map(|(action, entry)| Some(((*action)?, entry.placement.as_str())))
        .collect();
    if input.turn_key.is_some() {
        // The capacity entries are the turn's targets: the offer is bounded
        // to them.
        available_actions.retain(|action| placements.contains_key(action));
        if held_action.is_some_and(|held| !placements.contains_key(&held)) {
            return Err(refuse("held_action_unavailable"));
        }
        if available_actions.is_empty() {
            return Err(refuse("no_feasible_target"));
        }
    }
    let mut infeasible: BTreeMap<usize, String> = BTreeMap::new();
    for (action, entry) in &capacity {
        let Some(action) = action else { continue };
        if entry.fits == Some(false) {
            infeasible.insert(*action, entry.reason.clone().unwrap_or_default());
        } else if rule.is_some() && entry.fits.is_none() && entry.placement == "local" {
            infeasible.insert(*action, "capacity_unknown_local".to_owned());
        }
    }
    if !infeasible.is_empty() {
        available_actions.retain(|action| !infeasible.contains_key(action));
        if held_action.is_some_and(|held| infeasible.contains_key(&held)) {
            return Err(refuse("held_action_unavailable"));
        }
        if available_actions.is_empty() {
            return Err(refuse("no_feasible_target"));
        }
    }
    let last_served_action = selection
        .last_served_action_id
        .as_deref()
        .map(|id| action_of(id, "action_not_in_catalog"))
        .transpose()?;

    // The request's messages and who produced them.
    let request: Value = serde_json::from_str(input.request.get())
        .map_err(|e| DecideError::Projection(e.to_string()))?;
    let messages = request
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or(invalid("request.messages"))?;
    let has_tools = request
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    let mut sources: Vec<Option<String>> = vec![None; messages.len()];
    let mut served_at: Vec<Option<(usize, Option<usize>)>> = vec![None; messages.len()];
    for item in &input.attribution {
        let action = release
            .action_index
            .get(&item.action_id)
            .copied()
            .ok_or(refuse("attribution_out_of_catalog"))?;
        let trained = &release.actions[action].arms;
        let arm = match &item.arm_id {
            Some(arm) => Some(
                release
                    .arm_index
                    .get(arm)
                    .copied()
                    .filter(|arm| trained.contains(arm))
                    .ok_or(refuse("attribution_out_of_catalog"))?,
            ),
            None if trained.len() == 1 => Some(trained[0]),
            None => None,
        };
        let role = messages.get(item.message).and_then(|m| m.get("role"));
        if role.and_then(Value::as_str) != Some("assistant") {
            return Err(unsupported("attribution_not_representable"));
        }
        sources[item.message] = arm.map(|arm| release.arms[arm].id.clone());
        served_at[item.message] = Some((action, arm));
    }
    let served: Vec<(usize, Option<usize>)> = served_at.into_iter().flatten().collect();
    let attributed = if input.attribution.is_empty() {
        &[][..]
    } else {
        &sources[..]
    };
    let blocks = project_request(input.request.get(), attributed).map_err(|e| match e {
        rayline_arc_input::ProjectionError::TooLarge(reason) => DecideError::TooLarge(reason),
        rayline_arc_input::ProjectionError::Unsupported(reason) => {
            DecideError::NotEncodable(reason)
        }
        e => DecideError::Projection(e.to_string()),
    })?;
    let encoded = release
        .tokenizer
        .encode(&blocks)
        .map_err(|e| DecideError::Projection(e.to_string()))?;

    let mut available_arms: Vec<usize> = available_actions
        .iter()
        .flat_map(|action| release.actions[*action].arms.iter().copied())
        .collect();
    let mut held_arm = None;
    if let Some(held) = held_action {
        held_arm = served
            .iter()
            .rev()
            .find(|(action, _)| *action == held)
            .and_then(|(_, arm)| *arm);
        if held_arm.is_none() {
            available_arms = release.actions[held].arms.clone();
        }
    }
    let ResolvedPrices {
        rates,
        excluded,
        keys,
    } = prices::resolve(release, input.routing_prices.as_ref())?;
    if excluded.iter().any(|e| *e) {
        available_arms.retain(|arm| !excluded[*arm]);
        available_actions.retain(|action| {
            release.actions[*action]
                .arms
                .iter()
                .any(|arm| !excluded[*arm])
        });
        if held_action.is_some_and(|held| !available_actions.contains(&held))
            || held_arm.is_some_and(|arm| excluded[arm])
        {
            return Err(refuse("held_action_unavailable"));
        }
        if available_arms.is_empty() {
            return Err(refuse("no_supported_selection"));
        }
    }

    let rows = readouts
        .readouts(&ReadoutRequest {
            episode: input.episode.as_deref(),
            tokens: &encoded.tokens,
            offsets: &encoded.readout_offsets,
        })
        .map_err(DecideError::Readouts)?;
    let dim = release.head.dim();
    if rows.len() != blocks.len() || rows.iter().any(|row| row.len() != dim) {
        return Err(DecideError::Readouts(format!(
            "expected {} rows of {dim}, got {}",
            blocks.len(),
            rows.len()
        )));
    }
    if rows.iter().flatten().any(|value| !value.is_finite()) {
        return Err(DecideError::Readouts("a readout is not finite".into()));
    }
    let Some(decision_readout) = rows.last() else {
        return Err(DecideError::Projection("the request has no blocks".into()));
    };

    // Stage one.
    let internal = |e: crate::tensors::TensorError| DecideError::Internal(e.0);
    let predictions = release
        .head
        .predict(decision_readout, release.settings.regime)
        .map_err(internal)?;
    let mut arm_candidate = vec![None; release.arms.len()];
    for (candidate, arms) in release.candidate_arms.iter().enumerate() {
        for arm in arms {
            arm_candidate[*arm] = Some(candidate);
        }
    }
    let candidate_rates: Vec<Result<[f64; LEGS], ()>> = release
        .candidate_arms
        .iter()
        .map(|arms| {
            // An arm an exclusive caller table left out has no rate here.
            let mut surviving = arms.iter().filter(|arm| !excluded[**arm]);
            let Some(first) = surviving.next() else {
                return Ok([f64::NAN; LEGS]);
            };
            if surviving.any(|arm| rates[*arm] != rates[*first]) {
                return Err(());
            }
            Ok(rates[*first].map(|rate| rate.unwrap_or(f64::NAN)))
        })
        .collect();
    let first_user = blocks.iter().position(|block| block.role == "user");
    let answered =
        first_user.is_some_and(|first| blocks[first + 1..].iter().any(|b| b.role == "assistant"));
    let last_served = match last_served_action {
        Some(action) => Some((
            action,
            served
                .last()
                .filter(|(last, _)| *last == action)
                .and_then(|(_, arm)| *arm),
        )),
        None => served.last().copied(),
    };
    let wants_receipt = input.policy.is_some()
        || input.event.is_some()
        || input.turn_key.is_some()
        || !input.capacity.is_empty()
        || selection.last_served_action_id.is_some();
    let context = OneContext {
        release,
        input,
        predictions: &predictions,
        candidate_rates: &candidate_rates,
        arm_candidate: &arm_candidate,
        answered,
        caller_holds: held_action.is_some(),
        last_served,
        schedule: schedule.as_ref(),
        infeasible: &infeasible,
        rule,
        placements: &placements,
        wants_receipt,
    };
    let mut tier_pools = Vec::with_capacity(tiers.len());
    for tier in &tiers {
        let terms = tool_free(release, has_tools, &tier.name, Some(tier.cost_weight));
        tier_pools.push(
            context
                .stage_one(&available_arms, Some(tier.cost_weight), terms.as_ref())?
                .arms,
        );
    }
    let terms = tool_free(
        release,
        has_tools,
        &point_name,
        primary.as_ref().map(|point| point.cost_weight),
    );
    let outcome = context.stage_one(
        &available_arms,
        primary.as_ref().map(|point| point.cost_weight),
        terms.as_ref(),
    )?;

    // Stage two.
    let roles: Vec<&str> = blocks.iter().map(|block| block.role).collect();
    let block_sources: Vec<Option<usize>> = blocks
        .iter()
        .map(|block| {
            block
                .source
                .as_ref()
                .and_then(|id| release.arm_index.get(id).copied())
        })
        .collect();
    let heads: Vec<ArmScore> = release
        .stage_two
        .predict(&rows, &roles, &block_sources)
        .map_err(internal)?;
    let costs: Vec<f32> = heads
        .iter()
        .zip(&rates)
        .map(|(head, rates)| stage_two::cost(&head.usage, rates))
        .collect();
    let scores: Vec<f32> = heads
        .iter()
        .zip(&costs)
        .map(|(head, cost)| head.quality - release.stage_two_cost_weight as f32 * cost)
        .collect();
    let priced: Vec<bool> = rates
        .iter()
        .map(|r| r.iter().all(Option::is_some))
        .collect();
    let pick = |pool: &[usize]| -> Result<(usize, Vec<bool>), DecideError> {
        let mut valid = vec![false; release.arms.len()];
        for arm in pool {
            valid[*arm] = priced[*arm]
                && release.stage_two.quality_support[*arm]
                && release.stage_two.usage_support[*arm];
        }
        let mut chosen = best_arm(pool, &valid, &scores).ok_or(refuse("no_supported_selection"))?;
        if let Some(held) = held_arm {
            if !valid[held] {
                return Err(refuse("held_action_unavailable"));
            }
            chosen = held;
        }
        Ok((chosen, valid))
    };
    let (chosen, valid) = pick(&outcome.arms)?;
    let action_of_arm = |arm: usize| {
        release.arms[arm].action.ok_or(DecideError::Internal(
            "the chosen arm serves no action".into(),
        ))
    };
    let chosen_action = action_of_arm(chosen)?;
    let tier_picks = if tiers.is_empty() {
        None
    } else {
        let mut picks = BTreeMap::new();
        for (tier, pool) in tiers.iter().zip(&tier_pools) {
            let (arm, _) = pick(pool)?;
            let action = &release.actions[action_of_arm(arm)?];
            picks.insert(
                tier.name.clone(),
                TierPick {
                    selected_action_id: action.id.clone(),
                    selected_arm_id: release.arms[arm].id.clone(),
                    model: action.model.clone(),
                    thinking: action.thinking.clone(),
                },
            );
        }
        Some(picks)
    };

    // One score per action: its best arm's.
    let offered: HashSet<usize> = available_actions.iter().copied().collect();
    let mut by_action: Vec<Option<usize>> = vec![None; release.actions.len()];
    let mut order = Vec::new();
    for (arm, record) in release.arms.iter().enumerate() {
        let Some(action) = record.action else {
            continue;
        };
        match by_action[action] {
            None => {
                by_action[action] = Some(arm);
                order.push(action);
            }
            Some(best) => {
                // A supported arm first, then the higher score; the first on a tie.
                if (valid[arm] && !valid[best])
                    || (valid[arm] == valid[best] && scores[arm] > scores[best])
                {
                    by_action[action] = Some(arm);
                }
            }
        }
    }
    let actions = order
        .into_iter()
        .filter_map(|action| Some((action, by_action[action]?)))
        .map(|(action, best)| {
            let record = &release.actions[action];
            ActionScore {
                action_id: record.id.clone(),
                available: offered.contains(&action),
                supported: record.arms.iter().any(|arm| valid[*arm]),
                selected: action == chosen_action,
                score: scores[best],
                predicted_quality: release.stage_two.quality_support[best]
                    .then_some(heads[best].quality),
                predicted_cost_usd: (priced[best] && release.stage_two.usage_support[best])
                    .then_some(costs[best]),
                price_per_mtok: PricePerMtok {
                    input: rates[best][0],
                    cache_read: rates[best][1],
                    cache_write: rates[best][2],
                    output: rates[best][3],
                },
                price_key: keys[best].clone(),
                thinking: record.thinking.clone(),
            }
        })
        .collect();

    let receipt = wants_receipt.then(|| {
        let stated: HashSet<usize> = capacity
            .iter()
            .filter(|(_, entry)| entry.fits.is_some())
            .filter_map(|(action, _)| *action)
            .collect();
        let mut capacity_unknown: Vec<String> = available_actions
            .iter()
            .filter(|action| !stated.contains(action))
            .map(|action| release.actions[*action].id.clone())
            .collect();
        capacity_unknown.sort();
        let mut excluded: Vec<Excluded> = infeasible
            .iter()
            .map(|(action, reason)| Excluded {
                action_id: release.actions[*action].id.clone(),
                reason: reason.clone(),
            })
            .collect();
        excluded.sort_by(|a, b| a.action_id.cmp(&b.action_id));
        let info = outcome.receipt.unwrap_or(Receipted {
            decided_by: DecidedBy {
                model: None,
                thinking: "schedule",
            },
            eligible: Eligible {
                model: None,
                thinking: Some("all_turns"),
            },
            basis: "service_inferred".to_owned(),
            selection: None,
        });
        Receipt {
            action_id: release.actions[chosen_action].id.clone(),
            control_id: release.actions[chosen_action].thinking.control_id.clone(),
            policy: input.policy.as_ref().map(|policy| policy.preset.clone()),
            decided_by: info.decided_by,
            eligible: info.eligible,
            held_from: last_served.map(|(action, _)| release.actions[action].id.clone()),
            excluded,
            capacity_unknown,
            basis: Some(info.basis),
            selection: info.selection,
        }
    });

    Ok(DecideOutput {
        decision: Decision {
            selected_action_id: release.actions[chosen_action].id.clone(),
            selected_arm_id: release.arms[chosen].id.clone(),
            reason: if held_arm.is_some() {
                "scheduled_model_hold"
            } else {
                "supported_prediction"
            },
            operating_point: PointEcho {
                name: point_name,
                cost_weight: primary
                    .map_or(release.stage_two_cost_weight, |point| point.cost_weight),
            },
            tier_picks,
            receipt,
        },
        actions,
        encoding: Encoding {
            token_count: encoded.tokens.len(),
        },
        stage_one: outcome.evidence,
    })
}

/// Each action's rates per million tokens at `routing` prices, and the
/// caller key that priced them, as a decision would report them without
/// scoring: its first arm the prices don't exclude that all four legs
/// price, else its first arm they don't exclude. An action every arm of
/// which is excluded is left out. For a route made without a decision, so
/// its price table is a decision's.
pub fn action_prices(
    release: &Release,
    routing: Option<&crate::RoutingPrices>,
) -> Result<Vec<(String, PricePerMtok, Option<String>)>, DecideError> {
    let ResolvedPrices {
        rates,
        excluded,
        keys,
    } = prices::resolve(release, routing)?;
    Ok(release
        .actions
        .iter()
        .filter_map(|action| {
            let mut arms = action.arms.iter().filter(|arm| !excluded[**arm]);
            let first = *arms.clone().next()?;
            let arm = arms
                .find(|arm| rates[**arm].iter().all(Option::is_some))
                .copied()
                .unwrap_or(first);
            let [input, cache_read, cache_write, output] = rates[arm];
            Some((
                action.id.clone(),
                PricePerMtok {
                    input,
                    cache_read,
                    cache_write,
                    output,
                },
                keys[arm].clone(),
            ))
        })
        .collect())
}
