//! Parity with a release's golden decisions.
//!
//! A release's `goldens.jsonl` records, per request, the decision its policy
//! service made: the selected action and arm, the token count and every
//! action's score. This test replays them through [`decide`] with readouts
//! from a local encoder server and holds the result to the bar the service
//! holds a replica on another engine to: flags, reason and token count
//! exact, every score within 0.03, and the selected action exact unless the
//! golden's top two actions are within 0.0061 of each other.
//!
//! It needs files that are not in this repository, so it is ignored by
//! default:
//!
//! ```text
//! ARC_DECIDE_RELEASE=/path/to/release \
//! ARC_ENCODER_URL=http://127.0.0.1:18555 \
//!   cargo test -p rayline-arc-decide --test parity -- --ignored --nocapture
//! ```
//!
//! - `ARC_DECIDE_RELEASE`: a release directory whose `config.json` carries
//!   the serving record, with `package.json` and the tokenizer files.
//! - `ARC_DECIDE_WEIGHTS`, `ARC_DECIDE_GOLDENS`: other paths for
//!   `model.safetensors` and `goldens.jsonl` than the release directory's.
//! - `ARC_DECIDE_PRICES`: a price scenario for a release that carries none
//!   and whose goldens state no `routing_prices`.
//! - `ARC_ENCODER_URL`: the encoder server. `ARC_READOUT_CACHE` names a
//!   directory where its rows are kept, keyed by the tokens asked for; with
//!   every case cached the server is not needed.
//! - `ARC_DECIDE_HOST`: only goldens whose recording host contains this.
//! - `ARC_DECIDE_REFERENCE`: a JSON-lines file of `{"case_id", "response"}`,
//!   each `response` the policy service's own answer to the golden's request
//!   on the same encoder; compared with a tight tolerance, tier picks and
//!   receipt included. A golden line may carry the answer itself as
//!   `reference`, or `refused` (`{"code", "reason", "field"}`) in place of
//!   `expected` for a request the service refuses.
//!
//! - `ARC_DECIDE_DUMP`: a case id whose decision is printed as JSON.
//!
//! Failures name the case and what differs, never request content.

use std::cell::Cell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rayline_arc_decide::{
    DecideError, DecideInput, DecideOutput, HeldModel, LocalEncoder, ReadoutRequest, Readouts,
    Release, ReleaseFiles, decide,
};
use serde::Deserialize;
use serde_json::Value;
use serde_json::value::RawValue;
use sha2::{Digest, Sha256};

const SCORE_TOLERANCE: f64 = 0.03;
const NEAR_TIE_MARGIN: f64 = 0.0061;
/// Same readouts, same arithmetic in another float order.
const REFERENCE_TOLERANCE: f64 = 2e-4;

#[derive(Deserialize)]
struct Golden {
    case_id: String,
    request: Box<RawValue>,
    #[serde(default)]
    expected: Option<Expected>,
    #[serde(default)]
    refused: Option<Value>,
    #[serde(default)]
    reference: Option<Value>,
    #[serde(default)]
    stage_one_held: Option<HeldModel>,
    #[serde(default)]
    arithmetic: Value,
}

#[derive(Deserialize)]
struct Expected {
    selected_action_id: String,
    selected_arm_id: String,
    reason: String,
    token_count: usize,
    margin: Option<f64>,
    actions: Vec<ExpectedAction>,
}

#[derive(Deserialize)]
struct ExpectedAction {
    action_id: String,
    available: bool,
    supported: bool,
    selected: bool,
    score: f64,
}

/// Reads rows from a directory when it has them, else from the encoder.
struct Cached {
    directory: Option<PathBuf>,
    encoder: Option<LocalEncoder>,
    fetched: Cell<usize>,
}

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
        let path = self
            .directory
            .as_ref()
            .map(|d| d.join(format!("{name}.f32")));
        let rows = request.offsets.len();
        if let Some(bytes) = path.as_ref().and_then(|p| std::fs::read(p).ok()) {
            if rows > 0 && bytes.len() % (rows * 4) == 0 && !bytes.is_empty() {
                let width = bytes.len() / rows;
                return Ok(bytes
                    .chunks_exact(width)
                    .map(|row| {
                        row.chunks_exact(4)
                            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                            .collect()
                    })
                    .collect());
            }
        }
        let encoder = self
            .encoder
            .as_ref()
            .ok_or("no cached readout and no ARC_ENCODER_URL")?;
        let fetched = encoder.readouts(request)?;
        self.fetched.set(self.fetched.get() + 1);
        if let Some(path) = path {
            let bytes: Vec<u8> = fetched
                .iter()
                .flatten()
                .flat_map(|v| v.to_le_bytes())
                .collect();
            std::fs::write(&path, bytes).map_err(|e| format!("write {}: {e}", path.display()))?;
        }
        Ok(fetched)
    }
}

fn margin(expected: &Expected) -> Option<f64> {
    expected.margin?;
    let mut scores: Vec<f64> = expected
        .actions
        .iter()
        .filter(|a| a.available && a.supported)
        .map(|a| a.score)
        .collect();
    scores.sort_by(|a, b| b.total_cmp(a));
    (scores.len() > 1).then(|| scores[0] - scores[1])
}

