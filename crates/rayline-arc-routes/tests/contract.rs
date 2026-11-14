//! `parse` and `shape` against hand-written fixtures.

mod common;

use std::collections::BTreeMap;

use common::{
    ALIAS, LOCAL_MODEL, PRESET, answer, assert_core_accepts, catalog, id, manifest, request, turn,
};
use rayline_arc_routes::{
    ParseContext, Parsed, Refusal, RequestHeaders, offer_sha256, parse, price_sha256, sha256_hex,
    shape,
};
use serde_json::{Value, json};

fn headers() -> RequestHeaders {
    RequestHeaders {
        conversation_id: Some("conversation-1".to_owned()),
        branch_id: Some("branch-1".to_owned()),
        wire_format: Some("anthropic.messages.v1".to_owned()),
        cell: None,
    }
}

fn context() -> ParseContext {
    ParseContext {
        salt: "salt".to_owned(),
        as_of: "2026-01-02T03:04:05Z".to_owned(),
        nonce: "nonce".to_owned(),
    }
}

fn parsed(body: Value) -> Result<Parsed, Refusal> {
    parse(
        &catalog(),
        &serde_json::to_vec(&body).unwrap(),
        &headers(),
        &context(),
    )
}

/// The fixture request with the member at a JSON pointer set.
fn with(pointer: &str, value: Value) -> Value {
    let mut body = request();
    let (parent, key) = pointer.rsplit_once('/').expect("a JSON pointer");
    match body.pointer_mut(parent).expect("a fixture path") {
        Value::Object(map) => {
            map.insert(key.to_owned(), value);
        }
        Value::Array(items) => items[key.parse::<usize>().expect("an index")] = value,
        _ => panic!("{parent} holds no members"),
    }
    body
}

fn refused(body: Value) -> (u16, String) {
    let refusal = parsed(body).expect_err("a refusal");
    (refusal.status, refusal.code)
}

#[test]
fn a_request_becomes_the_decide_body() {
    let sent = request();
    let parsed = parsed(sent.clone()).unwrap();
    let expected = json!({
        "schema_version": "rayline.arc.policy-decision-request.v1",
        "package": {"alias": ALIAS, "package_sha256": sha256_hex(&manifest())},
        "episode_id_hash": sha256_hex(b"local-routes\0salt\0conversation-1\0branch-1\0"),
        "context_epoch": "epoch:0",
        "request_format": "anthropic_messages",
        "request": {"system": sent["system"], "tools": sent["tools"], "messages": sent["messages"]},
        "attribution": [{"message": 1, "action_id": id('a')}],
        "selection": {
            "available_action_ids": [id('a'), id('b'), id('c')],
            "operating_point": {"name": "swift", "cost_weight": 10.0},
            "tier_points": [{"name": "swift", "cost_weight": 10.0},
                            {"name": "sage", "cost_weight": 0.2222222222222222}],
            "last_served_action_id": id('a'),
        },
        "response_fields": ["price_per_mtok", "thinking"],
        "policy": {"preset": PRESET},
        "event": {"kind": "user_message", "basis": "marker_inferred", "compaction": null},
        "capacity": [
            {"action_id": id('b'), "placement": "cloud", "fits": null, "reason": null},
            {"action_id": id('a'), "placement": "cloud", "fits": null, "reason": null},
            {"action_id": id('c'), "placement": "local", "fits": false, "reason": "context_capacity"},
        ],
        "turn_key": sha256_hex(b"arc-turn-key.v1\nturn-1"),
        "cache_context": {"actions": [{
            "action_id": id('a'), "gap_s": 12.0, "ttl_s": 300.0, "ttl_source": "cache_control",
            "prompt_tokens": 900, "cache_read_tokens": 100, "cache_write_tokens": 700,
            "context_epoch": "epoch:0", "current_prefix_tokens": 950, "overlap_tokens": 900}]},
        "routing_prices": {
            "schema": "arc-routing-prices.v1", "basis": "caller", "exclusive": false,
            "store_version": "local-routes", "as_of": "2026-01-02T03:04:05Z",
            "rates": {LOCAL_MODEL: {"input": 0.0, "cache_read": 0.0, "cache_write": 0.0, "output": 0.25}}},
    });
    assert_eq!(serde_json::to_value(&parsed.decide).unwrap(), expected);

    let restate = parsed.restate;
    assert_eq!(restate.unknown, [id('f')]);
    assert_eq!(restate.offered, [id('b'), id('a'), id('c')]);
    assert_eq!(restate.workers[&id('c')], "local-qwen");
    assert!(restate.unapplied_keys.is_empty());
}

