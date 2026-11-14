//! Canonical JSON (RFC 8785) and the two digests an answer carries.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::PRICE_LEGS;

const ROUTE_PRICES_SCHEMA: &str = "arc-route-prices.v1";
const OFFER_SCHEMA: &str = "arc-offer.v1";

pub fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        out.push(HEX[usize::from(byte >> 4)] as char);
        out.push(HEX[usize::from(byte & 15)] as char);
    }
    out
}

/// ECMAScript `Number::toString` (RFC 8785 section 3.2.2.3) of a finite number.
fn es_number(number: f64) -> Option<String> {
    if !number.is_finite() {
        return None;
    }
    if number == 0.0 {
        return Some("0".to_owned());
    }
    // Rust's `{:e}` writes the shortest digits that round-trip: `d[.ddd]e<exp>`.
    let scientific = format!("{:e}", number.abs());
    let (mantissa, exponent) = scientific.split_once('e')?;
    let exponent: i64 = exponent.parse().ok()?;
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let k = i64::try_from(digits.len()).ok()?;
    let n = exponent + 1; // value = 0.digits x 10^n
    let text = if k <= n && n <= 21 {
        format!("{digits}{}", "0".repeat(usize::try_from(n - k).ok()?))
    } else if 0 < n && n <= 21 {
        let (whole, fraction) = digits.split_at(usize::try_from(n).ok()?);
        format!("{whole}.{fraction}")
    } else if -6 < n && n <= 0 {
        format!("0.{}{digits}", "0".repeat(usize::try_from(-n).ok()?))
    } else {
        let e = n - 1;
        let (first, rest) = digits.split_at(1);
        let mantissa = if rest.is_empty() {
            first.to_owned()
        } else {
            format!("{first}.{rest}")
        };
        format!("{mantissa}e{}{}", if e >= 0 { '+' } else { '-' }, e.abs())
    };
    Some(if number < 0.0 {
        format!("-{text}")
    } else {
        text
    })
}

/// RFC 8785 canonical JSON. `None` only for a number with no finite `f64`.
pub fn jcs(value: &Value) -> Option<String> {
    Some(match value {
        Value::Null | Value::Bool(_) | Value::String(_) => value.to_string(),
        Value::Number(number) => es_number(number.as_f64()?)?,
        Value::Array(items) => {
            let items: Option<Vec<String>> = items.iter().map(jcs).collect();
            format!("[{}]", items?.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by_cached_key(|key| key.encode_utf16().collect::<Vec<u16>>());
            let mut members = Vec::with_capacity(keys.len());
            for key in keys {
                let name = Value::String(key.clone()).to_string();
                members.push(format!("{name}:{}", jcs(&map[key.as_str()])?));
            }
            format!("{{{}}}", members.join(","))
        }
    })
}

/// The digest of a resolved price table: `sha256(JCS({schema, prices}))` over
/// every model's four legs. Spelling (`1` or `1.0`) and key order do not move it.
pub fn price_sha256(prices: &BTreeMap<String, [f64; 4]>) -> Option<String> {
    let table: serde_json::Map<String, Value> = prices
        .iter()
        .map(|(model, legs)| {
            let row: serde_json::Map<String, Value> = PRICE_LEGS
                .iter()
                .zip(legs)
                .map(|(leg, price)| ((*leg).to_owned(), json!(price)))
                .collect();
            (model.clone(), Value::Object(row))
        })
        .collect();
    let text = jcs(&json!({"schema": ROUTE_PRICES_SCHEMA, "prices": table}))?;
    Some(sha256_hex(text.as_bytes()))
}

/// The digest of an offer: the package and its sorted, distinct offered actions.
pub fn offer_sha256(package_sha256: &str, action_ids: &[String]) -> String {
    let mut ids: Vec<&String> = action_ids.iter().collect();
    ids.sort();
    ids.dedup();
    // Strings and arrays only: canonical JSON of this value always exists.
    let text =
        jcs(&json!({"schema": OFFER_SCHEMA, "package_sha256": package_sha256, "action_ids": ids}))
            .unwrap_or_default();
    sha256_hex(text.as_bytes())
}

/// The key a turn's decision derives from, stable across attempts.
pub fn turn_key(turn_id: &str) -> String {
    sha256_hex(format!("arc-turn-key.v1\n{turn_id}").as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_written_as_ecmascript_writes_them() {
        for (number, text) in [
            (0.0, "0"),
            (-0.0, "0"),
            (1.0, "1"),
            (10.0, "10"),
            (0.1, "0.1"),
            (2.93, "2.93"),
            (0.00294, "0.00294"),
            (-12.5, "-12.5"),
            (1e21, "1e+21"),
            (1e20, "100000000000000000000"),
            (123456789012345680000.0, "123456789012345680000"),
            (1.5e-7, "1.5e-7"),
            (0.000001, "0.000001"),
            (0.0000001, "1e-7"),
            (5e-324, "5e-324"),
            (1.7976931348623157e308, "1.7976931348623157e+308"),
            (0.2222222222222222, "0.2222222222222222"),
        ] {
            assert_eq!(es_number(number).as_deref(), Some(text), "{number}");
        }
        assert_eq!(es_number(f64::NAN), None);
        assert_eq!(es_number(f64::INFINITY), None);
    }

    #[test]
    fn canonical_json_sorts_keys_by_utf16_and_drops_number_spelling() {
        let value: Value = serde_json::from_str(
            r#"{"b": [1.0, 2, "x\n\"y"], "a": {"€": null, "😀": true, "z": 1e2}}"#,
        )
        .unwrap();
        assert_eq!(
            jcs(&value).unwrap(),
            "{\"a\":{\"z\":100,\"€\":null,\"😀\":true},\"b\":[1,2,\"x\\n\\\"y\"]}"
        );
    }

    #[test]
    fn price_digest_matches_the_reference_vector() {
        let mut prices = BTreeMap::new();
        prices.insert("vendor/model-a".to_owned(), [1.25, 0.125, 1.5, 10.0]);
        prices.insert("local/model-b".to_owned(), [0.0, 0.0, 0.0, 0.0]);
        // The digest below is sha256 of this canonical text, computed independently.
        let text = jcs(&json!({"schema": "arc-route-prices.v1", "prices": {
            "vendor/model-a": {"input": 1.25, "cache_read": 0.125, "cache_write": 1.5, "output": 10},
            "local/model-b": {"input": 0, "cache_read": 0, "cache_write": 0, "output": 0}}}))
        .unwrap();
        assert_eq!(
            text,
            "{\"prices\":{\"local/model-b\":{\"cache_read\":0,\"cache_write\":0,\"input\":0,\"output\":0},\
             \"vendor/model-a\":{\"cache_read\":0.125,\"cache_write\":1.5,\"input\":1.25,\"output\":10}},\
             \"schema\":\"arc-route-prices.v1\"}"
        );
        assert_eq!(
            price_sha256(&prices).unwrap(),
            "52768c61cf608fc01c0e78844d93994a3ab43fd2e4e8f05ee8d43346c6391f82"
        );
    }

    #[test]
    fn offer_digest_ignores_order_and_repeats() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let package = "c".repeat(64);
        let one = offer_sha256(&package, &[b.clone(), a.clone(), b.clone()]);
        assert_eq!(one, offer_sha256(&package, &[a.clone(), b.clone()]));
        assert_ne!(one, offer_sha256(&package, &[a]));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
