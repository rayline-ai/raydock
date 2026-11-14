//! A hand-written package, request and decide answer. Nothing here is
//! recorded traffic.
#![allow(dead_code)]

use rayline_arc_routes::Catalog;
use serde_json::{Value, json};

/// A sha256-shaped id: one hex digit, 64 times.
pub fn id(digit: char) -> String {
    digit.to_string().repeat(64)
}

pub const ALIAS: &str = "local/test-package";
pub const PRESET: &str = "arc-policy.prefer-local.v1";
pub const LOCAL_MODEL: &str = "qwen/qwen3.8-27b";

/// Three actions: two cloud models and a local one.
pub fn manifest() -> Vec<u8> {
    json!({
        "schema_version": "test",
        "thinking_controls_sha256": id('9'),
        "actions": [
            {"action_id": id('a'), "model": "claude-haiku-5.5", "control_id": id('1'), "level": "none"},
            {"action_id": id('b'), "model": "claude-opus-5.5", "control_id": id('2'), "level": "up"},
            {"action_id": id('c'), "model": "qwen3.8-27b", "control_id": id('3'), "level": null},
        ],
    })
    .to_string()
    .into_bytes()
}

pub fn catalog() -> Catalog {
    Catalog::from_manifest(&manifest(), ALIAS).expect("the fixture manifest")
}

pub fn turn() -> Value {
    json!({"id": "turn-1", "attempt": 1, "epoch": "epoch:0", "retry_of": null, "reconsult": null})
}

/// A second turn of a conversation: the first was served by action `a`. The
/// local target does not fit, and one target is outside the package.
pub fn request() -> Value {
    json!({
        "model": "arc-swift-1.0",
        "max_tokens": 1024,
        "stream": true,
        "system": [{"type": "text", "text": "You are a test."}],
        "tools": [{"name": "read", "input_schema": {"type": "object"}}],
        "messages": [
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "second"},
            {"role": "user", "content": "third"},
        ],
        "routing": {
            "execution": {
                "version": "arc-execution-context.v2",
                "turn": turn(),
                "event": {"kind": "user_message", "basis": "marker_inferred", "compaction": null},
                "last_served": {"action_id": id('a'), "worker": "haiku", "message": 1},
                "history": [{"message": 1, "action_id": id('a')}],
                "hold": null,
                "expect": null,
                "targets": [
                    {"action_id": id('b'), "worker": "opus", "placement": "cloud", "cache": null, "capacity": null},
                    {"action_id": id('a'), "worker": "haiku", "placement": "cloud", "capacity": null, "cache": {
                        "context_epoch": "epoch:0", "gap_s": 12, "residency": null,
                        "ttl": {"seconds": 300, "source": "cache_control", "basis": "wire_declared"},
                        "usage": {"prompt_tokens": 900, "cache_read_tokens": 100, "cache_write_tokens": 700,
                                  "units": "provider:haiku", "provenance": "provider_reported"},
                        "current_prefix": {"tokens": 950, "overlap_tokens": 900, "units": "provider:haiku",
                                           "provenance": "local_measured"}}},
                    {"action_id": id('c'), "worker": "local-qwen", "placement": "local", "cache": null, "capacity": {
                        "fits": false, "reason": "context_capacity", "required": 50000, "available": 32768,
                        "available_source": "configured", "units": "hf:test", "provenance": "local_measured"}},
                    {"action_id": id('f'), "worker": "elsewhere", "placement": "cloud", "cache": null, "capacity": null},
                ],
            },
            "policy": {"preset": PRESET},
            "prices": {LOCAL_MODEL: {"input": 0, "cache_read": 0.0, "cache_write": 0, "output": 0.25}},
        },
    })
}

