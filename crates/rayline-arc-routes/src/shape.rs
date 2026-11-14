//! A decide answer into the Routes API V1 answer.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Map, Value, json};

use crate::PRICE_LEGS;
use crate::catalog::{Action, Catalog};
use crate::digest::{offer_sha256, price_sha256};
use crate::error::Refusal;
use crate::execution::{self, PinStatus};
use crate::parse::{Restate, Tier};

/// Says on every answer that no hosted service decided it.
const LOCAL_WARNING: &str =
    "local_routes_adapter: decided by a local policy service, not the hosted cell";

fn source(kind: &str) -> Value {
    Value::Object(
        PRICE_LEGS
            .iter()
            .map(|leg| ((*leg).to_owned(), json!(kind)))
            .collect(),
    )
}

struct Shaper<'a> {
    catalog: &'a Catalog,
    restate: &'a Restate,
    /// `routing.prices.models`: one row per model of an offered action.
    table: Map<String, Value>,
    result_prices: Value,
    offer: String,
    receipt: Map<String, Value>,
}

impl<'a> Shaper<'a> {
    fn action(&self, action_id: &str) -> Result<&'a Action, Refusal> {
        self.catalog
            .actions
            .get(action_id)
            .ok_or_else(|| Refusal::upstream("the decision names an action outside the package."))
    }

    fn public_id(&self, model: &'a str) -> &'a str {
        self.catalog.public_id(model, self.restate.prices.keys())
    }

    /// One result: the action a tier picked, its identity and its receipt.
    fn result(&self, tier: Tier, pick: &Value) -> Result<Value, Refusal> {
        let action_id = pick
            .get("selected_action_id")
            .and_then(Value::as_str)
            .ok_or_else(|| Refusal::upstream("a tier pick names no action."))?;
        let action = self.action(action_id)?;
        let worker = self.restate.workers.get(action_id).ok_or_else(|| {
            Refusal::upstream("the decision selected an action that was not offered.")
        })?;
        let mut receipt = self.receipt.clone();
        receipt.insert("action_id".to_owned(), json!(action_id));
        receipt.insert("control_id".to_owned(), json!(action.control_id));
        let budget = pick
            .get("thinking")
            .and_then(|thinking| thinking.get("budget_tokens"));
        let mut result = json!({
            "model": self.public_id(&action.model),
            "worker": worker,
            "thinking": {"mode": "on", "budget_tokens": budget},
            "policy_action_id": action_id,
            "v_value": tier.value_usd(),
            "cost_weight": tier.cost_weight(),
            "prices": self.result_prices,
            "identity": {
                "package_sha256": self.catalog.package_sha256,
                "action_id": action_id,
                "policy_action_id": action_id,
                "control_id": action.control_id,
                "arm_id": pick.get("selected_arm_id"),
                "thinking_controls_sha256": self.catalog.thinking_controls_sha256,
                "offer_sha256": self.offer,
                "prices": self.result_prices,
            },
            "receipt": receipt,
        });
        if let Some(level) = action.level.as_deref().filter(|level| !level.is_empty()) {
            result["thinking_level"] = json!(level);
        }
        Ok(result)
    }
}

/// `routing.prices.models` as answered, and the same legs as numbers.
type PriceWorld = (Map<String, Value>, BTreeMap<String, [f64; 4]>);