#[test]
fn the_sage_model_asks_at_the_sage_point_and_optional_members_are_omitted() {
    let mut body = with("/model", json!("arc-sage"));
    let execution = &mut body["routing"]["execution"];
    execution["last_served"] = Value::Null;
    execution["history"] = json!([]);
    execution["hold"] = json!({"scope": "action", "action_id": id('b')});
    execution["turn"] =
        json!({"id": "t", "attempt": 2, "epoch": "e", "retry_of": "rt_1", "reconsult": "capacity"});
    execution["targets"][1]["cache"] = Value::Null;
    body["routing"].as_object_mut().unwrap().remove("prices");
    let decide = serde_json::to_value(parsed(body).unwrap().decide).unwrap();
    assert_eq!(decide["selection"]["operating_point"]["name"], "sage");
    assert_eq!(decide["selection"]["held_action_id"], json!(id('b')));
    assert_eq!(decide["reconsult"], "capacity");
    for absent in ["cache_context", "routing_prices"] {
        assert!(decide.get(absent).is_none(), "{absent}");
    }
    assert!(decide["selection"].get("last_served_action_id").is_none());
}

#[test]
fn evidence_that_is_not_admitted_is_sent_unknown() {
    let mut body = request();
    let targets = &mut body["routing"]["execution"]["targets"];
    targets[1]["cache"]["ttl"]["basis"] = json!("caller_estimated");
    targets[1]["cache"]["usage"]["units"] = json!("provider:other");
    targets[1]["cache"]["residency"] = json!({"state": "resident", "provenance": "local_measured"});
    targets[2]["capacity"] = json!({"fits": true, "reason": null, "required": 1, "available": 2,
        "available_source": "runtime_observed", "units": "hf:test", "provenance": "caller_estimated"});
    targets[0]["capacity"] = json!({"fits": true, "reason": null, "required": null, "available": null,
        "available_source": null, "units": null, "provenance": "caller_bounded"});
    let decide = serde_json::to_value(parsed(body).unwrap().decide).unwrap();
    let entry = &decide["cache_context"]["actions"][0];
    // An estimated TTL, a usage in other units than the prefix, and a
    // residency on a cloud target are all left out; the prefix stays.
    assert_eq!(entry["ttl_s"], Value::Null);
    assert_eq!(entry["prompt_tokens"], Value::Null);
    assert_eq!(entry["current_prefix_tokens"], 950);
    assert!(entry.get("residency").is_none());
    // An estimated fit is unknown; a bounded one is a fit.
    assert_eq!(decide["capacity"][2]["fits"], Value::Null);
    assert_eq!(decide["capacity"][0]["fits"], true);
}

#[test]
fn the_wire_format_is_the_headers_else_the_bodys() {
    let responses = |header: Option<&str>| {
        let mut body = request();
        let map = body.as_object_mut().unwrap();
        map.remove("messages");
        map.remove("system");
        map.insert("instructions".to_owned(), json!("be brief"));
        map.insert(
            "input".to_owned(),
            json!([{"role": "user", "content": "first"},
                   {"type": "function_call", "name": "read", "arguments": "{}", "call_id": "c1"},
                   {"type": "function_call_output", "call_id": "c1", "output": "ok"}]),
        );
        map.insert("tools".to_owned(), Value::Null);
        let headers = RequestHeaders {
            wire_format: header.map(str::to_owned),
            ..headers()
        };
        let parsed = parse(
            &catalog(),
            &serde_json::to_vec(&body).unwrap(),
            &headers,
            &context(),
        )
        .unwrap();
        serde_json::to_value(parsed.decide).unwrap()
    };
    for header in [Some("openai.responses.v1"), None, Some("something.else")] {
        let decide = responses(header);
        assert_eq!(decide["request_format"], "openai_responses");
        assert_eq!(decide["request"]["instructions"], "be brief");
        assert_eq!(decide["request"]["tools"], json!([]));
        assert_eq!(decide["request"]["input"].as_array().unwrap().len(), 3);
    }
    let mut chat = request();
    chat.as_object_mut().unwrap().remove("system");
    let headers = RequestHeaders {
        wire_format: Some("openai.chat.v1".to_owned()),
        ..headers()
    };
    let decide = serde_json::to_value(
        parse(
            &catalog(),
            &serde_json::to_vec(&chat).unwrap(),
            &headers,
            &context(),
        )
        .unwrap()
        .decide,
    )
    .unwrap();
    assert_eq!(decide["request_format"], "openai_chat");
    assert!(decide["request"].get("system").is_none());
}