/// What the policy answers [`request`] with: swift picks `a`, sage picks `b`.
pub fn answer() -> Value {
    let legs = |input: f64, output: f64| json!({"input": input, "cache_read": input / 10.0, "cache_write": 0, "output": output});
    json!({
        "decision": {
            "tier_picks": {
                "swift": {"selected_action_id": id('a'), "selected_arm_id": id('7'), "thinking": {"budget_tokens": null}},
                "sage": {"selected_action_id": id('b'), "selected_arm_id": null, "thinking": {"budget_tokens": 8000}},
            },
            "receipt": {
                "policy": PRESET,
                "decided_by": {"model": "utility_fallback", "thinking": "schedule"},
                "eligible": {"model": "user_messages", "thinking": "all_turns"},
                "held_from": null,
                "excluded": [{"action_id": id('c'), "reason": "context_capacity"}],
                "capacity_unknown": [id('a'), id('b')],
                "basis": "marker_inferred",
            },
        },
        "actions": [
            {"action_id": id('a'), "available": true, "score": 0.5, "price_per_mtok": legs(1.0, 5.0)},
            {"action_id": id('b'), "available": true, "score": 0.75, "price_per_mtok": legs(5.0, 25.0)},
            {"action_id": id('c'), "available": false, "score": null, "price_per_mtok": legs(0.0, 0.25)},
        ],
        "encoding": {"token_count": 1234},
    })
}

/// The checks the pinned core makes of a `/v1/routes` answer to `sent`
/// before it runs a turn: the receipt names each target that doesn't fit
/// and never selects one, every result carries the package identity, and
/// with caller prices the price table is acknowledged (applied, non-empty,
/// holding the selected model, under the digest every result names).
pub fn assert_core_accepts(shaped: &Value, sent: &Value) {
    assert!(shaped["route_id"].as_str().is_some_and(|id| !id.is_empty()));
    let fits_false: Vec<(&str, &str)> = sent["routing"]["execution"]["targets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|target| target["capacity"]["fits"] == false)
        .map(|target| {
            (
                target["action_id"].as_str().unwrap(),
                target["capacity"]["reason"].as_str().unwrap(),
            )
        })
        .collect();
    for name in ["swift", "sage"] {
        let result = &shaped["presets"][name];
        let identity = &result["identity"];
        assert_eq!(identity["action_id"], identity["policy_action_id"]);
        assert_eq!(result["policy_action_id"], identity["action_id"]);
        let action = &catalog().actions[identity["action_id"].as_str().unwrap()];
        assert_eq!(identity["control_id"], json!(action.control_id));
        assert_eq!(identity["package_sha256"], json!(catalog().package_sha256));
        let receipt = &result["receipt"];
        assert_eq!(receipt["action_id"], identity["action_id"]);
        assert_eq!(receipt["control_id"], identity["control_id"]);
        assert_eq!(receipt["policy"], PRESET);
        assert!(receipt["decided_by"]["thinking"].is_string());
        for (action_id, reason) in &fits_false {
            assert_ne!(identity["action_id"], *action_id);
            assert!(
                receipt["excluded"]
                    .as_array()
                    .unwrap()
                    .contains(&json!({"action_id": action_id, "reason": reason}))
            );
        }
    }
    let routing = &shaped["routing"];
    assert_eq!(routing["execution"]["status"], "applied");
    assert_eq!(routing["execution"]["version"], "arc-execution-context.v2");
    assert_eq!(
        routing["execution"]["turn"],
        sent["routing"]["execution"]["turn"]
    );
    assert_eq!(
        routing["policy"],
        json!({"status": "applied", "preset": PRESET})
    );
    assert_eq!(routing["unapplied_keys"], json!([]));
    if sent["routing"]["prices"]
        .as_object()
        .is_some_and(|prices| !prices.is_empty())
    {
        let prices = &routing["prices"];
        assert_eq!(prices["status"], "applied");
        let models = prices["models"].as_object().expect("a price table");
        assert!(!models.is_empty(), "an empty price table");
        assert!(
            models.contains_key(shaped["model"].as_str().unwrap()),
            "the table lacks the selected model"
        );
        let table: std::collections::BTreeMap<String, [f64; 4]> = models
            .iter()
            .map(|(model, row)| {
                let leg = |name: &str| row[name].as_f64().expect("a priced leg");
                (
                    model.clone(),
                    [
                        leg("input"),
                        leg("cache_read"),
                        leg("cache_write"),
                        leg("output"),
                    ],
                )
            })
            .collect();
        let digest = rayline_arc_routes::price_sha256(&table).unwrap();
        assert_eq!(prices["price_sha256"], json!(digest));
        for result in std::iter::once(shaped).chain(shaped["presets"].as_object().unwrap().values())
        {
            assert_eq!(result["identity"]["prices"]["price_sha256"], json!(digest));
        }
    }
}
