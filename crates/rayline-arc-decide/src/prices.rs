//! The rates one decision scores with: the release's own, or a request's
//! `routing_prices` mapped onto its arms.
//!
//! A price table names a model by its price id, or by `<id>@<endpoint>` for
//! an arm whose worker pins one endpoint. A caller's own table (`basis:
//! caller`) is applied as sent and, when `exclusive`, makes every arm it
//! does not price unavailable. A feed's table falls back, arm by arm, to
//! the release's rate wherever it has no usable entry, and arms that must
//! be priced alike fall back together.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::error::{DecideError, invalid, refuse, unsupported};
use crate::release::{PRICE_LEGS, Rates, Release};
use crate::stage_one::LEGS;
use crate::wire::RoutingPrices;

/// The largest rate (dollars per million tokens) a request may state.
const MAX_RATE: f64 = 1e9;

#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedPrices {
    /// Every arm's rates.
    pub rates: Vec<Rates>,
    /// Arms a caller's exclusive table left unpriced.
    pub excluded: Vec<bool>,
}

fn leg(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|rate| rate.is_finite() && (0.0..=MAX_RATE).contains(rate))
}

/// An RFC 3339 timestamp with an offset, as nanoseconds since the epoch.
fn instant(text: &str) -> Option<i128> {
    let bytes = text.as_bytes();
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = bytes.get(range)?;
        part.iter()
            .all(u8::is_ascii_digit)
            .then(|| part.iter().fold(0i64, |n, d| n * 10 + i64::from(d - b'0')))
    };
    let separators = [(4, b'-'), (7, b'-'), (10, b'T'), (13, b':'), (16, b':')];
    if separators
        .iter()
        .any(|(at, sep)| bytes.get(*at) != Some(sep))
    {
        return None;
    }
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let (hour, minute, second) = (digits(11..13)?, digits(14..16)?, digits(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let mut at = 19;
    let mut nanos = 0i128;
    if bytes.get(at) == Some(&b'.') {
        let start = at + 1;
        let end = start
            + bytes[start..]
                .iter()
                .take_while(|b| b.is_ascii_digit())
                .count();
        if end == start || end - start > 9 {
            return None;
        }
        nanos = i128::from(digits(start..end)?) * 10i128.pow((9 - (end - start)) as u32);
        at = end;
    }
    let offset = match bytes.get(at)? {
        b'Z' if at + 1 == bytes.len() => 0,
        sign @ (b'+' | b'-') if at + 6 == bytes.len() && bytes[at + 3] == b':' => {
            let seconds = digits(at + 1..at + 3)? * 3600 + digits(at + 4..at + 6)? * 60;
            if *sign == b'+' { seconds } else { -seconds }
        }
        _ => return None,
    };
    // Days from the civil date (proleptic Gregorian).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let year_of_era = y.rem_euclid(400);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    Some(i128::from(seconds) * 1_000_000_000 + nanos)
}

/// A feed entry's four legs, or `None` when the arm must keep the release's
/// rate: no entry, a missing or unusable leg, or an entry that expired
/// before the request's `as_of`. A null leg takes the release's leg.
fn live_rate(entry: Option<&Value>, package: &Rates, as_of: i128) -> Option<Rates> {
    let entry = entry?.as_object()?;
    let mut legs = [None; LEGS];
    for ((slot, name), base) in legs.iter_mut().zip(PRICE_LEGS).zip(package) {
        *slot = match entry.get(name)? {
            Value::Null => *base,
            value => Some(leg(value)?),
        };
    }
    match entry.get("valid_until") {
        None | Some(Value::Null) => {}
        Some(until) => {
            if instant(until.as_str()?)? < as_of {
                return None;
            }
        }
    }
    Some(legs)
}

/// The key of `rates` an arm reads: its endpoint's when the table has one,
/// else its model's.
fn rate_key(release: &Release, arm: usize, has: impl Fn(&str) -> bool) -> String {
    let arm = &release.arms[arm];
    if let Some(tag) = &arm.endpoint_tag {
        let tagged = format!("{}@{tag}", arm.model_id);
        if has(&tagged) {
            return tagged;
        }
    }
    arm.model_id.clone()
}

/// What must be priced one way: a rate key, a stage-one candidate, a group.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Unit {
    Key(String),
    Candidate(String),
    Group(String),
}

fn units(release: &Release, arm: usize, key: Option<&str>) -> Vec<Unit> {
    let record = &release.arms[arm];
    let mut out = Vec::with_capacity(3);
    if let Some(key) = key {
        out.push(Unit::Key(key.to_owned()));
    }
    if record.served {
        out.push(Unit::Candidate(record.model.clone()));
    }
    if let Some(group) = &record.group {
        out.push(Unit::Group(group.clone()));
    }
    out
}

pub fn resolve(
    release: &Release,
    routing: Option<&RoutingPrices>,
) -> Result<ResolvedPrices, DecideError> {
    let count = release.arms.len();
    let package = release
        .package_rates
        .clone()
        .unwrap_or_else(|| vec![[None; LEGS]; count]);
    let Some(routing) = routing else {
        return Ok(ResolvedPrices {
            rates: package,
            excluded: vec![false; count],
        });
    };
    match routing.basis.as_deref() {
        Some("caller") => caller(release, routing, package),
        Some(_) => Err(invalid("routing_prices")),
        None if routing.exclusive.is_some() => Err(invalid("routing_prices")),
        None => feed(release, routing, package),
    }
}

