//! A loaded ARC release: the head tensors of `model.safetensors`, the
//! serving record of `config.json` (`arc.serving`, `arc.stage_one`,
//! `arc.stage_two`) and the action catalog of `package.json`.
//!
//! A release decides alone only when its `config.json` carries the serving
//! record: the arm catalog with each arm's worker, and stage one's decision
//! settings. A release published without it loads as
//! [`ReleaseError::NoServingRecord`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use rayline_arc_input::ArcTokenizer;
use serde::Serialize;
use serde_json::Value;

use crate::canon;
use crate::stage_one::{LEGS, StageOneHead};
use crate::stage_two::StageTwo;
use crate::tensors::{TensorError, Tensors};

/// Dollars per million tokens on each billed leg (uncached input, cache
/// read, cache write, output); `None` is unknown.
pub type Rates = [Option<f64>; LEGS];

/// The names of the legs in a price entry, in [`Rates`] order.
pub const PRICE_LEGS: [&str; LEGS] = ["input", "cache_read", "cache_write", "output"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReleaseError {
    /// A file could not be read.
    Read(String),
    /// A file is not the shape a release has.
    Malformed(String),
    /// `config.json` has no `arc.serving` record, so the release names no
    /// arms, workers or stage-one settings to decide with.
    NoServingRecord,
    /// The release uses a policy feature this crate does not compute.
    Unsupported(String),
}

impl fmt::Display for ReleaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(reason) => write!(f, "cannot read the release: {reason}"),
            Self::Malformed(reason) => write!(f, "malformed release: {reason}"),
            Self::NoServingRecord => f.write_str(
                "the release carries no serving record (arc.serving in config.json) and cannot decide alone",
            ),
            Self::Unsupported(reason) => write!(f, "unsupported release: {reason}"),
        }
    }
}

impl std::error::Error for ReleaseError {}

impl From<TensorError> for ReleaseError {
    fn from(error: TensorError) -> Self {
        Self::Malformed(error.0)
    }
}

fn malformed(reason: impl Into<String>) -> ReleaseError {
    ReleaseError::Malformed(reason.into())
}

/// The files a release is loaded from.
#[derive(Debug, Clone)]
pub struct ReleaseFiles {
    pub config: PathBuf,
    pub package: PathBuf,
    pub weights: PathBuf,
    pub tokenizer: PathBuf,
    pub tokenizer_config: PathBuf,
}

impl ReleaseFiles {
    /// The files of a release directory under their published names.
    pub fn in_dir(directory: &Path) -> Self {
        Self {
            config: directory.join("config.json"),
            package: directory.join("package.json"),
            weights: directory.join("model.safetensors"),
            tokenizer: directory.join("tokenizer.json"),
            tokenizer_config: directory.join("tokenizer_config.json"),
        }
    }
}

/// An action's thinking control, as a decision reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Thinking {
    pub effort: Option<String>,
    pub budget_tokens: Option<u64>,
    pub steering_level: Option<String>,
    pub control_id: Option<String>,
}

/// One routable action: a model at one thinking control.
#[derive(Debug, Clone)]
pub struct Action {
    pub id: String,
    pub model: String,
    /// The trained arms that serve it, as indices into [`Release::arms`].
    pub arms: Vec<usize>,
    pub thinking: Thinking,
}

/// One trained arm.
#[derive(Debug, Clone)]
pub struct Arm {
    pub id: String,
    pub model: String,
    /// Whether the release has a worker for it.
    pub served: bool,
    pub action: Option<usize>,
    /// The id a price table names its model by.
    pub model_id: String,
    /// The one endpoint its worker pins, when it pins exactly one.
    pub endpoint_tag: Option<String>,
    /// Arms that must be priced alike share a group.
    pub group: Option<String>,
}

/// Why a stage-one candidate is never offered by this release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Withheld {
    BySelector,
    Excluded,
    Unsupported,
    UserTrafficOnly,
    /// The latency table does not name the model. A request that runs the
    /// model locally lifts this one reason.
    NoLatencyFactor,
}

