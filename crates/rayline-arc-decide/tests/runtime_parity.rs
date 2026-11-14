//! A runtime release (`arc-runtime.json`) decides as the full release it was
//! exported from.
//!
//! Replays a release's goldens through both, with the same readouts and the
//! same per-model price card, and requires the same decision: the selected
//! action and arm row, reason, token count, every action's flags, and every
//! number within 1e-9. Arm ids differ between the formats (a full release
//! names an arm by the digest of its body, a runtime release by its row), so
//! the request's attribution is translated row for row, and the runtime
//! decision's ids back.
//!
//! ```text
//! ARC_DECIDE_RELEASE=/path/to/full/release \
//! ARC_RUNTIME_RELEASE=/path/to/runtime/release \
//! ARC_DECIDE_PRICES=/path/to/card.json \
//! ARC_READOUT_CACHE=/path/to/readouts \
//!   cargo test --release -p rayline-arc-decide --test runtime_parity -- --ignored --nocapture
//! ```
//!
//! `ARC_DECIDE_PRICES` is a per-model card (`{"models": {...}}`) applied to
//! both. Every readout must be cached. Failures name the case, never
//! request content.

use std::collections::HashMap;
use std::path::PathBuf;

use rayline_arc_decide::{
    DecideInput, HeldModel, ReadoutRequest, Readouts, Release, ReleaseFiles, decide,
};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

const TOLERANCE: f64 = 1e-9;

#[derive(Deserialize)]
struct Golden {
    case_id: String,
    request: Box<RawValue>,
    #[serde(default)]
    stage_one_held: Option<HeldModel>,
}

struct Cached(PathBuf);

impl Readouts for Cached {
    fn readouts(&self, request: &ReadoutRequest<'_>) -> Result<Vec<Vec<f32>>, String> {
        let mut hasher = Sha256::new();
        for token in request.tokens {
            hasher.update(token.to_le_bytes());
        }
        hasher.update([0xff; 4]);
        for offset in request.offsets {
            hasher.update((*offset as u64).to_le_bytes());
        }
        let name: String = hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let bytes = std::fs::read(self.0.join(format!("{name}.f32")))
            .map_err(|_| "a readout is not cached".to_owned())?;
        let rows = request.offsets.len();
        if rows == 0 || bytes.is_empty() || bytes.len() % (rows * 4) != 0 {
            return Err("a cached readout has the wrong size".into());
        }
        Ok(bytes
            .chunks_exact(bytes.len() / rows)
            .map(|row| {
                row.chunks_exact(4)
                    .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                    .collect()
            })
            .collect())
    }
}

/// `value` with every string `map` names replaced.
fn rename(value: &Value, map: &HashMap<String, String>) -> Value {
    match value {
        Value::String(text) => {
            Value::String(map.get(text).cloned().unwrap_or_else(|| text.clone()))
        }
        Value::Array(items) => Value::Array(items.iter().map(|v| rename(v, map)).collect()),
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(k, v)| (k.clone(), rename(v, map)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The first path at which `a` and `b` differ, numbers within the tolerance.
fn differs(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            let (x, y) = (
                x.as_f64().unwrap_or(f64::NAN),
                y.as_f64().unwrap_or(f64::NAN),
            );
            ((x - y).abs() > TOLERANCE || x.is_nan() != y.is_nan()).then(|| path.to_owned())
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path} (length)"));
            }
            x.iter()
                .zip(y)
                .enumerate()
                .find_map(|(i, (x, y))| differs(x, y, &format!("{path}[{i}]")))
        }
        (Value::Object(x), Value::Object(y)) => {
            if x.len() != y.len() {
                return Some(format!("{path} (members)"));
            }
            x.iter().find_map(|(key, value)| match y.get(key) {
                Some(other) => differs(value, other, &format!("{path}.{key}")),
                None => Some(format!("{path}.{key} (missing)")),
            })
        }
        (x, y) => (x != y).then(|| path.to_owned()),
    }
}

fn env_path(name: &str) -> PathBuf {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set {name}"))
}

