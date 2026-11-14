//! A Routes API V1 request into the policy's decide input.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::PRICE_LEGS;
use crate::catalog::Catalog;
use crate::digest::{sha256_hex, turn_key};
use crate::error::Refusal;
use crate::execution::{
    self, ActionCache, Attribution, CapacityFact, Event, ExecutionContext, Pinned, Turn,
};
use crate::policy::{Policy, parse_policy};

const REQUEST_SCHEMA: &str = "rayline.arc.policy-decision-request.v1";
const ROUTING_PRICES_SCHEMA: &str = "arc-routing-prices.v1";
const VALUE_USD_MIN: f64 = 0.1;
const VALUE_USD_MAX: f64 = 30.0;
const MAX_PRICED_MODELS: usize = 256;
const MAX_MODEL_PATTERNS: usize = 256;
const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
/// Every `routing` member the route surface reads, in checking order.
const ROUTING_MEMBERS: &[&str] = &[
    "value_usd",
    "temperature",
    "models",
    "excluded_models",
    "prices",
    "price_profile",
    "response_fields",
    "execution",
    "policy",
];
/// The members this router validates but does not act on. A request that
/// sends one is answered with it listed in `routing.unapplied_keys`.
const NOT_APPLIED: &[&str] = &[
    "value_usd",
    "temperature",
    "models",
    "excluded_models",
    "price_profile",
    "response_fields",
];

/// A fixed operating point: the value placed on one successful result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    Swift,
    Sage,
}

impl Tier {
    /// Result names, in response order.
    pub const ALL: [Tier; 2] = [Tier::Swift, Tier::Sage];

    pub fn name(self) -> &'static str {
        match self {
            Tier::Swift => "swift",
            Tier::Sage => "sage",
        }
    }

    pub fn value_usd(self) -> f64 {
        match self {
            Tier::Swift => 0.1,
            Tier::Sage => 4.5,
        }
    }

    /// Stage one's weight on cost at this point: `1 / value_usd`.
    pub fn cost_weight(self) -> f64 {
        1.0 / self.value_usd()
    }
}

/// The headers of a `/v1/routes` request this contract reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestHeaders {
    /// `X-Conversation-Id`
    pub conversation_id: Option<String>,
    /// `X-Rayline-Branch-Id`
    pub branch_id: Option<String>,
    /// `X-Rayline-Wire-Format`
    pub wire_format: Option<String>,
    /// `X-Rayline-Cell`, echoed on the answer.
    pub cell: Option<String>,
}

/// What a parse takes from its process rather than from the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseContext {
    /// Mixed into every episode hash, so two processes never share an episode.
    pub salt: String,
    /// When caller prices were read: `YYYY-MM-DDTHH:MM:SSZ`.
    pub as_of: String,
    /// The episode of a request that names no conversation.
    pub nonce: String,
}