/// A tier's override for a request that carries no tool definitions.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolFree {
    pub v_multiplier: f64,
    pub excluded_models: Vec<String>,
    pub latency_weight: f64,
}

/// Stage one's decision settings.
#[derive(Debug, Clone)]
pub struct StageOneSettings {
    pub boundaries: Vec<String>,
    pub cost_weight: f64,
    pub stay_margin: f64,
    pub regime: usize,
    pub latency_weight: f64,
    /// Per candidate, the log of its latency factor (`NaN` where the table
    /// lacks the model); `None` without a latency table.
    pub log_latency: Option<Vec<f64>>,
    /// The cost weights a request may choose, `[low, high]`.
    pub cost_weight_bounds: Option<(f64, f64)>,
    /// Each named tier's cost weight.
    pub presets: BTreeMap<String, f64>,
    pub tool_free: BTreeMap<String, ToolFree>,
}

pub struct Release {
    pub name: String,
    pub package_sha256: String,
    pub(crate) tokenizer: ArcTokenizer,
    /// Stage one's candidate models, in head order.
    pub candidates: Vec<String>,
    pub(crate) withheld: Vec<Option<Withheld>>,
    /// Each candidate's served arms, in catalog order.
    pub(crate) candidate_arms: Vec<Vec<usize>>,
    pub(crate) usage_known: Vec<bool>,
    pub(crate) unmapped: Vec<bool>,
    pub(crate) local_placement_exempt: bool,
    pub settings: StageOneSettings,
    pub(crate) head: StageOneHead,
    pub(crate) stage_two: StageTwo,
    pub arms: Vec<Arm>,
    pub(crate) arm_index: HashMap<String, usize>,
    pub actions: Vec<Action>,
    pub(crate) action_index: HashMap<String, usize>,
    /// The release's own rate per arm, when it carries or was given prices.
    pub(crate) package_rates: Option<Vec<Rates>>,
    /// Stage two's cost weight.
    pub(crate) stage_two_cost_weight: f64,
    /// Whether a request's prices change decisions.
    pub(crate) live_prices: bool,
}

impl fmt::Debug for Release {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Release")
            .field("name", &self.name)
            .field("arms", &self.arms.len())
            .field("actions", &self.actions.len())
            .finish_non_exhaustive()
    }
}

fn read(path: &Path) -> Result<Vec<u8>, ReleaseError> {
    std::fs::read(path).map_err(|e| ReleaseError::Read(format!("{}: {e}", path.display())))
}

fn json(path: &Path, bytes: &[u8]) -> Result<Value, ReleaseError> {
    serde_json::from_slice(bytes).map_err(|e| malformed(format!("{}: {e}", path.display())))
}

fn strings(value: &Value, what: &str) -> Result<Vec<String>, ReleaseError> {
    value
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .ok_or_else(|| malformed(format!("{what} is not a list of strings")))
}

fn text(value: &Value, what: &str) -> Result<String, ReleaseError> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| malformed(format!("{what} is not a string")))
}

fn number(value: &Value, what: &str) -> Result<f64, ReleaseError> {
    value
        .as_f64()
        .filter(|n| n.is_finite())
        .ok_or_else(|| malformed(format!("{what} is not a number")))
}

/// A rate entry: four legs, each a nonnegative number or null.
pub(crate) fn rates(value: &Value) -> Option<Rates> {
    let legs = value.as_array().filter(|legs| legs.len() == LEGS)?;
    let mut out = [None; LEGS];
    for (slot, leg) in out.iter_mut().zip(legs) {
        *slot = match leg {
            Value::Null => None,
            other => Some(other.as_f64().filter(|r| r.is_finite() && *r >= 0.0)?),
        };
    }
    Some(out)
}