/// The price world: one row per model of an offered action, at the rate the
/// decision used. A caller-priced model is the caller's four legs.
fn price_table(
    catalog: &Catalog,
    restate: &Restate,
    rows: &BTreeMap<&str, &Value>,
) -> Result<PriceWorld, Refusal> {
    let mut table = Map::new();
    let mut resolved = BTreeMap::new();
    for action_id in &restate.offered {
        let action = catalog
            .actions
            .get(action_id)
            .ok_or_else(|| Refusal::upstream("an offered action is outside the package."))?;
        let id = catalog.public_id(&action.model, restate.prices.keys());
        if let Some(legs) = restate.prices.get(id) {
            let mut row = serde_json::to_value(legs)
                .ok()
                .and_then(|row| match row {
                    Value::Object(row) => Some(row),
                    _ => None,
                })
                .unwrap_or_default();
            row.insert("source".to_owned(), source("caller"));
            table.insert(id.to_owned(), Value::Object(row));
            resolved.insert(id.to_owned(), legs.array());
            continue;
        }
        let legs = rows
            .get(action_id.as_str())
            .and_then(|row| row.get("price_per_mtok"))
            .filter(|legs| !legs.is_null());
        let Some(legs) = legs else { continue };
        if table.contains_key(id) {
            continue;
        }
        let mut row = Map::new();
        let mut prices = [0.0; 4];
        for (leg, price) in PRICE_LEGS.iter().zip(&mut prices) {
            let value = legs.get(leg).filter(|value| value.is_number());
            let (Some(value), Some(number)) = (value, value.and_then(Value::as_f64)) else {
                return Err(Refusal::upstream(
                    "the decision priced an action without all four legs.",
                ));
            };
            *price = number;
            row.insert((*leg).to_owned(), value.clone());
        }
        row.insert("source".to_owned(), source("catalog"));
        table.insert(id.to_owned(), Value::Object(row));
        resolved.insert(id.to_owned(), prices);
    }
    Ok((table, resolved))
}