#[test]
fn an_episode_is_a_conversation_and_branch_under_a_salt() {
    let episode = |headers: &RequestHeaders, context: &ParseContext| {
        parse(
            &catalog(),
            &serde_json::to_vec(&request()).unwrap(),
            headers,
            context,
        )
        .unwrap()
        .decide
        .episode_id_hash
    };
    let base = episode(&headers(), &context());
    assert_eq!(
        base,
        episode(
            &headers(),
            &ParseContext {
                nonce: "other".to_owned(),
                ..context()
            }
        )
    );
    assert_ne!(
        base,
        episode(
            &headers(),
            &ParseContext {
                salt: "other".to_owned(),
                ..context()
            }
        )
    );
    let branch = RequestHeaders {
        branch_id: Some("branch-2".to_owned()),
        ..headers()
    };
    assert_ne!(base, episode(&branch, &context()));
    // A request naming no conversation is an episode of its own.
    let anonymous = RequestHeaders {
        conversation_id: None,
        ..headers()
    };
    assert_ne!(
        episode(&anonymous, &context()),
        episode(
            &anonymous,
            &ParseContext {
                nonce: "other".to_owned(),
                ..context()
            }
        )
    );
    // Any salt: one derived at run time, as the router derives its own.
    let salt = format!("{:x}", std::process::id());
    let fresh = ParseContext::now(&salt);
    assert_eq!(fresh.nonce.len(), 32);
    assert_eq!(fresh.as_of.len(), "2026-01-02T03:04:05Z".len());
    assert!(fresh.as_of.ends_with('Z'));
}