/// The one endpoint a worker's requests are served at: a provider pin of
/// exactly one endpoint that may not fall back.
fn endpoint_tag(worker: &Value) -> Option<String> {
    let provider = worker.get("extra_body")?.get("provider")?.as_object()?;
    if provider.get("allow_fallbacks") != Some(&Value::Bool(false)) {
        return None;
    }
    let pins = match provider.get("only") {
        Some(Value::Null) | None => provider.get("order")?,
        Some(only) => only,
    };
    match pins.as_array()?.as_slice() {
        [Value::String(tag)] if !tag.is_empty() => Some(tag.clone()),
        _ => None,
    }
}

fn thinking(action: &Value) -> Thinking {
    let level = action
        .get("level")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match action.get("control").filter(|c| c.is_object()) {
        Some(control) => Thinking {
            effort: control
                .get("base")
                .and_then(|base| base.get("native"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            budget_tokens: control.get("budget_tokens").and_then(Value::as_u64),
            steering_level: level,
            control_id: action
                .get("control_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
        },
        None => Thinking {
            effort: action
                .get("effort")
                .and_then(Value::as_str)
                .map(str::to_owned),
            budget_tokens: action.get("reasoning_max_tokens").and_then(Value::as_u64),
            steering_level: level,
            control_id: None,
        },
    }
}

fn require_recipe(recipe: &Value) -> Result<f64, ReleaseError> {
    let expect = |key: &str, want: Value| {
        if recipe.get(key) == Some(&want) {
            Ok(())
        } else {
            Err(ReleaseError::Unsupported(format!(
                "stage-two recipe {key} is not {want}"
            )))
        }
    };
    expect("architecture", "mlp".into())?;
    expect("retrieval", "none".into())?;
    expect("pricing", "analytic_usage".into())?;
    expect("usage_objective", "scaled_raw_mean_mse".into())?;
    expect("temporal_difference", false.into())?;
    expect("reference_policy", Value::Null)?;
    expect("thinking_representation", "level".into())?;
    if recipe.get("history_arm_identity") == Some(&"masked".into()) {
        return Err(ReleaseError::Unsupported(
            "stage-two recipe masks history attribution".into(),
        ));
    }
    if recipe.get("usage_scale").and_then(Value::as_f64) != Some(1_000_000.0) {
        return Err(ReleaseError::Unsupported(
            "stage-two recipe usage_scale is not 1e6".into(),
        ));
    }
    let weight = number(&recipe["cost_weight"], "recipe.cost_weight")?;
    if weight <= 0.0 {
        return Err(ReleaseError::Unsupported(
            "stage-two recipe has no cost weight".into(),
        ));
    }
    Ok(weight)
}

/// The stage-one encoder's chunk size in the release in `directory`
/// (`arc.serving.stage_one.encoder.chunk_tokens`): the tokens per encoder
/// decode call its readouts were made with.
pub fn stage_one_chunk_tokens(directory: &Path) -> Result<u32, ReleaseError> {
    let path = ReleaseFiles::in_dir(directory).config;
    let config = json(&path, &read(&path)?)?;
    config["arc"]["serving"]["stage_one"]["encoder"]["chunk_tokens"]
        .as_u64()
        .and_then(|tokens| u32::try_from(tokens).ok())
        .filter(|tokens| *tokens > 0)
        .ok_or_else(|| malformed("config.json names no stage-one encoder chunk_tokens"))
}

impl Release {
    /// Loads the release in `directory`.
    pub fn load(directory: &Path) -> Result<Self, ReleaseError> {
        Self::from_files(&ReleaseFiles::in_dir(directory))
    }

    pub fn from_files(files: &ReleaseFiles) -> Result<Self, ReleaseError> {
        let config = json(&files.config, &read(&files.config)?)?;
        let arc = &config["arc"];
        if !arc.is_object() {
            return Err(malformed("config.json has no arc section"));
        }
        let serving = match arc.get("serving") {
            Some(serving) if serving.is_object() => serving,
            _ => return Err(ReleaseError::NoServingRecord),
        };
        let package_bytes = read(&files.package)?;
        let package_sha256 = canon::sha256_hex(&package_bytes);
        if arc.get("package_sha256").and_then(Value::as_str) != Some(&package_sha256) {
            return Err(malformed(
                "package.json is not the package config.json names",
            ));
        }
        let package = json(&files.package, &package_bytes)?;
        let tokenizer = ArcTokenizer::from_release(
            &read(&files.tokenizer)?,
            &String::from_utf8(read(&files.tokenizer_config)?)
                .map_err(|_| malformed("tokenizer_config.json is not UTF-8"))?,
        )
        .map_err(|e| malformed(e.to_string()))?;

        for key in ["fixed_arms", "proxy_arms"] {
            if serving
                .get(key)
                .and_then(Value::as_object)
                .is_some_and(|m| !m.is_empty())
            {
                return Err(ReleaseError::Unsupported(format!("the release has {key}")));
            }
        }
        let decision = &package["decision"];
        if decision.get("mode") != Some(&"factored".into())
            || !decision.get("operating_bounds").is_none_or(Value::is_null)
        {
            return Err(ReleaseError::Unsupported(
                "the package's stage two is not a fixed-weight factored decision".into(),
            ));
        }
        let stage_two_cost_weight = require_recipe(&serving["recipe"])?;

        // Arms, their workers and the actions they serve.
        let workers = serving["workers"]
            .as_object()
            .ok_or_else(|| malformed("arc.serving.workers is not an object"))?;
        let bindings = serving
            .get("model_name_bindings")
            .and_then(Value::as_object);
        let bodies = serving["arms"]
            .as_array()
            .ok_or_else(|| malformed("arc.serving.arms is not a list"))?;
        let mut arms = Vec::with_capacity(bodies.len());
        let mut arm_index = HashMap::with_capacity(bodies.len());
        for body in bodies {
            let id = canon::digest(body);
            let model = text(&body["model"], "an arm's model")?;
            let worker = workers.get(&id);
            let model_id = worker
                .and_then(|w| w.get("model"))
                .and_then(Value::as_str)
                .filter(|m| !m.is_empty())
                .or_else(|| bindings.and_then(|b| b.get(&model)).and_then(Value::as_str))
                .unwrap_or(&model)
                .to_owned();
            let group = body
                .get("catalog_id")
                .and_then(Value::as_str)
                .filter(|catalog| catalog.starts_with("policy:"))
                .map(|catalog| {
                    canon::digest(&Value::Array(vec![
                        catalog.into(),
                        body["model"].clone(),
                        body.get("provider").cloned().unwrap_or(Value::Null),
                        body.get("provider_route").cloned().unwrap_or(Value::Null),
                        body.get("effort").cloned().unwrap_or(Value::Null),
                    ]))
                });
            if arm_index.insert(id.clone(), arms.len()).is_some() {
                return Err(malformed("the arm catalog repeats an arm"));
            }
            arms.push(Arm {
                id,
                model,
                served: worker.is_some(),
                action: None,
                model_id,
                endpoint_tag: worker.and_then(endpoint_tag),
                group,
            });
        }
        if let Some(stray) = workers.keys().find(|id| !arm_index.contains_key(*id)) {
            // Also the check that this crate derives arm ids as the release does.
            return Err(malformed(format!(
                "worker {} names no arm of the catalog",
                stray.chars().take(12).collect::<String>()
            )));
        }
        let mut actions = Vec::new();
        let mut action_index = HashMap::new();
        for body in package["actions"]
            .as_array()
            .ok_or_else(|| malformed("package.json has no actions list"))?
        {
            let id = text(&body["action_id"], "an action id")?;
            let mut members = Vec::new();
            for arm in strings(&body["trained_arm_ids"], "trained_arm_ids")? {
                let index = *arm_index
                    .get(&arm)
                    .ok_or_else(|| malformed("an action names an arm outside the catalog"))?;
                arms[index].action = Some(actions.len());
                members.push(index);
            }
            if action_index.insert(id.clone(), actions.len()).is_some() {
                return Err(malformed("the package repeats an action"));
            }
            actions.push(Action {
                id,
                model: text(&body["model"], "an action's model")?,
                arms: members,
                thinking: thinking(body),
            });
        }

        // Tensors.
        let head = StageOneHead::load(&Tensors::read(&files.weights, "stage_one.")?)?;
        let stage_two_record = &arc["stage_two"];
        if stage_two_record["decision"].get("rebuildable") != Some(&Value::Bool(true)) {
            return Err(ReleaseError::Unsupported(
                "the release's stage two cannot be rebuilt from its tensors".into(),
            ));
        }
        let stage_two = StageTwo::load(
            &Tensors::read(&files.weights, "stage_two.")?,
            &strings(
                &stage_two_record["decision"]["support_columns"],
                "arc.stage_two.decision.support_columns",
            )?,
        )?;
        if stage_two.arms() != arms.len() {
            return Err(malformed("stage two scores another arm catalog"));
        }

        // Stage one's record.
        let record = &arc["stage_one"];
        let one = &record["serving"];
        if !one.is_object() {
            return Err(ReleaseError::NoServingRecord);
        }
        if one.get("candidate_unit") != Some(&"model".into()) {
            return Err(ReleaseError::Unsupported(
                "stage one's candidates are not models".into(),
            ));
        }
        let candidates: Vec<String> = record["candidates"]
            .as_array()
            .ok_or_else(|| malformed("arc.stage_one.candidates is not a list"))?
            .iter()
            .map(|c| text(&c["model"], "a candidate's model"))
            .collect::<Result<_, _>>()?;
        let models = strings(&record["models"], "arc.stage_one.models")?;
        let count = candidates.len();
        if count != head.candidates() || stage_two.dim() != head.dim() {
            return Err(malformed(
                "the stage-one head scores another candidate list",
            ));
        }
        let supported: Vec<bool> = one["supported"]
            .as_array()
            .filter(|flags| flags.len() == count)
            .map(|flags| flags.iter().map(|f| f.as_bool() == Some(true)).collect())
            .ok_or_else(|| malformed("stage-one support is not one flag per candidate"))?;
        let known_rows = one["usage_known"]
            .as_array()
            .filter(|rows| rows.len() == models.len())
            .ok_or_else(|| malformed("stage-one usage_known is not one row per model"))?;
        let usage_known: Vec<bool> = candidates
            .iter()
            .map(|model| {
                models
                    .iter()
                    .position(|m| m == model)
                    .and_then(|row| known_rows[row].as_array())
                    .is_some_and(|row| {
                        row.len() == LEGS && row.iter().all(|k| k.as_bool() == Some(true))
                    })
            })
            .collect();
        let selector_withheld: HashSet<&str> = one["withheld"]
            .as_object()
            .map(|models| models.keys().map(String::as_str).collect())
            .unwrap_or_default();
        let unmapped_models: HashSet<String> = record
            .get("ensemble")
            .and_then(|e| e.get("unmapped_models"))
            .map(|m| strings(m, "unmapped_models"))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .collect();

        let body = &serving["stage_one"];
        let settings_record = &body["config"];
        if !settings_record.is_object() {
            return Err(ReleaseError::NoServingRecord);
        }
        if !settings_record
            .get("exploration")
            .is_none_or(Value::is_null)
        {
            return Err(ReleaseError::Unsupported(
                "stage one declares exploration".into(),
            ));
        }
        let regime_name = settings_record
            .get("regime")
            .and_then(Value::as_str)
            .unwrap_or("fixed");
        let regime = strings(&record["regimes"], "arc.stage_one.regimes")?
            .iter()
            .position(|r| r == regime_name)
            .filter(|r| *r < head.regimes())
            .ok_or_else(|| malformed("stage one's regime is not one of the head's"))?;
        let excluded: HashSet<String> = settings_record
            .get("excluded_models")
            .map(|m| strings(m, "excluded_models"))
            .transpose()?
            .unwrap_or_default()
            .into_iter()
            .collect();
        let latency_weight = settings_record
            .get("latency_weight")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let factors = settings_record
            .get("latency")
            .and_then(|l| l.get("factors"))
            .and_then(Value::as_object);
        if (latency_weight != 0.0) != factors.is_some() {
            return Err(malformed("a latency weight is declared with its table"));
        }
        let log_latency = factors.map(|factors| {
            candidates
                .iter()
                .map(|model| {
                    factors
                        .get(model)
                        .and_then(Value::as_f64)
                        .map_or(f64::NAN, f64::ln)
                })
                .collect::<Vec<f64>>()
        });
        let offer_auxiliary =
            settings_record.get("offer_user_traffic_only") == Some(&Value::Bool(true));
        let mut candidate_arms = vec![Vec::new(); count];
        let by_model: HashMap<&str, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, model)| (model.as_str(), i))
            .collect();
        for (index, arm) in arms.iter().enumerate() {
            if let (true, Some(candidate)) = (arm.served, by_model.get(arm.model.as_str())) {
                candidate_arms[*candidate].push(index);
            }
        }
        let withheld: Vec<Option<Withheld>> = candidates
            .iter()
            .enumerate()
            .map(|(i, model)| {
                if selector_withheld.contains(model.as_str()) {
                    Some(Withheld::BySelector)
                } else if excluded.contains(model) {
                    Some(Withheld::Excluded)
                } else if !supported[i] {
                    Some(Withheld::Unsupported)
                } else if head.auxiliary[i] && !offer_auxiliary {
                    Some(Withheld::UserTrafficOnly)
                } else if log_latency.as_ref().is_some_and(|l| l[i].is_nan()) {
                    Some(Withheld::NoLatencyFactor)
                } else {
                    None
                }
            })
            .collect();
        let manifest = &package["stage_one"];
        let bounds = manifest
            .get("operating_bounds")
            .and_then(|b| b.get("cost_weight"))
            .and_then(Value::as_array)
            .filter(|b| b.len() == 2)
            .and_then(|b| Some((b[0].as_f64()?, b[1].as_f64()?)));
        let mut presets = BTreeMap::new();
        for (name, weight) in manifest
            .get("presets")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            presets.insert(name.clone(), number(weight, "a preset's cost weight")?);
        }
        let mut tool_free = BTreeMap::new();
        for (name, entry) in manifest
            .get("tool_free_overrides")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            tool_free.insert(
                name.clone(),
                ToolFree {
                    v_multiplier: number(&entry["v_multiplier"], "v_multiplier")?,
                    excluded_models: entry
                        .get("excluded_models")
                        .map(|m| strings(m, "tool-free excluded_models"))
                        .transpose()?
                        .unwrap_or_default(),
                    latency_weight: entry
                        .get("latency_weight")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0),
                },
            );
        }
        let settings = StageOneSettings {
            boundaries: strings(&settings_record["boundaries"], "stage-one boundaries")?,
            cost_weight: number(&settings_record["cost_weight"], "stage-one cost_weight")?,
            stay_margin: settings_record
                .get("stay_margin")
                .and_then(Value::as_f64)
                .unwrap_or(1.0),
            regime,
            latency_weight,
            log_latency,
            cost_weight_bounds: bounds,
            presets,
            tool_free,
        };
        if settings.stay_margin <= 0.0 {
            // With no bootstrap draws, a positive stay margin is what holds
            // the model at a warm boundary; this crate relies on it.
            return Err(ReleaseError::Unsupported(
                "stage one has no stay margin".into(),
            ));
        }

        let mut release = Self {
            name: arc
                .get("release")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            package_sha256,
            tokenizer,
            unmapped: candidates
                .iter()
                .map(|m| unmapped_models.contains(m))
                .collect(),
            candidates,
            withheld,
            candidate_arms,
            usage_known,
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
            live_prices: package["pricing"].get("live_prices_affect_decisions")
                == Some(&Value::Bool(true)),
        };
        if let Some(prices) = serving.get("prices").filter(|p| p.is_object()) {
            release.set_prices(prices)?;
        }
        Ok(release)
    }

    /// Gives the release its own rate per arm from a price scenario file
    /// (`{"rates": {arm id: [input, cache_read, cache_write, output]}}`).
    /// A request's `routing_prices` are read over these.
    /// Whether a model the request places locally leaves stage one's
    /// latency comparison: it carries no latency term, and one withheld only
    /// because the latency table does not name it becomes offered.
    ///
    /// Off by default. The policy runtime this release was built with does
    /// not apply the rule, so its decisions (and the release's goldens) are
    /// reproduced with it off; with it off, a locally placed model the table
    /// does not name is chosen only when it is the one model offered.
    pub fn with_local_placement_exemption(mut self, exempt: bool) -> Self {
        self.local_placement_exempt = exempt;
        self
    }

    pub fn with_prices(mut self, path: &Path) -> Result<Self, ReleaseError> {
        let scenario = json(path, &read(path)?)?;
        self.set_prices(&scenario)?;
        Ok(self)
    }

    /// Gives the release its own rate per arm from a per-model price card
    /// (`{"models": {model id: [input, cache_read, cache_write, output]}}`,
    /// USD per million tokens): every arm of a listed model, named by its
    /// price-table id or its policy name, gets that model's rates. Arms of
    /// unlisted models stay unpriced. A request's `routing_prices` are read
    /// over these.
    pub fn with_model_prices(mut self, path: &Path) -> Result<Self, ReleaseError> {
        let card = json(path, &read(path)?)?;
        let models = card["models"]
            .as_object()
            .ok_or_else(|| malformed("the price card has no models"))?;
        let mut card_rates = HashMap::with_capacity(models.len());
        for (model, entry) in models {
            let legs =
                rates(entry).ok_or_else(|| malformed("a price card entry is not four rates"))?;
            card_rates.insert(model.as_str(), legs);
        }
        let table = self
            .arms
            .iter()
            .map(|arm| {
                card_rates
                    .get(arm.model_id.as_str())
                    .or_else(|| card_rates.get(arm.model.as_str()))
                    .copied()
                    .unwrap_or([None; LEGS])
            })
            .collect();
        self.package_rates = Some(table);
        Ok(self)
    }

    fn set_prices(&mut self, scenario: &Value) -> Result<(), ReleaseError> {
        if scenario
            .get("long_context")
            .and_then(Value::as_object)
            .is_some_and(|tiers| !tiers.is_empty())
        {
            return Err(ReleaseError::Unsupported(
                "the price scenario has long-context tiers".into(),
            ));
        }
        let entries = scenario["rates"]
            .as_object()
            .ok_or_else(|| malformed("the price scenario has no rates"))?;
        let mut table = vec![[None; LEGS]; self.arms.len()];
        for (arm, entry) in entries {
            let index = *self
                .arm_index
                .get(arm)
                .ok_or_else(|| malformed("the price scenario prices an arm outside the catalog"))?;
            table[index] =
                rates(entry).ok_or_else(|| malformed("a price entry is not four rates"))?;
        }
        self.package_rates = Some(table);
        Ok(())
    }

    pub fn action(&self, id: &str) -> Option<&Action> {
        self.action_index.get(id).map(|index| &self.actions[*index])
    }

    /// The stage-one candidate a model is, if it is one.
    pub(crate) fn candidate_of_model(&self, model: &str) -> Option<usize> {
        self.candidates.iter().position(|m| m == model)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stage_one_chunk_is_read_from_the_serving_record() {
        let dir = std::env::temp_dir().join(format!("arc-chunk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let write = |config: &str| std::fs::write(dir.join("config.json"), config).expect("write");
        write(r#"{"arc": {"serving": {"stage_one": {"encoder": {"chunk_tokens": 8192}}}}}"#);
        assert_eq!(stage_one_chunk_tokens(&dir).expect("a chunk size"), 8192);
        write(r#"{"arc": {"serving": {"stage_one": {"encoder": {"chunk_tokens": 0}}}}}"#);
        assert!(stage_one_chunk_tokens(&dir).is_err());
        write(r#"{"arc": {}}"#);
        assert!(stage_one_chunk_tokens(&dir).is_err());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