#[test]
#[ignore = "needs a full release, its runtime export, a price card and cached readouts"]
fn a_runtime_release_decides_as_its_full_release() {
    let full_dir = env_path("ARC_DECIDE_RELEASE");
    let card = env_path("ARC_DECIDE_PRICES");
    let full = Release::from_files(&ReleaseFiles::in_dir(&full_dir))
        .and_then(|r| r.with_model_prices(&card))
        .expect("the full release loads");
    let runtime = Release::load(&env_path("ARC_RUNTIME_RELEASE"))
        .and_then(|r| r.with_model_prices(&card))
        .expect("the runtime release loads");
    let source = Cached(env_path("ARC_READOUT_CACHE"));

    assert_eq!(full.arms.len(), runtime.arms.len(), "arm rows");
    assert_eq!(full.candidates, runtime.candidates, "stage-one candidates");
    let full_ids: Vec<&str> = full.actions.iter().map(|a| a.id.as_str()).collect();
    let runtime_ids: Vec<&str> = runtime.actions.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(full_ids, runtime_ids, "action ids");
    let to_runtime: HashMap<String, String> = full
        .arms
        .iter()
        .zip(&runtime.arms)
        .map(|(f, r)| (f.id.clone(), r.id.clone()))
        .collect();
    let mut to_full: HashMap<String, String> = to_runtime
        .iter()
        .map(|(f, r)| (r.clone(), f.clone()))
        .collect();
    to_full.insert(runtime.package_sha256.clone(), full.package_sha256.clone());
    to_full.insert(runtime.name.clone(), full.name.clone());

    let goldens = std::fs::read_to_string(full_dir.join("goldens.jsonl")).expect("goldens");
    let (mut cases, mut same, mut refused_alike) = (0, 0, 0);
    let mut failures = Vec::new();
    for line in goldens.lines().filter(|l| !l.trim().is_empty()) {
        let golden: Golden = serde_json::from_str(line).expect("a golden line");
        let mut full_input: DecideInput =
            serde_json::from_str(golden.request.get()).expect("a decide request");
        full_input.stage_one_held = golden.stage_one_held.clone();
        // The same request text; only the attributed arm ids are renamed.
        let mut runtime_input = full_input.clone();
        for attribution in &mut runtime_input.attribution {
            if let Some(id) = attribution.arm_id.as_mut() {
                if let Some(runtime_id) = to_runtime.get(id.as_str()) {
                    id.clone_from(runtime_id);
                }
            }
        }
        cases += 1;
        match (
            decide(&full, &full_input, &source),
            decide(&runtime, &runtime_input, &source),
        ) {
            (Ok(a), Ok(b)) => {
                let a = serde_json::to_value(&a).expect("json");
                let b = rename(&serde_json::to_value(&b).expect("json"), &to_full);
                match differs(&a, &b, "") {
                    None => same += 1,
                    Some(path) => failures.push(format!("{}: {path}", golden.case_id)),
                }
            }
            // A readout or internal error says nothing about the release:
            // it fails the case even when both sides hit it.
            (Err(a), Err(b))
                if a.to_string() == b.to_string()
                    && !matches!(
                        a,
                        rayline_arc_decide::DecideError::Readouts(_)
                            | rayline_arc_decide::DecideError::Internal(_)
                    ) =>
            {
                same += 1;
                refused_alike += 1;
            }
            (a, b) => failures.push(format!(
                "{}: full {} / runtime {}",
                golden.case_id,
                a.err().map_or("decided".into(), |e| e.to_string()),
                b.err().map_or("decided".into(), |e| e.to_string()),
            )),
        }
    }
    println!("runtime vs full: {same}/{cases} identical ({refused_alike} refused alike)");
    for failure in failures.iter().take(20) {
        println!("  {failure}");
    }
    assert!(cases > 0, "no golden was replayed");
    assert!(
        same > refused_alike,
        "none of {cases} goldens was decided and compared"
    );
    assert!(failures.is_empty(), "{} of {cases} differ", failures.len());
}
