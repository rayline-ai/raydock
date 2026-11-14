//! The compact runtime file set (`rayline.arc-runtime.v1`): what a local
//! decider reads, and nothing else.
//!
//! A runtime release directory holds:
//!
//! - `arc-runtime.json`: the policy. Arms are rows of the stage-two tensors,
//!   actions name the rows they serve, models are named once. An action's
//!   id is not stored: it is the digest of its model and control id, as the
//!   policy package defines it.
//! - `heads.safetensors`: the `stage_one.*` and `stage_two.*` tensors.
//! - `tokenizer.json`, `tokenizer_config.json`.
//! - `manifest.json`: sha256 and size of each of those files, checked on
//!   load.
//!
//! Every member of `arc-runtime.json` has a reader here; a member this
//! version does not know is refused. Version 1 means the one stage-two
//! recipe [`Release`] computes (a fixed-weight factored decision of the
//! level-thinking MLP head), so the file does not restate it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use rayline_arc_input::ArcTokenizer;
use serde::Deserialize;
use serde_json::json;

use crate::canon;
use crate::release::{
    Action, Arm, Release, ReleaseError, StageOneSettings, Thinking, ToolFree, Withheld,
};
use crate::stage_one::StageOneHead;
use crate::stage_two::StageTwo;
use crate::tensors::Tensors;

pub const RUNTIME_FILE: &str = "arc-runtime.json";
pub const RUNTIME_SCHEMA: &str = "rayline.arc-runtime.v1";
const MANIFEST_FILE: &str = "manifest.json";
const MANIFEST_SCHEMA: &str = "rayline.arc-runtime-manifest.v1";
const HEADS_FILE: &str = "heads.safetensors";
const TOKENIZER_FILE: &str = "tokenizer.json";
const TOKENIZER_CONFIG_FILE: &str = "tokenizer_config.json";
const REQUIRED: [&str; 4] = [
    RUNTIME_FILE,
    HEADS_FILE,
    TOKENIZER_FILE,
    TOKENIZER_CONFIG_FILE,
];