impl ParseContext {
    /// The current time and a fresh random nonce.
    pub fn now(salt: &str) -> Self {
        let now = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .ok()
            .and_then(|now| now.format(&Rfc3339).ok())
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_owned());
        Self {
            salt: salt.to_owned(),
            as_of: now,
            nonce: format!("{:032x}", rand::random::<u128>()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRef {
    pub alias: String,
    pub package_sha256: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EpisodeMode {
    /// Extends the episode's retained session with the new tokens only.
    Strict,
    /// Depends on the request alone.
    Relaxed,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OperatingPoint {
    pub name: &'static str,
    pub cost_weight: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Selection {
    pub available_action_ids: Vec<String>,
    pub operating_point: OperatingPoint,
    pub tier_points: Vec<OperatingPoint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_action_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_served_action_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CacheContext {
    pub actions: Vec<ActionCache>,
}

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Legs {
    pub input: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub output: f64,
}

impl Legs {
    pub fn array(self) -> [f64; 4] {
        [self.input, self.cache_read, self.cache_write, self.output]
    }
}

/// The caller's own rows. Not exclusive: every other model keeps the
/// package's rate.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoutingPrices {
    pub schema: &'static str,
    pub basis: &'static str,
    pub exclusive: bool,
    pub store_version: &'static str,
    pub as_of: String,
    pub rates: BTreeMap<String, Legs>,
}

/// The policy's decide request. It serializes to the decide body.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DecideInput {
    pub schema_version: &'static str,
    pub package: PackageRef,
    pub episode_id_hash: String,
    pub episode_mode: EpisodeMode,
    pub context_epoch: String,
    pub request_format: &'static str,
    /// The harness body's own members, unchanged.
    pub request: Value,
    pub attribution: Vec<Attribution>,
    pub selection: Selection,
    pub response_fields: [&'static str; 2],
    pub policy: Policy,
    pub event: Event,
    pub capacity: Vec<CapacityFact>,
    pub turn_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reconsult: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_context: Option<CacheContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub routing_prices: Option<RoutingPrices>,
}

/// What the answer must restate about its request.
#[derive(Debug, Clone, PartialEq)]
pub struct Restate {
    pub turn: Turn,
    pub preset: String,
    pub pinned: Pinned,
    /// The offer digest the caller pinned, checked once the offer is known.
    pub expect_offer_sha256: Option<String>,
    /// The result the request's model names.
    pub primary: Tier,
    /// The caller's price rows, by the caller's model id.
    pub prices: BTreeMap<String, Legs>,
    /// Targets the serving package does not hold: left out of the offer and
    /// named in every receipt.
    pub unknown: Vec<String>,
    /// The offered actions, in target order.
    pub offered: Vec<String>,
    /// Each offered action's worker.
    pub workers: BTreeMap<String, String>,
    pub unapplied_keys: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Parsed {
    pub decide: DecideInput,
    pub restate: Restate,
}

fn resolve_model(requested: Option<&Value>) -> Result<Option<Tier>, Refusal> {
    let Some(requested) = requested.and_then(Value::as_str).filter(|m| !m.is_empty()) else {
        return Err(Refusal::new(
            "invalid_body",
            "model must be a non-empty string.",
        ));
    };
    // `arc-swift-1.0`, its short spellings, and its data-mode entries.
    let name = requested
        .strip_suffix("-1.0")
        .or_else(|| requested.strip_suffix("-1"))
        .unwrap_or(requested);
    let product = name
        .strip_suffix("-contributor")
        .or_else(|| name.strip_suffix("-zdr"))
        .unwrap_or(name);
    match product {
        "arc-swift" => return Ok(Some(Tier::Swift)),
        "arc-sage" => return Ok(Some(Tier::Sage)),
        // `arc-1.0` and its spellings: a custom value, which no tier decides.
        "arc" if requested != "arc" => return Ok(None),
        _ => {}
    }
    let bare = requested.strip_prefix("rayline/").unwrap_or(requested);
    if bare == "arc" || bare.starts_with("arc-") || bare.starts_with("arc@") {
        return Err(Refusal::new(
            "unknown_model",
            format!("Unknown ARC model or pin: {requested}."),
        ));
    }
    Ok(None)
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64().filter(|n| n.is_finite()),
        _ => None,
    }
}

fn model_patterns(value: &Value, member: &str) -> Result<(), Refusal> {
    let distinct = value.as_array().filter(|entries| {
        entries.len() <= MAX_MODEL_PATTERNS
            && entries
                .iter()
                .all(|e| e.as_str().is_some_and(|s| !s.is_empty()))
            && entries
                .iter()
                .enumerate()
                .all(|(i, entry)| !entries[..i].contains(entry))
    });
    if distinct.is_none() {
        return Err(Refusal::new(
            "models_invalid",
            format!(
                "routing.{member} must be an array of at most {MAX_MODEL_PATTERNS} distinct, non-empty model ids or * patterns."
            ),
        ));
    }
    Ok(())
}

fn prices(value: &Value) -> Result<BTreeMap<String, Legs>, Refusal> {
    let legs_named = PRICE_LEGS.join(", ");
    let Some(sent) = value
        .as_object()
        .filter(|sent| !sent.is_empty() && sent.len() <= MAX_PRICED_MODELS)
    else {
        return Err(Refusal::new(
            "prices_invalid",
            format!(
                "routing.prices must map 1 to {MAX_PRICED_MODELS} model ids to USD-per-million-token legs ({legs_named})."
            ),
        ));
    };
    let mut parsed = BTreeMap::new();
    for (model, legs) in sent {
        if model.is_empty() {
            return Err(Refusal::new(
                "prices_invalid",
                "routing.prices keys are non-empty model ids.",
            ));
        }
        let Some(legs) = legs.as_object().filter(|legs| !legs.is_empty()) else {
            return Err(Refusal::new(
                "prices_invalid",
                format!(
                    "routing.prices[{model:?}] must be an object of one or more of {legs_named}."
                ),
            ));
        };
        if legs.keys().any(|leg| !PRICE_LEGS.contains(&leg.as_str())) {
            return Err(Refusal::new(
                "prices_invalid",
                format!("routing.prices[{model:?}] has unsupported member(s)."),
            ));
        }
        for leg in PRICE_LEGS {
            if let Some(price) = legs.get(leg) {
                if !number(price).is_some_and(|price| price >= 0.0) {
                    return Err(Refusal::new(
                        "price_leg_invalid",
                        format!(
                            "routing.prices[{model:?}].{leg} must be a finite number >= 0 USD per million tokens."
                        ),
                    ));
                }
            }
        }
        // The hosted router fills a leg the caller left out from its catalog.
        // There is no catalog here, so a row is all four legs or refused.
        let leg = |name: &str| legs.get(name).and_then(number);
        let (Some(input), Some(cache_read), Some(cache_write), Some(output)) = (
            leg("input"),
            leg("cache_read"),
            leg("cache_write"),
            leg("output"),
        ) else {
            return Err(Refusal::new(
                "prices_invalid",
                format!(
                    "routing.prices[{model:?}] must state all of {legs_named}: a local route has no catalog to fill a missing leg from."
                ),
            ));
        };
        parsed.insert(
            model.clone(),
            Legs {
                input,
                cache_read,
                cache_write,
                output,
            },
        );
    }
    Ok(parsed)
}

fn price_profile(value: &Value) -> Result<(), Refusal> {
    let id_ok = |id: &str| {
        (1..=128).contains(&id.len())
            && id.starts_with(|c: char| c.is_ascii_alphanumeric())
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
    };
    let valid = value.as_object().is_some_and(|profile| {
        profile.keys().all(|key| key == "id" || key == "version")
            && profile.get("id").and_then(Value::as_str).is_some_and(id_ok)
            && profile.get("version").is_none_or(|version| {
                number(version)
                    .is_some_and(|v| v.fract() == 0.0 && (1.0..=MAX_SAFE_INTEGER).contains(&v))
            })
    });
    if !valid {
        return Err(Refusal::new(
            "price_profile_invalid",
            "routing.price_profile must be {id, version?}: a valid profile id and, optionally, a positive integer version.",
        ));
    }
    Ok(())
}

/// The `routing` block's members before `execution`, in the contract's
/// checking order. Returns the caller's prices.
fn check_members(
    routing: &Map<String, Value>,
    tier: Option<Tier>,
) -> Result<Option<BTreeMap<String, Legs>>, Refusal> {
    let mut unknown: Vec<&str> = routing
        .keys()
        .map(String::as_str)
        .filter(|key| !ROUTING_MEMBERS.contains(key))
        .collect();
    if !unknown.is_empty() {
        unknown.sort_unstable();
        return Err(Refusal::new(
            "routing_unknown_member",
            format!("routing has unsupported member(s): {}.", unknown.join(", ")),
        ));
    }
    if let Some(value) = routing.get("value_usd") {
        let Some(value) = number(value) else {
            return Err(Refusal::new(
                "value_invalid",
                "routing.value_usd must be a finite number of USD.",
            ));
        };
        if !(VALUE_USD_MIN..=VALUE_USD_MAX).contains(&value) {
            return Err(Refusal::new(
                "value_out_of_bounds",
                format!(
                    "routing.value_usd must lie in [{VALUE_USD_MIN}, {VALUE_USD_MAX:?}]; it is never clamped."
                ),
            ));
        }
        if let Some(tier) = tier.filter(|tier| tier.value_usd() != value) {
            return Err(Refusal::new(
                "tier_value_immutable",
                format!(
                    "arc-{}-1.0 decides at a fixed value_usd of {}; use arc-1.0 for a custom value.",
                    tier.name(),
                    tier.value_usd()
                ),
            ));
        }
    }
    if let Some(temperature) = routing.get("temperature") {
        if !number(temperature).is_some_and(|t| t >= 0.0) {
            return Err(Refusal::new(
                "temperature_invalid",
                "routing.temperature must be a finite number >= 0 (a softmax temperature over normalized scores; 0 is greedy).",
            ));
        }
    }
    for member in ["models", "excluded_models"] {
        if let Some(value) = routing.get(member) {
            model_patterns(value, member)?;
        }
    }
    let sent_prices = routing.get("prices").map(prices).transpose()?;
    if let Some(profile) = routing.get("price_profile") {
        price_profile(profile)?;
    }
    if let Some(fields) = routing.get("response_fields") {
        if fields.as_array().map(Vec::as_slice) != Some(&[Value::from("predictions")]) {
            return Err(Refusal::new(
                "response_fields_invalid",
                "routing.response_fields must be a non-empty array of distinct values from [\"predictions\"].",
            ));
        }
    }
    Ok(sent_prices)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64() != Some(0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

/// The wire format the decide request names and the body members it reads.
fn request_members(
    body: &mut Map<String, Value>,
    wire_format: Option<&str>,
) -> (&'static str, Value) {
    let format = match wire_format {
        Some("anthropic.messages.v1") => "anthropic_messages",
        Some("openai.chat.v1") => "openai_chat",
        Some("openai.responses.v1") => "openai_responses",
        _ if execution::is_responses(body) => "openai_responses",
        _ => "anthropic_messages",
    };
    let mut request = Map::new();
    if format == "openai_responses" {
        for key in ["input", "instructions"] {
            request.insert(key.to_owned(), body.remove(key).unwrap_or(Value::Null));
        }
        let tools = body.remove("tools").filter(truthy);
        request.insert(
            "tools".to_owned(),
            tools.unwrap_or_else(|| Value::Array(Vec::new())),
        );
    } else {
        for key in ["system", "tools", "messages"] {
            if let Some(value) = body.remove(key).filter(|value| !value.is_null()) {
                request.insert(key.to_owned(), value);
            }
        }
    }
    (format, Value::Object(request))
}

/// Targets the package does not hold are left out of the offer and named in
/// the receipt, not refused. Returns their ids.
fn drop_unknown_targets(body: &mut Map<String, Value>, catalog: &Catalog) -> Vec<String> {
    let Some(targets) = body
        .get_mut("routing")
        .and_then(|routing| routing.get_mut("execution"))
        .and_then(Value::as_object_mut)
        .and_then(|execution| execution.get_mut("targets"))
        .and_then(Value::as_array_mut)
    else {
        return Vec::new();
    };
    let mut unknown = Vec::new();
    targets.retain(|target| {
        match target
            .as_object()
            .and_then(|target| target.get("action_id"))
            .and_then(Value::as_str)
        {
            Some(id) if !catalog.actions.contains_key(id) => {
                unknown.push(id.to_owned());
                false
            }
            _ => true,
        }
    });
    unknown
}

/// A `/v1/routes` request body and headers into the decide input and what the
/// answer restates, or the refusal the contract names. Checks run in a fixed
/// order: the model; the `routing` block's shape and members; `value_usd`,
/// `temperature`, `models`, `excluded_models`, `prices`, `price_profile`,
/// `response_fields`; `execution` (shape, counts, attribution); `policy`; then
/// the envelope against the serving package (catalog, pins, hold).
pub fn parse(
    catalog: &Catalog,
    body: Value,
    headers: &RequestHeaders,
    context: &ParseContext,
) -> Result<Parsed, Refusal> {
    let Value::Object(mut body) = body else {
        return Err(Refusal::new(
            "invalid_body",
            "The request body must be a JSON object.",
        ));
    };
    let unknown = drop_unknown_targets(&mut body, catalog);
    let tier = resolve_model(body.get("model"))?;
    let (sent_prices, parsed, unapplied_keys) = match body.get("routing") {
        None => (None, None, Vec::new()),
        Some(Value::Object(routing)) => {
            let sent_prices = check_members(routing, tier)?;
            let execution = match routing.get("execution") {
                Some(value)
                    if value.get("version").and_then(Value::as_str) == Some(execution::VERSION) =>
                {
                    Some(execution::parse_execution(value, &body)?)
                }
                Some(_) => {
                    return Err(Refusal::new(
                        "execution_invalid",
                        format!("routing.execution must be version {}.", execution::VERSION),
                    ));
                }
                None => None,
            };
            let policy = routing.get("policy").map(parse_policy).transpose()?;
            let unapplied = NOT_APPLIED
                .iter()
                .filter(|key| routing.contains_key(**key))
                .map(|key| (*key).to_owned())
                .collect();
            (sent_prices, execution.zip(policy), unapplied)
        }
        Some(_) => {
            return Err(Refusal::new(
                "routing_invalid",
                "routing must be an object.",
            ));
        }
    };
    let Some(primary) = tier else {
        return Err(Refusal::new(
            "unknown_model",
            "this local router serves arc-swift-1.0 and arc-sage-1.0.",
        ));
    };
    let Some((execution, policy)) = parsed else {
        return Err(Refusal::new(
            "execution_invalid",
            format!(
                "this local router serves routing.execution version {} with routing.policy.",
                execution::VERSION
            ),
        ));
    };
    let pinned = execution::bind(&execution, catalog)?;
    let (request_format, request) = request_members(&mut body, headers.wire_format.as_deref());
    let routing_prices = sent_prices.as_ref().map(|rates| RoutingPrices {
        schema: ROUTING_PRICES_SCHEMA,
        basis: "caller",
        exclusive: false,
        store_version: "local-routes",
        as_of: context.as_of.clone(),
        rates: rates.clone(),
    });
    let decide = decide_input(
        catalog,
        &execution,
        policy,
        primary,
        (request_format, request),
        routing_prices,
        episode(headers, context),
    );
    let ExecutionContext {
        turn,
        expect,
        targets,
        ..
    } = execution;
    Ok(Parsed {
        restate: Restate {
            turn,
            preset: decide.policy.preset.clone(),
            pinned,
            expect_offer_sha256: expect.and_then(|expect| expect.offer_sha256),
            primary,
            prices: sent_prices.unwrap_or_default(),
            unknown,
            offered: targets.iter().map(|t| t.action_id.clone()).collect(),
            workers: targets
                .into_iter()
                .map(|t| (t.action_id, t.worker))
                .collect(),
            unapplied_keys,
        },
        decide,
    })
}

/// One episode per conversation and branch; a request naming no conversation
/// is an episode of its own.
fn episode(headers: &RequestHeaders, context: &ParseContext) -> String {
    let conversation = headers.conversation_id.as_deref().unwrap_or("");
    let parts = [
        "local-routes",
        &context.salt,
        conversation,
        headers.branch_id.as_deref().unwrap_or(""),
        if conversation.is_empty() {
            &context.nonce
        } else {
            ""
        },
    ];
    sha256_hex(parts.join("\0").as_bytes())
}

fn decide_input(
    catalog: &Catalog,
    execution: &ExecutionContext,
    policy: Policy,
    primary: Tier,
    (request_format, request): (&'static str, Value),
    routing_prices: Option<RoutingPrices>,
    episode_id_hash: String,
) -> DecideInput {
    let point = |tier: Tier| OperatingPoint {
        name: tier.name(),
        cost_weight: tier.cost_weight(),
    };
    let mut attribution = execution.history.clone();
    attribution.sort_by_key(|entry| entry.message);
    let mut available: Vec<String> = execution
        .targets
        .iter()
        .map(|t| t.action_id.clone())
        .collect();
    available.sort_unstable();
    let cache = execution::admit_cache(execution);
    DecideInput {
        schema_version: REQUEST_SCHEMA,
        package: PackageRef {
            alias: catalog.alias.clone(),
            package_sha256: catalog.package_sha256.clone(),
        },
        episode_id_hash,
        episode_mode: EpisodeMode::Strict,
        context_epoch: execution.turn.epoch.clone(),
        request_format,
        request,
        attribution,
        selection: Selection {
            available_action_ids: available,
            operating_point: point(primary),
            tier_points: Tier::ALL.into_iter().map(point).collect(),
            held_action_id: execution.hold.clone(),
            last_served_action_id: execution
                .last_served
                .as_ref()
                .map(|served| served.action_id.clone()),
        },
        response_fields: ["price_per_mtok", "thinking"],
        policy,
        event: execution.event.clone(),
        capacity: execution::admit_capacity(execution),
        turn_key: turn_key(&execution.turn.id),
        reconsult: execution.turn.reconsult.clone(),
        cache_context: (!cache.is_empty()).then_some(CacheContext { actions: cache }),
        routing_prices,
    }
}