fn caller(
    release: &Release,
    routing: &RoutingPrices,
    package: Vec<Rates>,
) -> Result<ResolvedPrices, DecideError> {
    let mut table: HashMap<&str, Rates> = HashMap::new();
    for (key, entry) in &routing.rates {
        let entry = entry
            .as_object()
            .filter(|entry| entry.len() == LEGS)
            .ok_or(invalid("routing_prices"))?;
        let mut legs = [None; LEGS];
        for (slot, name) in legs.iter_mut().zip(PRICE_LEGS) {
            *slot = Some(
                entry
                    .get(name)
                    .and_then(leg)
                    .ok_or(invalid("routing_prices"))?,
            );
        }
        table.insert(key, legs);
    }
    if table.is_empty() {
        return Err(invalid("routing_prices"));
    }
    if !release.live_prices {
        return Err(unsupported("caller_prices_unsupported"));
    }
    let exclusive = routing.exclusive.unwrap_or(false);
    let count = release.arms.len();
    let keys: Vec<String> = (0..count)
        .map(|arm| rate_key(release, arm, |key| table.contains_key(key)))
        .collect();
    let read: HashSet<&str> = keys.iter().map(String::as_str).collect();
    if table.keys().any(|key| !read.contains(key)) {
        return Err(refuse("unknown_price_model"));
    }
    let mut rates = package;
    let mut excluded = vec![false; count];
    for arm in 0..count {
        match table.get(keys[arm].as_str()) {
            Some(legs) => rates[arm] = *legs,
            None => excluded[arm] = exclusive,
        }
    }
    let mut priced: HashMap<Unit, Rates> = HashMap::new();
    for arm in (0..count).filter(|arm| !excluded[*arm]) {
        for unit in units(release, arm, None) {
            if *priced.entry(unit).or_insert(rates[arm]) != rates[arm] {
                return Err(unsupported("caller_prices_inconsistent"));
            }
        }
    }
    Ok(ResolvedPrices { rates, excluded })
}

fn feed(
    release: &Release,
    routing: &RoutingPrices,
    package: Vec<Rates>,
) -> Result<ResolvedPrices, DecideError> {
    let as_of = routing
        .as_of
        .as_deref()
        .and_then(instant)
        .ok_or(invalid("routing_prices"))?;
    let count = release.arms.len();
    let none = ResolvedPrices {
        rates: package.clone(),
        excluded: vec![false; count],
    };
    if !release.live_prices {
        return Ok(none);
    }
    let keys: Vec<String> = (0..count)
        .map(|arm| rate_key(release, arm, |key| routing.rates.contains_key(key)))
        .collect();
    let live: Vec<Option<Rates>> = (0..count)
        .map(|arm| live_rate(routing.rates.get(&keys[arm]), &package[arm], as_of))
        .collect();
    let arm_units: Vec<Vec<Unit>> = (0..count)
        .map(|arm| units(release, arm, Some(&keys[arm])))
        .collect();
    let mut fallen: Vec<bool> = live.iter().map(Option::is_none).collect();
    loop {
        let mut closed: HashSet<&Unit> = HashSet::new();
        let mut shared: HashMap<&Unit, Rates> = HashMap::new();
        for arm in 0..count {
            for unit in &arm_units[arm] {
                match live[arm].filter(|_| !fallen[arm]) {
                    None => {
                        closed.insert(unit);
                    }
                    Some(legs) => {
                        if *shared.entry(unit).or_insert(legs) != legs {
                            closed.insert(unit);
                        }
                    }
                }
            }
        }
        let more: Vec<usize> = (0..count)
            .filter(|arm| !fallen[*arm] && arm_units[*arm].iter().any(|unit| closed.contains(unit)))
            .collect();
        if more.is_empty() {
            break;
        }
        for arm in more {
            fallen[arm] = true;
        }
    }
    let rates = (0..count)
        .map(|arm| match live[arm] {
            Some(legs) if !fallen[arm] => legs,
            _ => package[arm],
        })
        .collect();
    Ok(ResolvedPrices {
        rates,
        excluded: none.excluded,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn instants_compare_across_offsets_and_fractions() {
        let utc = instant("2026-01-01T00:00:00Z").unwrap();
        assert_eq!(utc, instant("2026-01-01T00:00:00+00:00").unwrap());
        assert_eq!(utc, instant("2026-01-01T02:00:00+02:00").unwrap());
        assert_eq!(
            utc + 500_000_000,
            instant("2026-01-01T00:00:00.5Z").unwrap()
        );
        assert_eq!(instant("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            instant("2024-03-01T00:00:00Z"),
            Some(1_709_251_200 * 1_000_000_000)
        );
        for bad in [
            "2026-01-01",
            "2026-01-01T00:00:00",
            "2026-13-01T00:00:00Z",
            "2026-01-01T00:00:00.Z",
        ] {
            assert_eq!(instant(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_feed_entry_fills_null_legs_and_expires() {
        let package = [Some(1.0), Some(0.1), None, Some(4.0)];
        let entry = json!({"input": 2.0, "cache_read": null, "cache_write": null, "output": 8.0});
        assert_eq!(
            live_rate(Some(&entry), &package, 0),
            Some([Some(2.0), Some(0.1), None, Some(8.0)])
        );
        let missing = json!({"input": 2.0, "cache_read": 1.0, "output": 8.0});
        assert_eq!(live_rate(Some(&missing), &package, 0), None);
        let negative = json!({"input": -2.0, "cache_read": 1.0, "cache_write": 1.0, "output": 8.0});
        assert_eq!(live_rate(Some(&negative), &package, 0), None);
        let expired = json!({"input": 2.0, "cache_read": 1.0, "cache_write": 1.0, "output": 8.0,
                             "valid_until": "1969-12-31T23:59:59Z"});
        assert_eq!(live_rate(Some(&expired), &package, 0), None);
        assert_eq!(live_rate(None, &package, 0), None);
    }
}
