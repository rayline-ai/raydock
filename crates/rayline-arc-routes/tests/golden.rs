//! Parity with recorded traffic and a reference implementation's output.
//!
//! The recordings hold real requests, so they are never committed: these
//! tests read them from paths the environment names and are ignored by
//! default.
//!
//! ```text
//! RAYLINE_ARC_ROUTES_PACKAGE=<package.json> \
//! RAYLINE_ARC_ROUTES_GOLDEN=<golden.jsonl> \
//! RAYLINE_ARC_ROUTES_RECORDED=<routes.jsonl> \
//!   cargo test -p rayline-arc-routes --test golden -- --ignored
//! ```
//!
//! A golden line is `{salt, request, headers, decide, answer, shaped}`: one
//! recorded `/v1/routes` request, the decide body the reference builds for
//! it, the decide answer it got and the V1 answer it shaped. A recorded line
//! is `{path, request, response}` as a hosted router answered it.

use std::collections::BTreeMap;

use rayline_arc_routes::{Catalog, ParseContext, RequestHeaders, parse, price_sha256, shape};
use serde_json::Value;

fn env_path(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

fn lines(path: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .expect("readable recording")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a JSON line"))
        .collect()
}

/// The paths at which two values differ. Paths only: values are private.
fn differences(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(a), Value::Object(b)) => {
            let keys: std::collections::BTreeSet<&String> = a.keys().chain(b.keys()).collect();
            for key in keys {
                match (a.get(key), b.get(key)) {
                    (Some(x), Some(y)) => differences(&format!("{path}.{key}"), x, y, out),
                    (Some(_), None) => out.push(format!("{path}.{key} (only ours)")),
                    _ => out.push(format!("{path}.{key} (only theirs)")),
                }
            }
        }
        (Value::Array(a), Value::Array(b)) if a.len() == b.len() => {
            for (i, (x, y)) in a.iter().zip(b).enumerate() {
                differences(&format!("{path}[{i}]"), x, y, out);
            }
        }
        _ if left != right => out.push(path.to_owned()),
        _ => {}
    }
}

fn header(headers: &Value, name: &str) -> Option<String> {
    headers.get(name).and_then(Value::as_str).map(str::to_owned)
}

#[test]
#[ignore = "reads private recordings named by RAYLINE_ARC_ROUTES_GOLDEN"]
fn recorded_requests_parse_and_shape_as_the_reference_does() {
    let manifest = std::fs::read(env_path("RAYLINE_ARC_ROUTES_PACKAGE")).expect("package.json");
    let golden = lines(&env_path("RAYLINE_ARC_ROUTES_GOLDEN"));
    assert!(!golden.is_empty());
    let mut failures = Vec::new();
    for (index, line) in golden.iter().enumerate() {
        let mut expected = line["decide"].clone();
        let alias = expected["package"]["alias"].as_str().expect("alias");
        let catalog = Catalog::from_manifest(&manifest, alias).expect("catalog");
        let headers = RequestHeaders {
            conversation_id: header(&line["headers"], "x-conversation-id"),
            branch_id: header(&line["headers"], "x-rayline-branch-id"),
            wire_format: header(&line["headers"], "x-rayline-wire-format"),
            cell: header(&line["headers"], "x-rayline-cell"),
        };
        let context = ParseContext {
            salt: line["salt"].as_str().expect("salt").to_owned(),
            as_of: String::new(),
            nonce: String::new(),
        };
        let parsed = match parse(
            &catalog,
            &serde_json::to_vec(&line["request"]).unwrap(),
            &headers,
            &context,
        ) {
            Ok(parsed) => parsed,
            Err(refusal) => {
                failures.push(format!("line {index}: refused {}", refusal.code));
                continue;
            }
        };
        let mut ours = serde_json::to_value(&parsed.decide).expect("serializable");
        // The one member that is a clock reading.
        for body in [&mut ours, &mut expected] {
            if let Some(prices) = body
                .get_mut("routing_prices")
                .and_then(Value::as_object_mut)
            {
                prices.remove("as_of");
            }
        }
        let mut out = Vec::new();
        differences("decide", &ours, &expected, &mut out);

        let theirs = &line["shaped"];
        let route_id = theirs["route_id"].as_str().expect("route_id");
        let latency = theirs["latency_ms"].as_u64().expect("latency_ms");
        match shape(
            &catalog,
            &parsed.restate,
            &line["answer"],
            route_id,
            latency,
        ) {
            Ok(shaped) => differences("answer", &shaped, theirs, &mut out),
            Err(refusal) => out.push(format!("shape refused {}", refusal.code)),
        }
        if !out.is_empty() {
            failures.push(format!("line {index}: {}", out.join(", ")));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("{} golden pairs match", golden.len());
}

#[test]
#[ignore = "reads private recordings named by RAYLINE_ARC_ROUTES_RECORDED"]
fn price_digest_matches_what_a_hosted_router_answered() {
    let mut checked = 0;
    for line in lines(&env_path("RAYLINE_ARC_ROUTES_RECORDED")) {
        let prices = &line["response"]["routing"]["prices"];
        let Some(models) = prices["models"].as_object() else {
            continue;
        };
        let table: BTreeMap<String, [f64; 4]> = models
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
        assert_eq!(
            price_sha256(&table).as_deref(),
            prices["price_sha256"].as_str()
        );
        checked += 1;
    }
    assert!(checked > 0);
    println!("{checked} hosted price digests match");
}
