//! A route for when the decider cannot answer: the local encoder is down,
//! restarting or slow, or the request is one it doesn't read.
//!
//! The execution core does not fall back on its own: a lookup it cannot get
//! fails the harness turn. So an unavailable decider is answered here with
//! the conversation's held model, or the package's fallback action, and a
//! receipt that says why.

use serde_json::{Value, json};

use crate::catalog::Catalog;
use crate::error::Refusal;
use crate::parse::DecideInput;

/// The receipt `basis` of every route made without a decision, whatever
/// the reason (encoder down or slow, a request too large, a format or
/// content the decider doesn't read). The route log names the reason.
pub const ENCODER_UNAVAILABLE: &str = "encoder_unavailable";

/// What a fallback is chosen from, kept before the decide input is spent.
#[derive(Debug, Clone)]
pub struct Fallback {
    /// Most preferred first: the held action, the last served one, the
    /// latest attributed one, the package's fallback action, then every
    /// offered action from the cheapest at the request's prices (a first turn
    /// whose package fallback is not offered still gets a route).
    preference: Vec<String>,
    /// How many leading `preference` entries come from the conversation.
    conversation: usize,
    offered: Vec<String>,
    tiers: Vec<&'static str>,
    /// The targets the request says don't fit, with its reason: a receipt
    /// names each, as a decision's does.
    excluded: Vec<Value>,
    /// The policy preset the request applies, which every receipt names.
    preset: String,
}

impl Fallback {
    /// `rows` are the decider's [`crate::Decider::action_prices`].
    pub fn of(decide: &DecideInput, catalog: &Catalog, rows: &[Value]) -> Self {
        let selection = &decide.selection;
        let latest_attributed = decide
            .attribution
            .iter()
            .max_by_key(|item| item.message)
            .map(|item| item.action_id.clone());
        let mut preference: Vec<String> = [
            selection.held_action_id.clone(),
            selection.last_served_action_id.clone(),
            latest_attributed,
        ]
        .into_iter()
        .flatten()
        .collect();
        let conversation = preference.len();
        preference.extend(catalog.fallback_action_id.clone());
        preference.extend(cheapest_first(decide, rows));
        let mut tiers: Vec<&'static str> = selection
            .tier_points
            .iter()
            .map(|point| point.name)
            .collect();
        if !tiers.contains(&selection.operating_point.name) {
            tiers.push(selection.operating_point.name);
        }
        Self {
            preference,
            conversation,
            // A fallback never serves a target the request's capacity facts
            // say does not fit, nor a local one whose fit is unknown (as the
            // policy's own admission), unless it is the held action: a hold
            // is the only action the executor may run.
            offered: selection
                .available_action_ids
                .iter()
                .filter(|action| {
                    let held = selection.held_action_id.as_ref() == Some(*action);
                    decide
                        .capacity
                        .iter()
                        .filter(|fact| &fact.action_id == *action)
                        .all(|fact| match fact.fits {
                            Some(fits) => fits,
                            None => held || fact.placement != "local",
                        })
                })
                .cloned()
                .collect(),
            tiers,
            preset: decide.policy.preset.clone(),
            excluded: {
                let mut excluded: Vec<(String, String)> = decide
                    .capacity
                    .iter()
                    .filter(|fact| fact.fits == Some(false))
                    .map(|fact| {
                        (
                            fact.action_id.clone(),
                            fact.reason.clone().unwrap_or_default(),
                        )
                    })
                    .collect();
                excluded.sort();
                excluded.dedup();
                excluded
                    .into_iter()
                    .map(|(action_id, reason)| json!({"action_id": action_id, "reason": reason}))
                    .collect()
            },
        }
    }

    /// The action a fallback route serves, if one is offered, and whether
    /// it is the conversation's own (held, last served or attributed).
    fn pick(&self) -> Option<(&str, bool)> {
        self.preference
            .iter()
            .position(|action| self.offered.contains(action))
            .map(|at| (self.preference[at].as_str(), at < self.conversation))
    }

    /// The action a fallback route serves, if one is offered.
    pub fn action(&self) -> Option<&str> {
        self.pick().map(|(action, _)| action)
    }

    /// A decide answer naming [`Self::action`] for every tier, or `None`
    /// when no candidate is offered. `rows` are the decider's
    /// [`crate::Decider::action_prices`], so the route's price table and
    /// receipt are what a decision's would be; only the selection differs.
    pub fn answer(&self, rows: Vec<Value>) -> Option<Value> {
        let (action, own) = self.pick()?;
        // The core accepts only the receipt reasons it knows: a model the
        // conversation already had is `held`, anything else the policy's
        // `utility_fallback`. `basis` says why the decider was skipped.
        let (model, thinking) = if own {
            ("held", "held")
        } else {
            ("utility_fallback", "schedule")
        };
        let picks: serde_json::Map<String, Value> = self
            .tiers
            .iter()
            .map(|tier| ((*tier).to_owned(), json!({"selected_action_id": action})))
            .collect();
        Some(json!({
            "decision": {
                "tier_picks": picks,
                "receipt": {
                    "policy": self.preset,
                    "decided_by": {"model": model, "thinking": thinking},
                    "basis": ENCODER_UNAVAILABLE,
                    "excluded": self.excluded,
                },
            },
            "actions": rows,
            "encoding": {"token_count": null},
        }))
    }
}

/// The offered actions, cheapest first at the rates the decider prices
/// them at (input plus output per token); unpriced actions after priced
/// ones, then by id.
fn cheapest_first(decide: &DecideInput, rows: &[Value]) -> Vec<String> {
    let price = |action_id: &str| -> Option<f64> {
        let legs = rows
            .iter()
            .find(|row| row["action_id"] == action_id)?
            .get("price_per_mtok")?;
        Some(legs.get("input")?.as_f64()? + legs.get("output")?.as_f64()?)
    };
    let mut offered: Vec<(Option<f64>, String)> = decide
        .selection
        .available_action_ids
        .iter()
        .map(|id| (price(id), id.clone()))
        .collect();
    offered.sort_by(|(a, a_id), (b, b_id)| match (a, b) {
        (Some(a), Some(b)) => a.total_cmp(b).then_with(|| a_id.cmp(b_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a_id.cmp(b_id),
    });
    offered.into_iter().map(|(_, id)| id).collect()
}

/// Whether a decider's refusal means it could not decide at all (its
/// backend is down or slow), as opposed to refusing this request.
pub fn unavailable(refusal: &Refusal) -> bool {
    matches!(refusal.status, 502..=504)
}