#[test]
fn requests_the_contract_refuses_name_their_code() {
    let execution = |key: &str, value: Value| with(&format!("/routing/execution/{key}"), value);
    let cases: Vec<(Value, u16, &str)> = vec![
        (json!([1, 2]), 400, "invalid_body"),
        (with("/model", json!(7)), 400, "invalid_body"),
        (with("/model", json!("arc-turbo-1.0")), 400, "unknown_model"),
        (
            with("/model", json!("rayline/arc-swift-1.0")),
            400,
            "unknown_model",
        ),
        (with("/model", json!("arc-1.0")), 400, "unknown_model"),
        (
            with("/model", json!("some/other-model")),
            400,
            "unknown_model",
        ),
        (with("/routing", json!("x")), 400, "routing_invalid"),
        (with("/routing/prices", json!({})), 400, "prices_invalid"),
        (
            with(
                "/routing/prices",
                json!({LOCAL_MODEL: {"input": 1, "extra": 2}}),
            ),
            400,
            "prices_invalid",
        ),
        (
            with("/routing/prices", json!({LOCAL_MODEL: {"input": 1}})),
            400,
            "prices_invalid",
        ),
        (
            with("/routing/prices", json!({LOCAL_MODEL: {"input": -1}})),
            400,
            "price_leg_invalid",
        ),
        (
            with("/routing/prices", json!({LOCAL_MODEL: {"input": "1"}})),
            400,
            "price_leg_invalid",
        ),
        (
            with("/routing/policy", json!({"preset": "nope"})),
            400,
            "policy_invalid",
        ),
        (
            execution("version", json!("arc-execution-context.v1")),
            400,
            "execution_invalid",
        ),
        (execution("extra", json!(1)), 400, "execution_invalid"),
        (
            execution("turn/attempt", json!(0)),
            400,
            "execution_invalid",
        ),
        (
            execution("turn/attempt", json!(1.0)),
            400,
            "execution_invalid",
        ),
        (
            execution("turn/attempt", json!(true)),
            400,
            "execution_invalid",
        ),
        (
            execution("turn/id", json!("has/slash")),
            400,
            "execution_invalid",
        ),
        (
            execution("turn/reconsult", json!("veto")),
            400,
            "execution_invalid",
        ),
        (
            execution("event/kind", json!(null)),
            400,
            "execution_invalid",
        ),
        (
            execution("event/kind", json!("compaction")),
            400,
            "execution_invalid",
        ),
        (
            execution(
                "event/compaction",
                json!({"summary_message": 0, "retained_from": null, "new": true}),
            ),
            400,
            "execution_invalid",
        ),
        (
            execution("targets/0/placement", json!("edge")),
            400,
            "execution_invalid",
        ),
        (
            execution("targets/0/action_id", json!(id('a'))),
            400,
            "execution_invalid",
        ),
        (
            execution("targets/1/cache/gap_s", json!(-1)),
            400,
            "execution_invalid",
        ),
        (
            execution("targets/1/cache/ttl/seconds", json!(0)),
            400,
            "execution_invalid",
        ),
        (
            execution("targets/2/capacity/reason", json!(null)),
            400,
            "execution_invalid",
        ),
        (
            execution("targets/1/cache/usage/prompt_tokens", json!(700)),
            400,
            "execution_counts_inconsistent",
        ),
        (
            execution("targets/1/cache/current_prefix/overlap_tokens", json!(951)),
            400,
            "execution_counts_inconsistent",
        ),
        (
            execution("history/0/message", json!(0)),
            400,
            "execution_attribution_invalid",
        ),
        (
            execution("history/0/message", json!(9)),
            400,
            "execution_attribution_invalid",
        ),
        (
            execution("history/0/action_id", json!(id('e'))),
            400,
            "execution_unknown_action",
        ),
        (
            execution(
                "expect",
                json!({"package_sha256": id('0'), "thinking_controls_sha256": null, "offer_sha256": null}),
            ),
            409,
            "execution_identity_mismatch",
        ),
        (
            execution(
                "expect",
                json!({"package_sha256": null, "thinking_controls_sha256": id('0'), "offer_sha256": null}),
            ),
            409,
            "execution_identity_mismatch",
        ),
        (
            execution(
                "last_served",
                json!({"action_id": id('b'), "worker": "opus", "message": 1}),
            ),
            400,
            "execution_last_served_conflict",
        ),
        (
            execution(
                "last_served",
                json!({"action_id": id('b'), "worker": "opus", "message": null}),
            ),
            400,
            "execution_last_served_conflict",
        ),
        (
            execution("hold", json!({"scope": "action", "action_id": id('c')})),
            409,
            "execution_hold_infeasible",
        ),
        (
            execution("hold", json!({"scope": "model", "action_id": id('a')})),
            400,
            "execution_invalid",
        ),
    ];
    for (index, (body, status, code)) in cases.into_iter().enumerate() {
        assert_eq!(refused(body), (status, code.to_owned()), "case {index}");
    }

    // A member this contract knows nothing of is refused, never ignored.
    let mut unknown = request();
    unknown["routing"]["shortcut"] = json!(true);
    assert_eq!(refused(unknown), (400, "routing_unknown_member".to_owned()));
    // Members validated in the contract's order, each with its own code.
    for (member, value, code) in [
        ("value_usd", json!("0.1"), "value_invalid"),
        ("value_usd", json!(31), "value_out_of_bounds"),
        ("value_usd", json!(4.5), "tier_value_immutable"),
        ("temperature", json!(-0.5), "temperature_invalid"),
        ("models", json!(["a", "a"]), "models_invalid"),
        ("excluded_models", json!([""]), "models_invalid"),
        (
            "price_profile",
            json!({"id": "-bad"}),
            "price_profile_invalid",
        ),
        (
            "price_profile",
            json!({"id": "ok", "version": 1.5}),
            "price_profile_invalid",
        ),
        ("response_fields", json!([]), "response_fields_invalid"),
    ] {
        let mut body = request();
        body["routing"][member] = value;
        assert_eq!(refused(body), (400, code.to_owned()), "{member}");
    }
    // The envelope and the policy are both required here.
    for member in ["execution", "policy"] {
        let mut body = request();
        body["routing"].as_object_mut().unwrap().remove(member);
        assert_eq!(
            refused(body),
            (400, "execution_invalid".to_owned()),
            "{member}"
        );
    }
    let mut bare = request();
    bare.as_object_mut().unwrap().remove("routing");
    assert_eq!(refused(bare), (400, "execution_invalid".to_owned()));
    // A hold on an action that is not a target is not offered.
    let mut body = request();
    body["routing"]["execution"]["targets"]
        .as_array_mut()
        .unwrap()
        .remove(0);
    body["routing"]["execution"]["hold"] = json!({"scope": "action", "action_id": id('b')});
    assert_eq!(
        refused(body),
        (409, "execution_hold_unavailable".to_owned())
    );
}