/// The V1 answer for one decide answer. `route_id` and `latency_ms` are the
/// caller's: this function is otherwise a function of its inputs alone.
///
/// A decide answer this router cannot restate (no pick for the requested
/// tier, an action outside the offer, a price row without its legs) is
/// refused `inconsistent_upstream`, never patched.
pub fn shape(
    catalog: &Catalog,
    restate: &Restate,
    answer: &Value,
    route_id: &str,
    latency_ms: u64,
) -> Result<Value, Refusal> {
    let decision = answer
        .get("decision")
        .and_then(Value::as_object)
        .ok_or_else(|| Refusal::upstream("the decide answer carries no decision."))?;
    let actions = answer
        .get("actions")
        .and_then(Value::as_array)
        .ok_or_else(|| Refusal::upstream("the decide answer carries no actions."))?;
    let rows: BTreeMap<&str, &Value> = actions
        .iter()
        .filter_map(|row| Some((row.get("action_id")?.as_str()?, row)))
        .collect();
    let priced = !restate.prices.is_empty();
    let (table, resolved) = price_table(catalog, restate, &rows)?;
    let digest = price_sha256(&resolved)
        .ok_or_else(|| Refusal::upstream("the decision priced an action at no finite rate."))?;
    let basis = if table.keys().any(|model| restate.prices.contains_key(model)) {
        "caller"
    } else {
        "catalog"
    };
    let offer = offer_sha256(&catalog.package_sha256, &restate.offered);
    let offer_pin = match &restate.expect_offer_sha256 {
        None => PinStatus::Unpinned,
        Some(stated) if *stated == offer => PinStatus::Matched,
        Some(stated) => {
            return Err(Refusal::new(
                "execution_identity_mismatch",
                format!(
                    "routing.execution.expect.offer_sha256 is {stated}; the offer's is {offer}."
                ),
            ));
        }
    };

    // One receipt for every result of this decision, naming the dropped targets.
    let applied = decision.contains_key("receipt");
    let mut receipt = decision
        .get("receipt")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut excluded = match receipt.remove("excluded") {
        Some(Value::Array(excluded)) => excluded,
        _ => Vec::new(),
    };
    excluded.extend(
        restate
            .unknown
            .iter()
            .map(|id| json!({"action_id": id, "reason": "unknown_action"})),
    );
    receipt.insert("excluded".to_owned(), Value::Array(excluded));

    let shaper = Shaper {
        catalog,
        restate,
        table,
        result_prices: json!({"basis": basis, "price_sha256": priced.then_some(&digest)}),
        offer,
        receipt,
    };
    let picks = decision.get("tier_picks").and_then(Value::as_object);
    let mut presets = Map::new();
    for tier in Tier::ALL {
        if let Some(pick) = picks.and_then(|picks| picks.get(tier.name())) {
            presets.insert(tier.name().to_owned(), shaper.result(tier, pick)?);
        }
    }
    let top = presets
        .get(restate.primary.name())
        .and_then(Value::as_object)
        .ok_or_else(|| Refusal::upstream("the decision has no pick for the requested tier."))?;
    let top_model = top.get("model").and_then(Value::as_str).unwrap_or_default();
    let chosen = shaper.table.get(top_model);

    // The next best models by score, each once, the selected one left out.
    let mut scored: Vec<(f64, &Value)> = actions
        .iter()
        .filter(|row| row.get("available").and_then(Value::as_bool) == Some(true))
        .filter_map(|row| Some((row.get("score")?.as_f64()?, row)))
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut seen = BTreeSet::from([top_model]);
    let mut alternatives = Vec::new();
    for (_, row) in scored {
        let Some(action_id) = row.get("action_id").and_then(Value::as_str) else {
            continue;
        };
        let name = shaper.public_id(&shaper.action(action_id)?.model);
        if seen.insert(name) && alternatives.len() < 2 {
            let worker = restate.workers.get(action_id).map_or(name, String::as_str);
            alternatives.push(json!({"model": name, "worker": worker, "score": row.get("score")}));
        }
    }

    let mut selected_pricing: Map<String, Value> = PRICE_LEGS
        .iter()
        .map(|leg| {
            let price = chosen.and_then(|row| row.get(leg)).cloned();
            ((*leg).to_owned(), price.unwrap_or(Value::Null))
        })
        .collect();
    let pricing_source = chosen
        .and_then(|row| row.get("source")?.get("input")?.as_str())
        .unwrap_or("catalog");
    selected_pricing.insert("pricing_source".to_owned(), json!(pricing_source));

    let mut routing = json!({
        "execution": {
            "status": if applied { "applied" } else { "unsupported" },
            "version": execution::VERSION,
            "turn": restate.turn,
            "pinned": applied.then(|| json!({
                "package_sha256": restate.pinned.package_sha256,
                "thinking_controls_sha256": restate.pinned.thinking_controls_sha256,
                "offer_sha256": offer_pin,
            })),
            "episode_identity": "session_branch",
        },
        "policy": {
            "status": if applied { "applied" } else { "unsupported" },
            "preset": restate.preset,
        },
        "unapplied_keys": restate.unapplied_keys,
    });
    if priced {
        let unused: Vec<&String> = restate
            .prices
            .keys()
            .filter(|model| !shaper.table.contains_key(*model))
            .collect();
        routing["prices"] = json!({
            "status": "applied", "models": shaper.table, "unused": unused, "price_sha256": digest});
    }

    let mut out = json!({
        "route_id": route_id,
        "object": "route",
        "checkpoint": format!(
            "local.{}.{}",
            catalog.alias,
            catalog.package_sha256.get(..16).unwrap_or(&catalog.package_sha256)
        ),
        "alternatives": alternatives,
        "selected_pricing": selected_pricing,
        "warnings": [LOCAL_WARNING],
        "usage": {
            "encoded_input_tokens": answer.get("encoding").and_then(|e| e.get("token_count")),
            "cache_read_tokens": 0,
        },
        "latency_ms": latency_ms,
        "routing": routing,
    });
    for key in [
        "model",
        "worker",
        "thinking",
        "thinking_level",
        "policy_action_id",
        "identity",
        "receipt",
    ] {
        if let Some(value) = top.get(key) {
            out[key] = value.clone();
        }
    }
    out["presets"] = Value::Object(presets);
    Ok(out)
}