fn malformed(reason: impl Into<String>) -> ReleaseError {
    ReleaseError::Malformed(reason.into())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Runtime {
    schema: String,
    release: String,
    encoder: EncoderV1,
    #[serde(default)]
    thinking_controls_sha256: Option<String>,
    models: Vec<ModelV1>,
    arms: ArmsV1,
    actions: Vec<ActionV1>,
    stage_one: StageOneV1,
    stage_two: StageTwoV1,
    /// The hosted service's action for a turn it can't decide; local
    /// decisions have none.
    #[serde(default, rename = "fallback_action")]
    _fallback_action: Option<usize>,
    #[serde(default)]
    live_prices: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EncoderV1 {
    chunk_tokens: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelV1 {
    name: String,
    /// The id a request's prices and a price card name the model by, when
    /// it is not `name`.
    #[serde(default)]
    wire: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArmsV1 {
    /// Per stage-two row, its model (an index into `models`).
    model: Vec<usize>,
    /// Per row, whether the release serves it.
    served: Vec<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionV1 {
    model: usize,
    control_id: String,
    /// The stage-two rows that serve it.
    arms: Vec<usize>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    budget_tokens: Option<u64>,
    #[serde(default)]
    level: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageOneV1 {
    candidates: Vec<CandidateV1>,
    regime: usize,
    boundaries: Vec<String>,
    cost_weight: f64,
    stay_margin: f64,
    #[serde(default)]
    latency_weight: f64,
    #[serde(default)]
    offer_user_traffic_only: bool,
    #[serde(default)]
    cost_weight_bounds: Option<(f64, f64)>,
    #[serde(default)]
    presets: BTreeMap<String, f64>,
    #[serde(default)]
    tool_free: BTreeMap<String, ToolFreeV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateV1 {
    model: usize,
    #[serde(default)]
    withheld: Option<WithheldV1>,
    #[serde(default)]
    usage_unknown: bool,
    #[serde(default)]
    unmapped: bool,
    #[serde(default)]
    latency_factor: Option<f64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WithheldV1 {
    Selector,
    Excluded,
    Unsupported,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolFreeV1 {
    v_multiplier: f64,
    #[serde(default)]
    excluded_models: Vec<String>,
    #[serde(default)]
    latency_weight: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StageTwoV1 {
    cost_weight: f64,
    support_columns: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    #[serde(default)]
    release: Option<String>,
    files: BTreeMap<String, ManifestFile>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestFile {
    sha256: String,
    bytes: u64,
}

/// Whether `directory` holds a runtime file set.
pub fn is_runtime_release(directory: &Path) -> bool {
    directory.join(RUNTIME_FILE).is_file()
}

pub(crate) fn parse(bytes: &[u8]) -> Result<Runtime, ReleaseError> {
    let runtime: Runtime =
        serde_json::from_slice(bytes).map_err(|e| malformed(format!("{RUNTIME_FILE}: {e}")))?;
    if runtime.schema != RUNTIME_SCHEMA {
        return Err(ReleaseError::Unsupported(format!(
            "{RUNTIME_FILE} schema {} is not {RUNTIME_SCHEMA}",
            runtime.schema
        )));
    }
    Ok(runtime)
}

/// The stage-one encoder's chunk size a runtime release names.
pub(crate) fn chunk_tokens(directory: &Path) -> Result<u32, ReleaseError> {
    let path = directory.join(RUNTIME_FILE);
    let bytes =
        std::fs::read(&path).map_err(|e| ReleaseError::Read(format!("{}: {e}", path.display())))?;
    let tokens = parse(&bytes)?.encoder.chunk_tokens;
    if tokens == 0 {
        return Err(malformed("the runtime release names no encoder chunk size"));
    }
    Ok(tokens)
}

/// The largest `manifest.json` read.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;
/// The largest file the decider keeps in memory to parse (the runtime
/// document and the tokenizer files); the heads are hashed as a stream and
/// read by their own bounded reader.
const MAX_PARSED_BYTES: u64 = 64 << 20;

/// Opens `name` in `directory` when its size is at most `max` bytes.
fn open_sized(
    directory: &Path,
    name: &str,
    max: u64,
) -> Result<(std::fs::File, u64), ReleaseError> {
    let path = directory.join(name);
    let error = |e: std::io::Error| ReleaseError::Read(format!("{}: {e}", path.display()));
    let file = std::fs::File::open(&path).map_err(error)?;
    let size = file.metadata().map_err(error)?.len();
    if size > max {
        return Err(malformed(format!("{name} is larger than {max} bytes")));
    }
    Ok((file, size))
}

/// Reads every file the manifest lists and checks its size, from the
/// file's metadata before any byte is read, and its sha256. Returns the
/// bytes of the files the decider parses itself.
fn verified_files(directory: &Path) -> Result<HashMap<&'static str, Vec<u8>>, ReleaseError> {
    use std::io::Read as _;
    let read_error = |name: &str, e: std::io::Error| {
        ReleaseError::Read(format!("{}: {e}", directory.join(name).display()))
    };
    let (mut file, _) = open_sized(directory, MANIFEST_FILE, MAX_MANIFEST_BYTES)?;
    let mut raw = Vec::new();
    file.read_to_end(&mut raw)
        .map_err(|e| read_error(MANIFEST_FILE, e))?;
    let manifest: Manifest =
        serde_json::from_slice(&raw).map_err(|e| malformed(format!("{MANIFEST_FILE}: {e}")))?;
    if manifest.schema != MANIFEST_SCHEMA {
        return Err(ReleaseError::Unsupported(format!(
            "{MANIFEST_FILE} schema {} is not {MANIFEST_SCHEMA}",
            manifest.schema
        )));
    }
    let _ = manifest.release;
    for name in REQUIRED {
        if !manifest.files.contains_key(name) {
            return Err(malformed(format!("{MANIFEST_FILE} does not list {name}")));
        }
    }
    let mut kept = HashMap::new();
    for (name, want) in &manifest.files {
        let plain = Path::new(name)
            .file_name()
            .is_some_and(|base| base == name.as_str());
        if !plain {
            return Err(malformed(format!(
                "{MANIFEST_FILE} lists a path outside the release"
            )));
        }
        let not_named = || malformed(format!("{name} is not the file {MANIFEST_FILE} names"));
        let kept_as = REQUIRED
            .iter()
            .find(|r| **r == name.as_str() && **r != HEADS_FILE);
        let max = if kept_as.is_some() {
            MAX_PARSED_BYTES
        } else {
            u64::MAX
        };
        let (mut file, size) = open_sized(directory, name, max)?;
        if size != want.bytes {
            return Err(not_named());
        }
        match kept_as {
            Some(required) => {
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)
                    .map_err(|e| read_error(name, e))?;
                if bytes.len() as u64 != want.bytes || canon::sha256_hex(&bytes) != want.sha256 {
                    return Err(not_named());
                }
                kept.insert(*required, bytes);
            }
            None => {
                use sha2::Digest as _;
                let mut hasher = sha2::Sha256::new();
                std::io::copy(&mut file, &mut hasher).map_err(|e| read_error(name, e))?;
                if format!("{:x}", hasher.finalize()) != want.sha256 {
                    return Err(not_named());
                }
            }
        }
    }
    Ok(kept)
}

/// What the routes contract reads of a runtime release: the catalog the
/// package manifest gives a full release.
#[derive(Debug, Clone)]
pub struct RuntimeCatalog {
    /// sha256 of `arc-runtime.json`'s bytes.
    pub package_sha256: String,
    pub thinking_controls_sha256: Option<String>,
    pub actions: Vec<RuntimeAction>,
    /// Model name to the id requests and answers name it by, for each
    /// model whose `wire` id is not its name.
    pub wire_ids: Vec<(String, String)>,
}

#[derive(Debug, Clone)]
pub struct RuntimeAction {
    pub id: String,
    pub model: String,
    pub control_id: String,
    pub level: Option<String>,
}

/// An action's id: the digest of its model and control id.
fn action_id(model: &str, control_id: &str) -> String {
    canon::digest(&json!({"model": model, "control_id": control_id}))
}

/// The catalog of the runtime release whose `arc-runtime.json` is `raw`.
pub fn runtime_catalog(raw: &[u8]) -> Result<RuntimeCatalog, ReleaseError> {
    let runtime = parse(raw)?;
    let mut actions = Vec::with_capacity(runtime.actions.len());
    for body in &runtime.actions {
        let model = runtime
            .models
            .get(body.model)
            .ok_or_else(|| malformed("a model index is outside the model list"))?
            .name
            .clone();
        actions.push(RuntimeAction {
            id: action_id(&model, &body.control_id),
            model,
            control_id: body.control_id.clone(),
            level: body.level.clone(),
        });
    }
    let wire_ids = runtime
        .models
        .iter()
        .filter_map(|model| Some((model.name.clone(), model.wire.clone()?)))
        .collect();
    Ok(RuntimeCatalog {
        package_sha256: canon::sha256_hex(raw),
        thinking_controls_sha256: runtime.thinking_controls_sha256,
        actions,
        wire_ids,
    })
}

/// The id a decision names a stage-two row by.
fn arm_id(row: usize) -> String {
    canon::sha256_hex(format!("rayline.arc-runtime.v1 arm {row}").as_bytes())
}

fn check_weight(value: f64, what: &str) -> Result<f64, ReleaseError> {
    if value.is_finite() {
        Ok(value)
    } else {
        Err(malformed(format!("{what} is not a number")))
    }
}

impl Release {
    /// Loads a runtime release directory (`rayline.arc-runtime.v1`).
    pub fn from_runtime(directory: &Path) -> Result<Self, ReleaseError> {
        let mut files = verified_files(directory)?;
        let mut take = |name: &str| {
            files
                .remove(name)
                .ok_or_else(|| malformed(format!("{name} is missing")))
        };
        let runtime_bytes = take(RUNTIME_FILE)?;
        let runtime = parse(&runtime_bytes)?;
        runtime_catalog(&runtime_bytes)?;
        let tokenizer = ArcTokenizer::from_release(
            &take(TOKENIZER_FILE)?,
            &String::from_utf8(take(TOKENIZER_CONFIG_FILE)?)
                .map_err(|_| malformed("tokenizer_config.json is not UTF-8"))?,
        )
        .map_err(|e| malformed(e.to_string()))?;
        let heads = directory.join(HEADS_FILE);
        let head = StageOneHead::load(&Tensors::read(&heads, "stage_one.")?)?;
        let stage_two = StageTwo::load(
            &Tensors::read(&heads, "stage_two.")?,
            &runtime.stage_two.support_columns,
        )?;
        build(
            runtime,
            canon::sha256_hex(&runtime_bytes),
            tokenizer,
            head,
            stage_two,
        )
    }
}

fn build(
    runtime: Runtime,
    package_sha256: String,
    tokenizer: ArcTokenizer,
    head: StageOneHead,
    stage_two: StageTwo,
) -> Result<Release, ReleaseError> {
    let model = |index: usize| {
        runtime
            .models
            .get(index)
            .ok_or_else(|| malformed("a model index is outside the model list"))
    };
    let stage_two_cost_weight =
        check_weight(runtime.stage_two.cost_weight, "stage-two cost weight")?;
    if stage_two_cost_weight <= 0.0 {
        return Err(ReleaseError::Unsupported(
            "stage two has no cost weight".into(),
        ));
    }

    // Arms: one per stage-two row.
    let rows = runtime.arms.model.len();
    if runtime.arms.served.len() != rows || stage_two.arms() != rows {
        return Err(malformed("the arm rows are not stage two's"));
    }
    let mut arms = Vec::with_capacity(rows);
    let mut arm_index = HashMap::with_capacity(rows);
    for (row, (&m, &served)) in runtime
        .arms
        .model
        .iter()
        .zip(&runtime.arms.served)
        .enumerate()
    {
        let entry = model(m)?;
        let id = arm_id(row);
        arm_index.insert(id.clone(), row);
        arms.push(Arm {
            id,
            model: entry.name.clone(),
            served,
            action: None,
            model_id: entry.wire.clone().unwrap_or_else(|| entry.name.clone()),
            endpoint_tag: None,
            group: None,
        });
    }

    // Actions.
    let mut actions = Vec::with_capacity(runtime.actions.len());
    let mut action_index = HashMap::with_capacity(runtime.actions.len());
    for body in &runtime.actions {
        let name = model(body.model)?.name.clone();
        let id = action_id(&name, &body.control_id);
        for &row in &body.arms {
            let arm = arms
                .get_mut(row)
                .ok_or_else(|| malformed("an action names a row outside stage two"))?;
            if arm.action.is_some() || arm.model != name {
                return Err(malformed(
                    "an arm row serves two actions, or another model's action",
                ));
            }
            arm.action = Some(actions.len());
        }
        if action_index.insert(id.clone(), actions.len()).is_some() {
            return Err(malformed("two actions have one model and control"));
        }
        actions.push(Action {
            id,
            model: name,
            arms: body.arms.clone(),
            thinking: Thinking {
                effort: body.effort.clone(),
                budget_tokens: body.budget_tokens,
                steering_level: body.level.clone(),
                control_id: Some(body.control_id.clone()),
            },
        });
    }
    // Stage one.
    let one = &runtime.stage_one;
    let count = one.candidates.len();
    if count != head.candidates() || stage_two.dim() != head.dim() {
        return Err(malformed(
            "the stage-one head scores another candidate list",
        ));
    }
    if one.regime >= head.regimes() {
        return Err(malformed("stage one's regime is not one of the head's"));
    }
    let candidates: Vec<String> = one
        .candidates
        .iter()
        .map(|c| model(c.model).map(|m| m.name.clone()))
        .collect::<Result<_, _>>()?;
    let latency_weight = check_weight(one.latency_weight, "stage-one latency weight")?;
    let log_latency = (latency_weight != 0.0).then(|| {
        one.candidates
            .iter()
            .map(|c| c.latency_factor.map_or(f64::NAN, f64::ln))
            .collect::<Vec<f64>>()
    });
    let withheld: Vec<Option<Withheld>> = one
        .candidates
        .iter()
        .enumerate()
        .map(|(i, c)| match c.withheld {
            Some(WithheldV1::Selector) => Some(Withheld::BySelector),
            Some(WithheldV1::Excluded) => Some(Withheld::Excluded),
            Some(WithheldV1::Unsupported) => Some(Withheld::Unsupported),
            None if head.auxiliary[i] && !one.offer_user_traffic_only => {
                Some(Withheld::UserTrafficOnly)
            }
            None if log_latency.as_ref().is_some_and(|l| l[i].is_nan()) => {
                Some(Withheld::NoLatencyFactor)
            }
            None => None,
        })
        .collect();
    let mut candidate_arms = vec![Vec::new(); count];
    let by_model: HashMap<&str, usize> = candidates
        .iter()
        .enumerate()
        .map(|(i, m)| (m.as_str(), i))
        .collect();
    for (index, arm) in arms.iter().enumerate() {
        if let (true, Some(candidate)) = (arm.served, by_model.get(arm.model.as_str())) {
            candidate_arms[*candidate].push(index);
        }
    }
    let names: HashSet<&str> = runtime.models.iter().map(|m| m.name.as_str()).collect();
    let mut tool_free = BTreeMap::new();
    for (tier, entry) in &one.tool_free {
        if let Some(stray) = entry
            .excluded_models
            .iter()
            .find(|m| !names.contains(m.as_str()))
        {
            return Err(malformed(format!(
                "tier {tier} excludes {stray}, which is not a model of the release"
            )));
        }
        tool_free.insert(
            tier.clone(),
            ToolFree {
                v_multiplier: check_weight(entry.v_multiplier, "v_multiplier")?,
                excluded_models: entry.excluded_models.clone(),
                latency_weight: check_weight(entry.latency_weight, "tool-free latency weight")?,
            },
        );
    }
    let settings = StageOneSettings {
        boundaries: one.boundaries.clone(),
        cost_weight: check_weight(one.cost_weight, "stage-one cost_weight")?,
        stay_margin: check_weight(one.stay_margin, "stage-one stay_margin")?,
        regime: one.regime,
        latency_weight,
        log_latency,
        cost_weight_bounds: one.cost_weight_bounds,
        presets: one.presets.clone(),
        tool_free,
    };
    if settings.stay_margin <= 0.0 {
        return Err(ReleaseError::Unsupported(
            "stage one has no stay margin".into(),
        ));
    }
    Ok(Release {
        name: runtime.release,
        package_sha256,
        tokenizer,
        usage_known: one.candidates.iter().map(|c| !c.usage_unknown).collect(),
        unmapped: one.candidates.iter().map(|c| c.unmapped).collect(),
        candidates,
        withheld,
        candidate_arms,
        local_placement_exempt: false,
        settings,
        head,
        stage_two,
        arms,
        arm_index,
        actions,
        action_index,
        package_rates: None,
        stage_two_cost_weight,
        live_prices: runtime.live_prices,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"{
        "schema": "rayline.arc-runtime.v1", "release": "t", "encoder": {"chunk_tokens": 512},
        "models": [{"name": "m"}], "arms": {"model": [0], "served": [true]},
        "actions": [{"model": 0, "control_id": "c", "arms": [0]}],
        "stage_one": {"candidates": [{"model": 0}], "regime": 0, "boundaries": ["task_start"],
                      "cost_weight": 1.0, "stay_margin": 1.0},
        "stage_two": {"cost_weight": 0.3, "support_columns": ["quality"]}
    }"#;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("arc-runtime-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn the_catalog_carries_each_model_wire_id() {
        let plain = runtime_catalog(MINIMAL.as_bytes()).expect("catalog");
        assert!(plain.wire_ids.is_empty());
        let wired = MINIMAL.replacen(
            r#"{"name": "m"}"#,
            r#"{"name": "m", "wire": "vendor/m"}"#,
            1,
        );
        let catalog = runtime_catalog(wired.as_bytes()).expect("catalog");
        assert_eq!(catalog.wire_ids, [("m".to_owned(), "vendor/m".to_owned())]);
        // Action ids stay digests of the model's name.
        assert_eq!(catalog.actions[0].id, plain.actions[0].id);
        assert_eq!(catalog.actions[0].model, "m");
    }

    #[test]
    fn a_minimal_document_parses_and_an_unknown_member_is_refused() {
        let runtime = parse(MINIMAL.as_bytes()).expect("parses");
        assert_eq!(runtime.encoder.chunk_tokens, 512);
        assert_eq!(runtime.actions[0].arms, vec![0]);
        let extra = MINIMAL.replacen(
            "\"release\": \"t\",",
            "\"release\": \"t\", \"recipe\": {},",
            1,
        );
        assert!(matches!(
            parse(extra.as_bytes()),
            Err(ReleaseError::Malformed(_))
        ));
        let nested = MINIMAL.replacen(
            "\"cost_weight\": 0.3,",
            "\"cost_weight\": 0.3, \"seed\": 1,",
            1,
        );
        assert!(matches!(
            parse(nested.as_bytes()),
            Err(ReleaseError::Malformed(_))
        ));
        let other = MINIMAL.replacen("arc-runtime.v1", "arc-runtime.v2", 1);
        assert!(matches!(
            parse(other.as_bytes()),
            Err(ReleaseError::Unsupported(_))
        ));
    }

    #[test]
    fn the_chunk_size_is_read_from_the_runtime_file() {
        let dir = scratch("chunk");
        std::fs::write(dir.join(RUNTIME_FILE), MINIMAL).expect("write");
        assert!(is_runtime_release(&dir));
        assert_eq!(
            crate::stage_one_chunk_tokens(&dir).expect("a chunk size"),
            512
        );
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn the_manifest_must_name_every_file_by_its_digest() {
        let dir = scratch("manifest");
        for name in REQUIRED {
            std::fs::write(dir.join(name), name).expect("write");
        }
        let entry =
            |name: &str| json!({"sha256": canon::sha256_hex(name.as_bytes()), "bytes": name.len()});
        let mut files: serde_json::Map<String, serde_json::Value> = REQUIRED
            .iter()
            .map(|n| ((*n).to_owned(), entry(n)))
            .collect();
        let write = |files: &serde_json::Map<String, serde_json::Value>| {
            let manifest = json!({"schema": MANIFEST_SCHEMA, "files": files});
            std::fs::write(dir.join(MANIFEST_FILE), manifest.to_string()).expect("write");
        };
        write(&files);
        let kept = verified_files(&dir).expect("verifies");
        assert_eq!(kept[RUNTIME_FILE], RUNTIME_FILE.as_bytes());
        assert!(!kept.contains_key(HEADS_FILE));

        std::fs::write(dir.join(TOKENIZER_FILE), "changed").expect("write");
        assert!(verified_files(&dir).is_err());
        std::fs::write(dir.join(TOKENIZER_FILE), TOKENIZER_FILE).expect("write");

        files.insert("../outside".into(), entry("x"));
        write(&files);
        assert!(verified_files(&dir).is_err());
        files.remove("../outside");
        files.remove(HEADS_FILE);
        write(&files);
        assert!(verified_files(&dir).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