#[test]
fn members_this_router_does_not_apply_are_named_unapplied() {
    let mut body = request();
    body["routing"]["value_usd"] = json!(0.1);
    body["routing"]["temperature"] = json!(0);
    body["routing"]["models"] = json!(["anthropic/*"]);
    body["routing"]["price_profile"] = json!({"id": "team", "version": 3.0});
    body["routing"]["response_fields"] = json!(["predictions"]);
    let parsed = parsed(body).unwrap();
    assert_eq!(
        parsed.restate.unapplied_keys,
        [
            "value_usd",
            "temperature",
            "models",
            "price_profile",
            "response_fields"
        ]
    );
    let shaped = shape(&catalog(), &parsed.restate, &answer(), "rt_x", 1).unwrap();
    assert_eq!(
        shaped["routing"]["unapplied_keys"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
}

/// The checks the execution core runs on an answer before acting on it.
#[test]
fn the_answer_is_one_the_core_validates() {
    let sent = request();
    let parsed = parsed(sent.clone()).unwrap();
    let shaped = shape(&catalog(), &parsed.restate, &answer(), "rt_test", 42).unwrap();
    assert_core_accepts(&shaped, &sent);

    // The top-level result is the requested tier's.
    let swift = &shaped["presets"]["swift"];
    for key in [
        "model",
        "worker",
        "thinking",
        "thinking_level",
        "policy_action_id",
        "identity",
        "receipt",
    ] {
        assert_eq!(shaped[key], swift[key], "{key}");
    }
    assert_eq!(shaped["route_id"], "rt_test");
    assert_eq!(shaped["latency_ms"], 42);
    assert_eq!(shaped["model"], "anthropic/claude-haiku-5.5");
    assert_eq!(shaped["worker"], "haiku");
    assert_eq!(shaped["thinking_level"], "none");
    assert_eq!(swift["v_value"], 0.1);
    assert_eq!(swift["cost_weight"], 10.0);
    assert_eq!(swift["identity"]["arm_id"], json!(id('7')));
    let sage = &shaped["presets"]["sage"];
    assert_eq!(sage["model"], "anthropic/claude-opus-5.5");
    assert_eq!(
        sage["thinking"],
        json!({"mode": "on", "budget_tokens": 8000})
    );
    assert_eq!(sage["v_value"], 4.5);
    assert_eq!(
        shaped["usage"],
        json!({"encoded_input_tokens": 1234, "cache_read_tokens": 0})
    );
    assert_eq!(
        shaped["checkpoint"],
        json!(format!("local.{ALIAS}.{}", &catalog().package_sha256[..16]))
    );
    assert_eq!(
        shaped["alternatives"],
        json!([{"model": "anthropic/claude-opus-5.5", "worker": "opus", "score": 0.75}])
    );
    // A target outside the package is named in every receipt.
    assert!(
        shaped["receipt"]["excluded"]
            .as_array()
            .unwrap()
            .contains(&json!({"action_id": id('f'), "reason": "unknown_action"}))
    );
    assert_eq!(
        shaped["routing"]["execution"]["pinned"],
        json!({"package_sha256": "unpinned", "thinking_controls_sha256": "unpinned", "offer_sha256": "unpinned"})
    );

    // The price world: the caller's row at the caller's legs, the rest at
    // the rates the decision used, under a digest of exactly that table.
    let prices = &shaped["routing"]["prices"];
    assert_eq!(prices["status"], "applied");
    assert_eq!(prices["unused"], json!([]));
    let models = prices["models"].as_object().unwrap();
    assert_eq!(
        models[LOCAL_MODEL],
        json!({"input": 0.0, "cache_read": 0.0, "cache_write": 0.0, "output": 0.25,
               "source": {"input": "caller", "cache_read": "caller", "cache_write": "caller", "output": "caller"}})
    );
    assert_eq!(
        models["anthropic/claude-haiku-5.5"]["source"]["input"],
        "catalog"
    );
    assert!(models.contains_key(shaped["model"].as_str().unwrap()));
    let table: BTreeMap<String, [f64; 4]> = models
        .iter()
        .map(|(model, row)| {
            let leg = |name: &str| row[name].as_f64().unwrap();
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
    let digest = price_sha256(&table).unwrap();
    assert_eq!(prices["price_sha256"], json!(digest));
    for result in [&shaped, swift, sage] {
        assert_eq!(
            result["identity"]["prices"],
            json!({"basis": "caller", "price_sha256": digest})
        );
    }
    assert_eq!(swift["prices"], swift["identity"]["prices"]);
    assert_eq!(
        shaped["selected_pricing"],
        json!({"input": 1.0, "cache_read": 0.1, "cache_write": 0, "output": 5.0, "pricing_source": "catalog"})
    );
    assert_eq!(
        shaped["identity"]["offer_sha256"],
        json!(offer_sha256(
            &catalog().package_sha256,
            &[id('a'), id('b'), id('c')]
        ))
    );
}

#[test]
fn a_caller_row_the_decision_did_not_use_is_unused_not_applied() {
    let mut sent = request();
    sent["routing"]["prices"][LOCAL_MODEL] =
        json!({"input": 9, "cache_read": 9, "cache_write": 9, "output": 9});
    let parsed = parsed(sent.clone()).unwrap();
    // The decider priced this model at its catalog rate: no caller key.
    let mut answer = answer();
    answer["actions"][2]
        .as_object_mut()
        .unwrap()
        .remove("price_key");
    let shaped = shape(&catalog(), &parsed.restate, &answer, "rt_test", 1).unwrap();
    let prices = &shaped["routing"]["prices"];
    assert_eq!(prices["unused"], json!([LOCAL_MODEL]));
    let row = &prices["models"][LOCAL_MODEL];
    assert_eq!(row["output"], 0.25);
    assert_eq!(row["source"]["output"], "catalog");
    // With no caller row applied, every result's prices are the catalog's.
    assert_eq!(shaped["identity"]["prices"]["basis"], "catalog");
    assert_core_accepts(&shaped, &sent);
}

#[test]
fn an_unused_caller_alias_never_renames_a_model() {
    let mut sent = request();
    // A caller key that names the selected model under another vendor; the
    // decider didn't price by it.
    sent["routing"]["prices"]["other/claude-haiku-5.5"] =
        json!({"input": 7, "cache_read": 7, "cache_write": 7, "output": 7});
    let parsed = parsed(sent.clone()).unwrap();
    let shaped = shape(&catalog(), &parsed.restate, &answer(), "rt_test", 1).unwrap();
    assert_eq!(shaped["model"], "anthropic/claude-haiku-5.5");
    let prices = &shaped["routing"]["prices"];
    assert!(prices["models"]["anthropic/claude-haiku-5.5"].is_object());
    assert_eq!(prices["unused"], json!(["other/claude-haiku-5.5"]));
    assert_eq!(shaped["selected_pricing"]["input"], 1.0);
    assert_core_accepts(&shaped, &sent);
}

#[test]
fn without_caller_prices_the_answer_is_in_the_catalog_price_world() {
    let mut sent = request();
    sent["routing"].as_object_mut().unwrap().remove("prices");
    let parsed = parsed(sent.clone()).unwrap();
    let shaped = shape(&catalog(), &parsed.restate, &answer(), "rt_test", 1).unwrap();
    assert_core_accepts(&shaped, &sent);
    assert!(shaped["routing"].get("prices").is_none());
    assert_eq!(
        shaped["identity"]["prices"],
        json!({"basis": "catalog", "price_sha256": null})
    );
}

#[test]
fn a_priced_model_no_offered_action_runs_is_unused() {
    let mut sent = request();
    sent["routing"]["prices"]["vendor/unoffered"] =
        json!({"input": 1, "cache_read": 1, "cache_write": 1, "output": 1});
    let parsed = parsed(sent).unwrap();
    let shaped = shape(&catalog(), &parsed.restate, &answer(), "rt_test", 1).unwrap();
    assert_eq!(
        shaped["routing"]["prices"]["unused"],
        json!(["vendor/unoffered"])
    );
    assert!(
        shaped["routing"]["prices"]["models"]
            .get("vendor/unoffered")
            .is_none()
    );
}

#[test]
fn a_pinned_offer_is_confirmed_or_refused() {
    let offer = offer_sha256(&catalog().package_sha256, &[id('a'), id('b'), id('c')]);
    let pinned = |offer: &str| {
        let sent = with(
            "/routing/execution/expect",
            json!({"package_sha256": catalog().package_sha256,
                   "thinking_controls_sha256": id('9'), "offer_sha256": offer}),
        );
        let parsed = parsed(sent).unwrap();
        shape(&catalog(), &parsed.restate, &answer(), "rt_test", 1)
    };
    assert_eq!(
        pinned(&offer).unwrap()["routing"]["execution"]["pinned"],
        json!({"package_sha256": "matched", "thinking_controls_sha256": "matched", "offer_sha256": "matched"})
    );
    let refusal = pinned(&id('0')).unwrap_err();
    assert_eq!(
        (refusal.status, refusal.code.as_str()),
        (409, "execution_identity_mismatch")
    );
}

#[test]
fn a_decision_without_a_receipt_did_not_apply_the_envelope() {
    let parsed = parsed(request()).unwrap();
    let mut unsupported = answer();
    unsupported["decision"]
        .as_object_mut()
        .unwrap()
        .remove("receipt");
    let shaped = shape(&catalog(), &parsed.restate, &unsupported, "rt_test", 1).unwrap();
    assert_eq!(shaped["routing"]["execution"]["status"], "unsupported");
    assert_eq!(shaped["routing"]["execution"]["pinned"], Value::Null);
    assert_eq!(shaped["routing"]["execution"]["turn"], turn());
    assert_eq!(shaped["routing"]["policy"]["status"], "unsupported");
}

#[test]
fn a_decide_answer_that_cannot_be_restated_is_refused_not_patched() {
    let parsed = parsed(request()).unwrap();
    let broken = |edit: fn(&mut Value)| {
        let mut answer = answer();
        edit(&mut answer);
        let refusal = shape(&catalog(), &parsed.restate, &answer, "rt_test", 1).unwrap_err();
        (refusal.status, refusal.code)
    };
    let upstream = (502, "inconsistent_upstream".to_owned());
    assert_eq!(broken(|a| a["decision"] = Value::Null), upstream);
    assert_eq!(broken(|a| a["actions"] = Value::Null), upstream);
    assert_eq!(
        broken(|a| {
            a["decision"]["tier_picks"]
                .as_object_mut()
                .unwrap()
                .remove("swift");
        }),
        upstream
    );
    assert_eq!(
        broken(
            |a| a["decision"]["tier_picks"]["sage"]["selected_action_id"] = json!("e".repeat(64))
        ),
        upstream
    );
    assert_eq!(
        broken(|a| a["actions"][0]["price_per_mtok"]["output"] = Value::Null),
        upstream
    );
}