/// Why `observed` does not reproduce `expected`, and the largest score gap.
fn mismatch(expected: &Expected, observed: &DecideOutput) -> (Option<String>, f64) {
    let action_tie = margin(expected).is_some_and(|m| m > 0.0 && m <= NEAR_TIE_MARGIN);
    let arm_tie = expected.margin.is_some_and(|m| m <= NEAR_TIE_MARGIN);
    let got: HashMap<&str, _> = observed
        .actions
        .iter()
        .map(|a| (a.action_id.as_str(), a))
        .collect();
    let mut worst = 0f64;
    let mut reason = None;
    let mut note = |text: String| {
        if reason.is_none() {
            reason = Some(text);
        }
    };
    if !action_tie && observed.decision.selected_action_id != expected.selected_action_id {
        note("selected action differs".into());
    }
    if !arm_tie && observed.decision.selected_arm_id != expected.selected_arm_id {
        note("selected arm differs".into());
    }
    if observed.decision.reason != expected.reason {
        note(format!(
            "reason {} != {}",
            observed.decision.reason, expected.reason
        ));
    }
    if observed.encoding.token_count != expected.token_count {
        note(format!(
            "token count {} != {}",
            observed.encoding.token_count, expected.token_count
        ));
    }
    if got.len() != expected.actions.len() {
        note("scored actions differ".into());
    }
    for want in &expected.actions {
        let short = &want.action_id[..12.min(want.action_id.len())];
        let Some(action) = got.get(want.action_id.as_str()) else {
            note(format!("action {short} is not scored"));
            continue;
        };
        if action.available != want.available
            || action.supported != want.supported
            || (!action_tie && action.selected != want.selected)
        {
            note(format!("action {short} flags differ"));
        }
        let gap = (f64::from(action.score) - want.score).abs();
        worst = worst.max(gap);
        if gap.is_nan() || gap > SCORE_TOLERANCE {
            note(format!("action {short} score off by {gap:.4}"));
        }
    }
    (reason, worst)
}

/// Whether two JSON values are equal, numbers within the reference
/// tolerance. Members only `ours` has are ignored.
fn close(theirs: &Value, ours: &Value) -> bool {
    match (theirs, ours) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (
                a.as_f64().unwrap_or(f64::NAN),
                b.as_f64().unwrap_or(f64::NAN),
            );
            (a - b).abs() <= REFERENCE_TOLERANCE
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| close(a, b))
        }
        (Value::Object(a), Value::Object(b)) => a
            .iter()
            .all(|(key, value)| close(value, b.get(key).unwrap_or(&Value::Null))),
        (a, b) => a == b,
    }
}

/// How far `observed` is from the service's own answer on the same encoder.
fn against_reference(reference: &Value, observed: &DecideOutput) -> Result<f64, String> {
    let decision = &reference["decision"];
    let ours = serde_json::to_value(&observed.decision).map_err(|e| e.to_string())?;
    if let Some(picks) = decision.get("tier_picks").and_then(Value::as_object) {
        for (name, pick) in picks {
            let mut pick = pick.clone();
            if let Some(members) = pick.as_object_mut() {
                members.remove("stage_one_candidate");
            }
            if !close(&pick, &ours["tier_picks"][name]) {
                return Err(format!("tier pick {name} differs"));
            }
        }
    }
    if let Some(receipt) = decision.get("receipt") {
        for (key, value) in receipt.as_object().into_iter().flatten() {
            if !close(value, &ours["receipt"][key]) {
                return Err(format!("receipt {key} differs"));
            }
        }
    }
    if decision["operating_point"]["cost_weight"] != ours["operating_point"]["cost_weight"] {
        return Err("operating point differs".into());
    }
    if decision["selected_action_id"] != observed.decision.selected_action_id.as_str() {
        return Err("selected action differs".into());
    }
    if decision["reason"] != observed.decision.reason {
        return Err("reason differs".into());
    }
    if reference["encoding"]["token_count"] != observed.encoding.token_count {
        return Err("token count differs".into());
    }
    let got: HashMap<&str, _> = observed
        .actions
        .iter()
        .map(|a| (a.action_id.as_str(), a))
        .collect();
    let actions = reference["actions"].as_array().ok_or("no actions")?;
    if actions.len() != got.len() {
        return Err("scored actions differ".into());
    }
    let mut worst = 0f64;
    for want in actions {
        let action = want["action_id"]
            .as_str()
            .and_then(|id| got.get(id))
            .ok_or("an action is not scored")?;
        if want["available"] != action.available
            || want["supported"] != action.supported
            || want["selected"] != action.selected
        {
            return Err("action flags differ".into());
        }
        let gap = (want["score"].as_f64().unwrap_or(f64::NAN) - f64::from(action.score)).abs();
        if gap.is_nan() || gap > REFERENCE_TOLERANCE {
            return Err(format!("a score is off by {gap:.6}"));
        }
        worst = worst.max(gap);
    }
    Ok(worst)
}

/// Whether `error` is the refusal the service gave.
fn refusal_matches(refused: &Value, error: &DecideError) -> bool {
    let detail = match error {
        DecideError::Invalid { field } => ("field", *field),
        DecideError::Refused { reason } | DecideError::Unsupported { reason } => {
            ("reason", *reason)
        }
        _ => return false,
    };
    refused["code"] == error.code() && refused[detail.0] == detail.1
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

#[test]
#[ignore = "needs a release with its serving record, goldens and an encoder (see the module docs)"]
fn golden_decisions_reproduce() {
    let directory = env_path("ARC_DECIDE_RELEASE").expect("set ARC_DECIDE_RELEASE");
    let mut files = ReleaseFiles::in_dir(&directory);
    if let Some(weights) = env_path("ARC_DECIDE_WEIGHTS") {
        files.weights = weights;
    }
    let mut release = Release::from_files(&files).expect("the release loads");
    if let Some(prices) = env_path("ARC_DECIDE_PRICES") {
        release = release.with_prices(&prices).expect("the prices load");
    }
    let goldens = env_path("ARC_DECIDE_GOLDENS").unwrap_or_else(|| directory.join("goldens.jsonl"));
    let host = std::env::var("ARC_DECIDE_HOST").unwrap_or_default();
    let cache = env_path("ARC_READOUT_CACHE");
    if let Some(cache) = &cache {
        std::fs::create_dir_all(cache).expect("create the readout cache");
    }
    let source = Cached {
        directory: cache,
        encoder: std::env::var("ARC_ENCODER_URL")
            .ok()
            .filter(|url| !url.is_empty())
            .map(|url| LocalEncoder::new(&url).expect("ARC_ENCODER_URL is http://host:port")),
        fetched: Cell::new(0),
    };
    let reference: HashMap<String, Value> = env_path("ARC_DECIDE_REFERENCE")
        .map(|path| read_lines(&path))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|line| {
            let row: Value = serde_json::from_str(&line).ok()?;
            Some((row["case_id"].as_str()?.to_owned(), row["response"].clone()))
        })
        .collect();

    let dump = std::env::var("ARC_DECIDE_DUMP").ok();
    let (mut cases, mut passed, mut skipped, mut same_action, mut refused_alike) = (0, 0, 0, 0, 0);
    let (mut worst, mut reference_cases, mut reference_passed, mut reference_worst) =
        (0f64, 0, 0, 0f64);
    let mut failures = Vec::new();
    for line in read_lines(&goldens) {
        let golden: Golden = serde_json::from_str(&line).expect("a golden line");
        let recorded_on = golden.arithmetic["host_id"].as_str().unwrap_or_default();
        if !recorded_on.contains(&host) {
            continue;
        }
        let mut input: DecideInput =
            serde_json::from_str(golden.request.get()).expect("a decide request");
        input.stage_one_held = golden.stage_one_held.clone();
        let observed = match decide(&release, &input, &source) {
            Ok(observed) => observed,
            // Requests with images: the text encoder input is not built for them.
            Err(DecideError::Projection(reason)) if reason.contains("not supported yet") => {
                skipped += 1;
                continue;
            }
            Err(error) => {
                cases += 1;
                match &golden.refused {
                    Some(refused) if refusal_matches(refused, &error) => {
                        passed += 1;
                        refused_alike += 1;
                    }
                    _ => failures.push(format!("{}: {error}", golden.case_id)),
                }
                continue;
            }
        };
        cases += 1;
        let Some(expected) = &golden.expected else {
            failures.push(format!(
                "{}: decided a request the service refuses",
                golden.case_id
            ));
            continue;
        };
        if dump.as_deref() == Some(golden.case_id.as_str()) {
            println!(
                "{}",
                serde_json::to_string(&observed).expect("a decision serializes")
            );
        }
        let (reason, gap) = mismatch(expected, &observed);
        worst = worst.max(gap);
        same_action +=
            usize::from(observed.decision.selected_action_id == expected.selected_action_id);
        match reason {
            None => passed += 1,
            Some(reason) => failures.push(format!("{}: {reason}", golden.case_id)),
        }
        if let Some(response) = golden
            .reference
            .as_ref()
            .or_else(|| reference.get(&golden.case_id))
        {
            reference_cases += 1;
            match against_reference(response, &observed) {
                Ok(gap) => {
                    reference_passed += 1;
                    reference_worst = reference_worst.max(gap);
                }
                Err(reason) => failures.push(format!("{} vs reference: {reason}", golden.case_id)),
            }
        }
    }
    println!(
        "goldens: {passed}/{cases} reproduce (same action {same_action}/{cases}, largest score gap {worst:.5}, \
         {refused_alike} refused alike, {skipped} skipped, {} encoded now)",
        source.fetched.get()
    );
    if reference_cases > 0 {
        println!(
            "service on the same encoder: {reference_passed}/{reference_cases} equal within {REFERENCE_TOLERANCE} \
             (largest score gap {reference_worst:.2e})"
        );
    }
    for failure in &failures {
        println!("  {failure}");
    }
    assert!(cases > 0, "no golden was replayed");
    assert!(
        failures.is_empty(),
        "{} of {cases} goldens differ",
        failures.len()
    );
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect()
}
