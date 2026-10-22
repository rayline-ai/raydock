//! Effective router-config resolution and the default `~/.config/rayline/router.json`.
//!
//! Routing is config-file driven: an explicit `--router-config-path` wins, else the
//! default file at [`default_config_path`] (auto-created on first cloud launch with
//! content that reproduces today's behavior — everything through the hosted cloud
//! router). The on-device router is engaged only when the effective config routes
//! something *away* from the hosted cloud router (see [`config_needs_local_router`]);
//! a pure default stays on today's hosted path with no local process.
//!
//! The generated default is per hosted environment: prod keeps `router.json`, and
//! any other `--env <name>` gets its own `router.<name>.json` pointed at that
//! environment's router (see [`default_config_path_for_env`]).

use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// Default user-editable router config: `~/.config/rayline/router.json`.
pub fn default_config_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("router.json")
}

/// Default router config for a hosted environment. The default environment
/// (prod) keeps the historical `router.json`; every other environment gets
/// `router.<env>.json`, so a file generated for one environment is never reused
/// for another (a dev `rlk-` key must not reach the prod router, and vice
/// versa). Callers resolve the environment first, which rejects names that are
/// not filename-safe (`is_valid_root_env`).
pub fn default_config_path_for_env(home: &Path, env_name: &str) -> PathBuf {
    if env_name == crate::status::resolve_env(None, None) {
        return default_config_path(home);
    }
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join(format!("router.{env_name}.json"))
}

/// Default content: reproduce today's `rayline claude` — route everything to the
/// hosted cloud router at `router_url`. Users edit this to send subagents/main to
/// local or custom endpoints.
pub fn default_config_json(router_url: &str) -> Value {
    json!({
        "endpoints": [
            {
                "id": "rayline-cloud",
                "protocol": "anthropic_messages",
                "base_url": router_url,
                "api_key_env": "RAYLINE_ROUTER_API_KEY",
                "models": ["rayline-router"]
            }
        ],
        "routes": {
            "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
            "default": { "endpoint": "rayline-cloud", "model": "rayline-router" },
            "subagents": {}
        }
    })
}

/// Create the default config for `env_name` if absent, pointing its cloud
/// endpoint at `router_url` (the environment's resolved router). Idempotent;
/// never overwrites user edits.
pub fn ensure_default_config(home: &Path, env_name: &str, router_url: &str) -> io::Result<PathBuf> {
    let path = default_config_path_for_env(home, env_name);
    if !path.exists() {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = serde_json::to_vec_pretty(&default_config_json(router_url))
            .map_err(io::Error::other)?;
        std::fs::write(&path, body)?;
    }
    Ok(path)
}

/// Resolve the effective config path: explicit flag wins, else the default file if
/// it exists. `None` means neither is present (caller stays on today's behavior).
pub fn resolve_config_path(flag: Option<&Path>, home: &Path) -> Option<PathBuf> {
    if let Some(path) = flag {
        return Some(path.to_path_buf());
    }
    let default = default_config_path(home);
    default.exists().then_some(default)
}

/// Whether the config requires the on-device router. True if any route targets an
/// endpoint that is *not* the hosted cloud router (`api.rayline.ai`) — including the
/// bundled `"local"` endpoint, a custom/loopback provider, or a direct-Anthropic
/// endpoint, all of which the hosted plane cannot serve on its own — **or** if any
/// route declares `router: rayline-local` (the on-device LSR is explicitly the
/// router, even when it forwards to the `rayline-cloud` endpoint: it pins the
/// route's `model` on-device instead of letting the hosted RCR pick). A pure
/// everything-to-the-cloud-router config with no `rayline-local` route returns
/// `false` → stay on today's hosted path.
pub fn config_needs_local_router(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    config_value_needs_local_router(&cfg)
}

fn config_value_needs_local_router(cfg: &Value) -> bool {
    if config_value_uses_local_decider(cfg) {
        return true;
    }
    // "Not a hosted RCR endpoint" is what pulls a route off the hosted path, so
    // this must span prod AND dev: reading `api-dev.rayline.ai` as non-cloud
    // engaged the on-device LSR for configs that are entirely cloud.
    let cloud_ids = hosted_rcr_endpoint_ids(cfg);
    route_target_endpoints(cfg)
        .into_iter()
        .any(|endpoint| !cloud_ids.contains(&endpoint))
}

/// Whether any route names the on-device LSR as its router (`router: rayline-local`).
/// Such a route is decided + has its `model` pinned on-device — so the LSR must be
/// engaged even if the route's endpoint is the hosted cloud router.
fn config_value_uses_local_decider(cfg: &Value) -> bool {
    let Some(routes) = cfg.get("routes") else {
        return false;
    };
    let singletons = ["main", "subagent", "default"]
        .into_iter()
        .filter_map(|key| routes.get(key));
    let maps = ["subagents", "model_routes"]
        .into_iter()
        .filter_map(|key| routes.get(key))
        .filter_map(Value::as_object)
        .flat_map(|map| map.values());
    singletons
        .chain(maps)
        .any(|route| route.get("router").and_then(Value::as_str) == Some(ROUTER_RAYLINE_LOCAL))
}

/// Whether any route targets the hosted cloud router — prod (`api.rayline.ai`)
/// **or** dev (`api-dev.rayline.ai`) — so its `rlk-` key should be resolved from
/// `rayline auth login`.
///
/// Both hosts, deliberately. This used to match prod only, which made a
/// dev-targeted config fail three ways at once: no `rlk-` key was injected (the
/// endpoint answered 401), the on-device LSR was engaged for a config that is
/// purely cloud, and may-local could therefore never fire (see
/// [`config_needs_local_router`], which reads "not a cloud endpoint" as "needs
/// the local router"). Every other hosted-RCR test in the workspace already
/// spans both hosts — `endpoint_base_url_is_hosted_rcr` here, and
/// `endpoint_is_hosted_rcr` in `rayline-local-router` — so prod-only was the
/// lone holdout, not a policy.
pub fn config_uses_cloud_router(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    let cloud_ids = hosted_rcr_endpoint_ids(&cfg);
    route_target_endpoints(&cfg)
        .into_iter()
        .any(|endpoint| cloud_ids.contains(&endpoint))
}

/// Whether any route targets a hosted Rayline Cloud Router endpoint — prod
/// (`api.rayline.ai`) **or** dev (`api-dev.rayline.ai`) — so its `rlk-` key
/// should be provisioned from `rayline auth login`.
///
/// The Codex-side spelling of [`config_uses_cloud_router`], kept as its own name
/// because `codex.rs` guards its native rewrite on this question. The two were
/// separate predicates while the cloud-router gate was prod-only; they now ask
/// exactly the same thing, so this delegates rather than drifting again.
pub fn config_routes_to_hosted_rcr(path: &Path) -> bool {
    config_uses_cloud_router(path)
}

/// Endpoint ids whose `base_url` host is a hosted RCR (prod or dev).
fn hosted_rcr_endpoint_ids(cfg: &Value) -> Vec<String> {
    let Some(endpoints) = cfg.get("endpoints").and_then(Value::as_array) else {
        return Vec::new();
    };
    endpoints
        .iter()
        .filter_map(|endpoint| {
            let id = endpoint.get("id").and_then(Value::as_str)?;
            let base_url = endpoint.get("base_url").and_then(Value::as_str);
            endpoint_base_url_is_hosted_rcr(base_url).then(|| id.to_owned())
        })
        .collect()
}

/// Whether any route targets the bundled `"local"` endpoint (needs a configured
/// local model).
pub fn config_uses_local_endpoint(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return false;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return false;
    };
    route_target_endpoints(&cfg)
        .into_iter()
        .any(|endpoint| endpoint == "local")
}

/// `router` value selecting the hosted cloud decider (the default when absent).
pub const ROUTER_RAYLINE_CLOUD: &str = "rayline-cloud";
/// `router` value selecting the on-device LSR decider (Rl-Rl): the LSR routes the
/// class per the static JSON and pins its `model`, rather than the hosted RCR.
pub const ROUTER_RAYLINE_LOCAL: &str = "rayline-local";

/// The virtual model that asks the hosted RCR to decide (its balanced tiering).
/// A may-local `rayline-cloud` route may declare only `local_models` and omit
/// `model`; the local router rejects a non-local route with an empty `model`, so
/// materialization fills this sentinel before startup.
const RAYLINE_ROUTER_MODEL: &str = "rayline-router";

/// The local model the hosted cloud router may redirect a `rayline` class to
/// ("may-local"), resolved from the config. Returns the advertised model id and
/// the base URL of the local endpoint that serves it (the redirect target the
/// proxy fronts via a custom-mode adapter). `None` when no route turns may-local
/// on (no `router: rayline-cloud` route carries a non-empty `local_models`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MayLocal {
    pub model: String,
    pub upstream_url: String,
}

/// Resolve [`MayLocal`] from a config file. See [`MayLocal`].
pub fn config_may_local(path: &Path) -> Option<MayLocal> {
    let raw = std::fs::read(path).ok()?;
    let cfg: Value = serde_json::from_slice(&raw).ok()?;
    config_value_may_local(&cfg)
}

/// The hosted-RCR ROOT URL of the config's cloud endpoint (prod `api.rayline.ai`
/// or dev `api-dev.rayline.ai`), if any. This is the RCR that issues may-local's
/// `usage_doc_id` in its 307, so it is where the on-device adapter must post its
/// `/v1/usage/update` to close the placeholder row. Reads the config directly
/// (not env resolution) so it reflects exactly which RCR the config points at.
///
/// The returned URL is normalized to the server ROOT — a trailing `/v1` is
/// stripped — because the adapter appends `/v1/usage/update`. A hosted RCR
/// endpoint may legitimately declare its base_url as `…/api.rayline.ai/v1`
/// (valid for `openai_responses`, and the Codex-native materializer normalizes
/// hosted endpoints to that shape via `ensure_rcr_base_url_has_v1`), which would
/// otherwise produce `…/v1/v1/usage/update` and leave the row unclosed.
pub fn config_hosted_rcr_base_url(path: &Path) -> Option<String> {
    let raw = std::fs::read(path).ok()?;
    let cfg: Value = serde_json::from_slice(&raw).ok()?;
    config_value_hosted_rcr_base_url(&cfg)
}

fn config_value_hosted_rcr_base_url(cfg: &Value) -> Option<String> {
    // Use the endpoint that the may-local route actually targets — a config may
    // declare multiple hosted RCRs (e.g. prod + dev), and the callback must reach
    // the one that received the turn (and thus owns the usage_doc_id), not merely
    // the first hosted endpoint declared.
    let endpoint_id = may_local_route_endpoint_id(cfg);
    let endpoints = cfg.get("endpoints").and_then(Value::as_array)?;
    let base_url = if let Some(id) = endpoint_id.as_deref() {
        endpoints
            .iter()
            .find(|e| e.get("id").and_then(Value::as_str) == Some(id))
            .and_then(|e| e.get("base_url").and_then(Value::as_str))
            .filter(|b| endpoint_base_url_is_hosted_rcr(Some(b)))
    } else {
        None
    }
    // Fallback: no resolvable may-local endpoint id (e.g. an inherited cloud
    // default) — use the first hosted RCR endpoint declared.
    .or_else(|| {
        endpoints
            .iter()
            .find(|e| endpoint_base_url_is_hosted_rcr(e.get("base_url").and_then(Value::as_str)))
            .and_then(|e| e.get("base_url").and_then(Value::as_str))
    })?;
    Some(crate::local_model::normalize_base_url(base_url))
}

/// The `endpoint` id of the route that turns may-local on (a `router: rayline-cloud`
/// route carrying a non-empty `local_models`). This is the RCR endpoint the
/// may-local turn forwards to and that owns the `usage_doc_id`. `None` when the
/// route names no explicit endpoint (it inherits the cloud default).
fn may_local_route_endpoint_id(cfg: &Value) -> Option<String> {
    let routes = cfg.get("routes")?;
    let singletons = ["main", "subagent", "default"]
        .into_iter()
        .filter_map(|key| routes.get(key));
    let maps = ["subagents", "model_routes"]
        .into_iter()
        .filter_map(|key| routes.get(key))
        .filter_map(Value::as_object)
        .flat_map(|map| map.values());
    singletons
        .chain(maps)
        .filter(|route| route_advertised_local_model(route).is_some())
        .find_map(|route| {
            route
                .get("endpoint")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
}

fn config_value_may_local(cfg: &Value) -> Option<MayLocal> {
    let model = config_advertised_local_model(cfg)?;
    let raw_base = endpoint_base_url_for_model(cfg, &model)?;
    // The custom-mode adapter appends `/v1/messages` to the upstream base
    // (`rayline-adapter` `format!("{}/v1/messages", target)`), so the upstream must
    // be the server *root*. Normalize exactly like `rayline local custom` does —
    // strip a trailing `/v1` — so an `openai_chat` endpoint's `…/v1` base_url does
    // not become `…/v1/v1/messages`.
    let upstream_url = crate::local_model::normalize_base_url(&raw_base);
    Some(MayLocal {
        model,
        upstream_url,
    })
}

/// First local model advertised by a may-local route: a route whose `router` is
/// `rayline-cloud` (or absent → the cloud default) carrying a non-empty
/// `local_models`. Routes are scanned `main`, `subagent`, `default`, then the
/// `subagents`/`model_routes` maps. `rayline-local` routes are skipped (may-local
/// is `N/A` there).
fn config_advertised_local_model(cfg: &Value) -> Option<String> {
    let routes = cfg.get("routes")?;
    let singletons = ["main", "subagent", "default"]
        .into_iter()
        .filter_map(|key| routes.get(key));
    let maps = ["subagents", "model_routes"]
        .into_iter()
        .filter_map(|key| routes.get(key))
        .filter_map(Value::as_object)
        .flat_map(|map| map.values());
    singletons
        .chain(maps)
        .filter_map(route_advertised_local_model)
        .next()
}

/// Fill a missing/empty `model` on any non-`rayline-local` route that declares
/// `local_models`, using the `rayline-router` sentinel. `config_may_local` accepts
/// a may-local `rayline-cloud` route that carries only `local_models` (no `model`),
/// but the local router rejects a non-local route with an empty `model` at startup
/// (`route to endpoint … must include a model`). Without this, such an accepted
/// may-local config fails at daemon start instead of enabling may-local. Scans the
/// same route set as `config_advertised_local_model`. Returns whether it changed.
fn ensure_may_local_route_models(cfg: &mut Value) -> bool {
    let Some(routes) = cfg.get_mut("routes").and_then(Value::as_object_mut) else {
        return false;
    };
    let mut changed = false;
    let mut fill = |route: &mut Value| {
        // Only routes that advertise may-local (non-`rayline-local`, has
        // `local_models`) and lack a usable `model`.
        if route.get("router").and_then(Value::as_str) == Some(ROUTER_RAYLINE_LOCAL) {
            return;
        }
        let has_local_models = route
            .get("local_models")
            .and_then(Value::as_array)
            .is_some_and(|a| a.iter().any(|m| m.as_str().is_some_and(|s| !s.is_empty())));
        let missing_model = route
            .get("model")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if has_local_models && missing_model {
            if let Some(obj) = route.as_object_mut() {
                obj.insert(
                    "model".to_owned(),
                    Value::String(RAYLINE_ROUTER_MODEL.to_owned()),
                );
                changed = true;
            }
        }
    };
    for key in ["main", "subagent", "default"] {
        if let Some(route) = routes.get_mut(key) {
            fill(route);
        }
    }
    for key in ["subagents", "model_routes"] {
        if let Some(map) = routes.get_mut(key).and_then(Value::as_object_mut) {
            for route in map.values_mut() {
                fill(route);
            }
        }
    }
    changed
}

/// A single route's advertised local model, if it has may-local on.
fn route_advertised_local_model(route: &Value) -> Option<String> {
    // `rayline-local` routes never advertise may-local (N/A).
    if route.get("router").and_then(Value::as_str) == Some(ROUTER_RAYLINE_LOCAL) {
        return None;
    }
    route
        .get("local_models")
        .and_then(Value::as_array)?
        .iter()
        .find_map(Value::as_str)
        .map(ToOwned::to_owned)
}

/// Base URL of the (non-cloud) endpoint that lists `model` in its `models`. This
/// is the upstream the proxy's may-local redirect is fronted onto. The hosted
/// cloud router is excluded — a local model is served by a local endpoint.
fn endpoint_base_url_for_model(cfg: &Value, model: &str) -> Option<String> {
    // Spans prod AND dev: with prod-only matching, a dev-targeted config could
    // resolve `api-dev.rayline.ai` as the *local* upstream for a may-local
    // redirect — pointing the redirect back at the hosted RCR it came from.
    let cloud_ids = hosted_rcr_endpoint_ids(cfg);
    cfg.get("endpoints")
        .and_then(Value::as_array)?
        .iter()
        .find(|endpoint| {
            let id = endpoint.get("id").and_then(Value::as_str);
            let is_cloud = id.is_some_and(|id| cloud_ids.iter().any(|cloud| cloud == id));
            !is_cloud
                && endpoint
                    .get("models")
                    .and_then(Value::as_array)
                    .is_some_and(|models| models.iter().any(|m| m.as_str() == Some(model)))
        })
        .and_then(|endpoint| endpoint.get("base_url").and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

/// Reserved `routes.main.endpoint` value meaning "do not route the main agent —
/// let it pass through to the caller's own Claude subscription/credential".
/// `RouterConfig` cannot express a credential-passthrough endpoint, so the CLI
/// reads this sentinel to pick the proxy scope (selective-subagents) instead of
/// emitting it into the on-device router config.
pub const SUBSCRIPTION_MAIN: &str = "subscription";

/// Whether the main agent should pass through to the caller's own subscription
/// rather than be routed by the on-device router. True when `routes.main` is
/// absent, or its `endpoint` is the reserved [`SUBSCRIPTION_MAIN`] sentinel.
/// Drives `RoutingMode::ProxySubagents` (main passthrough) vs `Proxy` (route all).
pub fn config_main_is_passthrough(path: &Path) -> bool {
    let Ok(raw) = std::fs::read(path) else {
        return true;
    };
    let Ok(cfg) = serde_json::from_slice::<Value>(&raw) else {
        return true;
    };
    config_value_main_is_passthrough(&cfg)
}

fn config_value_main_is_passthrough(cfg: &Value) -> bool {
    match cfg.get("routes").and_then(|routes| routes.get("main")) {
        None => true,
        Some(main) => route_endpoint(main).as_deref() == Some(SUBSCRIPTION_MAIN),
    }
}

/// Produce the config the on-device router should load. When `routes.main` is the
/// passthrough sentinel (or absent) — main stays on the caller's subscription,
/// handled by the proxy's selective-subagents scope — strip `routes.main` so the
/// local router (which has no `subscription` endpoint) doesn't reject it during
/// normalization. Otherwise the original file is used verbatim.
///
/// The derived file is written to `~/.rayline/rld/config-routes.json`.
pub fn materialize_for_local_router(path: &Path, home: &Path) -> io::Result<PathBuf> {
    if !config_main_is_passthrough(path) {
        return Ok(path.to_path_buf());
    }
    let raw = std::fs::read(path)?;
    let mut cfg: Value = serde_json::from_slice(&raw).map_err(io::Error::other)?;
    if let Some(routes) = cfg.get_mut("routes").and_then(Value::as_object_mut) {
        routes.remove("main");
    }
    let out = home.join(".rayline").join("rld").join("config-routes.json");
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&cfg).map_err(io::Error::other)?;
    std::fs::write(&out, body)?;
    Ok(out)
}

/// Codex has no transparent MITM passthrough layer: Codex is explicitly pointed
/// at Rayline as a custom Responses provider. In subscription mode, materialize
/// the shared `subscription` sentinel into a real client-bearer endpoint that
/// forwards Codex's ChatGPT auth headers to the ChatGPT Codex backend.
pub fn materialize_codex_subscription_for_local_router(
    path: &Path,
    home: &Path,
) -> io::Result<PathBuf> {
    let raw = std::fs::read(path)?;
    let mut cfg: Value = serde_json::from_slice(&raw).map_err(io::Error::other)?;
    let mut changed = ensure_codex_subscription_endpoint(&mut cfg);
    changed |= ensure_codex_subscription_main_route(&mut cfg);
    changed |= rewrite_subscription_routes_for_codex(&mut cfg);
    // A*-subagent-cloud case: a `rayline-cloud` subagent endpoint pointed at the
    // hosted RCR must also forward native Responses + carry `x-rayline-client`.
    changed |= rewrite_rayline_cloud_for_codex_native(&mut cfg);
    // …and if that may-local subagent route declares only `local_models`, fill the
    // RCR sentinel model so the local router doesn't reject it at startup.
    changed |= ensure_may_local_route_models(&mut cfg);
    // After the rewrite `routes.main` is the concrete codex-subscription endpoint,
    // so pinning the sentinel `--model` to it points Codex's MAIN turns there. The
    // local router skips this model_route on subagent turns (so `routes.subagent`
    // governs them) — see `select_route`.
    changed |= ensure_codex_config_model_routes(&mut cfg);
    if !changed {
        return Ok(path.to_path_buf());
    }
    let out = home
        .join(".rayline")
        .join("rld")
        .join("codex-config-routes.json");
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&cfg).map_err(io::Error::other)?;
    std::fs::write(&out, body)?;
    Ok(out)
}

/// Non-subscription Codex `--config` (e.g. a local/ollama or provider main): like
/// [`materialize_for_local_router`], plus pins Codex's sentinel `--model`
/// (`rayline-local`/`rayline-codex`) to `routes.main` via [`ensure_codex_config_model_routes`]
/// so MAIN turns reach the configured main endpoint. The local router skips this
/// model_route on subagent turns, so `routes.subagent`/`routes.subagents` govern
/// them (see `select_route`).
pub fn materialize_codex_config_for_local_router(path: &Path, home: &Path) -> io::Result<PathBuf> {
    let raw = std::fs::read(path)?;
    let mut cfg: Value = serde_json::from_slice(&raw).map_err(io::Error::other)?;
    let mut changed = ensure_codex_config_model_routes(&mut cfg);
    // A may-local `rayline-cloud` route may declare only `local_models` and omit
    // `model` (accepted by config_may_local); fill the RCR sentinel so the local
    // router's non-empty-model check doesn't reject it at startup.
    changed |= ensure_may_local_route_models(&mut cfg);
    // Codex `R*` main → hosted RCR: forward native Responses (not the lossy
    // Anthropic bridge) and stamp `x-rayline-client: codex`. Host-guarded.
    changed |= rewrite_rayline_cloud_for_codex_native(&mut cfg);
    // Same passthrough handling as materialize_for_local_router: the local router
    // has no `subscription` endpoint, so strip a passthrough `main` (that combo is
    // for `--auth subscription`, which uses a different materialization).
    if config_value_main_is_passthrough(&cfg) {
        if let Some(routes) = cfg.get_mut("routes").and_then(Value::as_object_mut) {
            changed |= routes.remove("main").is_some();
        }
    }
    if !changed {
        return Ok(path.to_path_buf());
    }
    let out = home
        .join(".rayline")
        .join("rld")
        .join("codex-config-routes.json");
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let body = serde_json::to_vec_pretty(&cfg).map_err(io::Error::other)?;
    std::fs::write(&out, body)?;
    Ok(out)
}

/// Pin Codex's sentinel `--model` to `routes.main`. Codex sends `rayline-local`
/// (default) or `rayline-codex` on every turn; cloning the main route onto both
/// sentinels routes MAIN turns to the configured main endpoint. The local router
/// skips these sentinel `model_routes` on SUBAGENT turns so `routes.subagent` /
/// `routes.subagents` govern them (see `select_route`) — that is what makes a
/// main≠subagent Codex split possible. Skips the `subscription` passthrough
/// sentinel (no concrete endpoint here — that's the `--auth subscription` path)
/// and leaves existing `model_routes` entries untouched.
fn ensure_codex_config_model_routes(cfg: &mut Value) -> bool {
    let Some(main_route) = cfg
        .get("routes")
        .and_then(|routes| routes.get("main"))
        .cloned()
    else {
        return false;
    };
    if route_endpoint(&main_route).as_deref() == Some(SUBSCRIPTION_MAIN) {
        return false;
    }
    let routes = cfg
        .as_object_mut()
        .map(|object| object.entry("routes").or_insert_with(|| json!({})))
        .and_then(Value::as_object_mut);
    let Some(routes) = routes else {
        return false;
    };
    let model_routes = routes.entry("model_routes").or_insert_with(|| json!({}));
    let Some(model_routes) = model_routes.as_object_mut() else {
        return false;
    };
    let mut changed = false;
    for model in ["rayline-local", "rayline-codex"] {
        if model_routes.contains_key(model) {
            continue;
        }
        model_routes.insert(model.to_owned(), main_route.clone());
        changed = true;
    }
    changed
}

/// `x-rayline-client` value the edge stamps on Codex requests forwarded to the
/// hosted RCR, so the cloud router selects a GPT (vs Claude) model. Path-based
/// class detection is the RCR-side fallback; this header is the intended
/// contract.
const RAYLINE_CLIENT_HEADER: &str = "x-rayline-client";
const RAYLINE_CLIENT_CODEX: &str = "codex";

/// Rewrite any endpoint pointing at the hosted RCR so a Codex `/v1/responses`
/// request forwards **natively** (`openai_responses`) instead of being
/// down-translated to Anthropic. Concretely, on the matched endpoint(s):
/// - flip `protocol` `anthropic_messages` → `openai_responses` (so
///   `handle_responses` dispatches to `forward_openai_responses_endpoint`);
/// - set `auth: bearer` so the `rlk-` router key rides on `Authorization:
///   Bearer` — the header the RCR's `getUser`/`extractAuthHeader` accepts and
///   the style `forward_openai_passthrough_endpoint` applies;
/// - inject `x-rayline-client: codex` into the endpoint `headers` map so
///   `apply_endpoint_headers` forwards it verbatim.
///
/// **Host-guarded:** only endpoints whose `base_url` host is the hosted RCR
/// (`api.rayline.ai` / `api-dev.rayline.ai`) are flipped. A user's own custom
/// `anthropic_messages` endpoint is left untouched.
fn rewrite_rayline_cloud_for_codex_native(cfg: &mut Value) -> bool {
    let Some(endpoints) = cfg.get_mut("endpoints").and_then(Value::as_array_mut) else {
        return false;
    };
    let mut changed = false;
    for endpoint in endpoints.iter_mut() {
        let Some(object) = endpoint.as_object_mut() else {
            continue;
        };
        if !endpoint_base_url_is_hosted_rcr(object.get("base_url").and_then(Value::as_str)) {
            continue;
        }
        changed |= normalize_rcr_endpoint_for_codex_native(object);
    }
    changed
}

/// Normalize a hosted-RCR endpoint for native Codex forwarding. Applied
/// uniformly regardless of the endpoint's declared protocol (a fresh
/// `anthropic_messages` Rc-Rc endpoint, or a partially-migrated `openai_responses`
/// one), so the result is always: `openai_responses` + `auth: bearer` +
/// `/v1` base_url + `x-rayline-client: codex`. Each step is idempotent; returns
/// whether anything changed.
fn normalize_rcr_endpoint_for_codex_native(endpoint: &mut serde_json::Map<String, Value>) -> bool {
    let mut changed = false;
    if endpoint.get("protocol").and_then(Value::as_str) != Some("openai_responses") {
        endpoint.insert("protocol".to_owned(), json!("openai_responses"));
        changed = true;
    }
    // Bearer so the rlk- key rides on Authorization (the style the native
    // passthrough applies); never leave a stale `auth: api_key` (→ x-api-key).
    if endpoint.get("auth").and_then(Value::as_str) != Some("bearer") {
        endpoint.insert("auth".to_owned(), json!("bearer"));
        changed = true;
    }
    changed |= ensure_rcr_base_url_has_v1(endpoint);
    changed |= inject_rayline_client_codex_header(endpoint);
    changed
}

/// Ensure the RCR endpoint's `base_url` ends in `/v1`, so the native passthrough
/// (which strips the inbound `/v1/` prefix before appending `responses`) reaches
/// `…/v1/responses` on the hosted router rather than `…/responses`. The Rc-Rc-shape
/// base_url is the bare host root (`https://api.rayline.ai`); add the `/v1`.
/// Idempotent. Returns whether the config changed.
fn ensure_rcr_base_url_has_v1(endpoint: &mut serde_json::Map<String, Value>) -> bool {
    let Some(base_url) = endpoint.get("base_url").and_then(Value::as_str) else {
        return false;
    };
    let trimmed = base_url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        return false;
    }
    endpoint.insert("base_url".to_owned(), json!(format!("{trimmed}/v1")));
    true
}

/// Whether a `base_url` host is the hosted Rayline Cloud Router (prod or dev).
pub(crate) fn endpoint_base_url_is_hosted_rcr(base_url: Option<&str>) -> bool {
    matches!(
        base_url.and_then(host_of).as_deref(),
        Some("api.rayline.ai") | Some("api-dev.rayline.ai")
    )
}

/// Add `x-rayline-client: codex` to an endpoint's `headers` map (creating it if
/// absent). Idempotent. Returns whether the config changed.
fn inject_rayline_client_codex_header(endpoint: &mut serde_json::Map<String, Value>) -> bool {
    let headers = endpoint.entry("headers").or_insert_with(|| json!({}));
    let Some(headers) = headers.as_object_mut() else {
        return false;
    };
    if headers.get(RAYLINE_CLIENT_HEADER).and_then(Value::as_str) == Some(RAYLINE_CLIENT_CODEX) {
        return false;
    }
    headers.insert(
        RAYLINE_CLIENT_HEADER.to_owned(),
        json!(RAYLINE_CLIENT_CODEX),
    );
    true
}

fn ensure_codex_subscription_endpoint(cfg: &mut Value) -> bool {
    let endpoint_id = crate::codex::CODEX_SUBSCRIPTION_ENDPOINT_ID;
    let endpoints = cfg
        .as_object_mut()
        .map(|object| {
            object
                .entry("endpoints")
                .or_insert_with(|| Value::Array(Vec::new()))
        })
        .and_then(Value::as_array_mut);
    let Some(endpoints) = endpoints else {
        return false;
    };
    if endpoints
        .iter()
        .any(|endpoint| endpoint.get("id").and_then(Value::as_str) == Some(endpoint_id))
    {
        return false;
    }
    endpoints.push(json!({
        "id": endpoint_id,
        "protocol": "openai_responses",
        "base_url": crate::codex::CODEX_SUBSCRIPTION_BASE_URL,
        "auth": "client_bearer",
        "models": crate::codex::CODEX_SUBSCRIPTION_MODELS
    }));
    true
}

fn ensure_codex_subscription_main_route(cfg: &mut Value) -> bool {
    let routes = cfg
        .as_object_mut()
        .map(|object| object.entry("routes").or_insert_with(|| json!({})))
        .and_then(Value::as_object_mut);
    let Some(routes) = routes else {
        return false;
    };
    if routes.contains_key("main") {
        return false;
    }
    routes.insert(
        "main".to_owned(),
        json!({
            "endpoint": crate::codex::CODEX_SUBSCRIPTION_ENDPOINT_ID,
            "model": crate::codex::CODEX_SUBSCRIPTION_DEFAULT_MODEL
        }),
    );
    true
}

fn rewrite_subscription_routes_for_codex(cfg: &mut Value) -> bool {
    let Some(routes) = cfg.get_mut("routes") else {
        return false;
    };
    let mut changed = false;
    for key in ["main", "default", "subagent"] {
        if let Some(route) = routes.get_mut(key) {
            changed |= rewrite_subscription_route_for_codex(route);
        }
    }
    for key in ["subagents", "model_routes"] {
        if let Some(map) = routes.get_mut(key).and_then(Value::as_object_mut) {
            for route in map.values_mut() {
                changed |= rewrite_subscription_route_for_codex(route);
            }
        }
    }
    changed
}

fn rewrite_subscription_route_for_codex(route: &mut Value) -> bool {
    let Some(object) = route.as_object_mut() else {
        return false;
    };
    if object.get("endpoint").and_then(Value::as_str) != Some(SUBSCRIPTION_MAIN) {
        return false;
    }
    object.insert(
        "endpoint".to_owned(),
        Value::String(crate::codex::CODEX_SUBSCRIPTION_ENDPOINT_ID.to_owned()),
    );
    if object
        .get("model")
        .and_then(Value::as_str)
        .is_none_or(|model| model.trim().is_empty())
    {
        object.insert(
            "model".to_owned(),
            Value::String(crate::codex::CODEX_SUBSCRIPTION_DEFAULT_MODEL.to_owned()),
        );
    }
    true
}

/// Every endpoint id referenced by any route (`main`, `default`, `subagent`,
/// `subagents.*`, `model_routes.*`).
fn route_target_endpoints(cfg: &Value) -> Vec<String> {
    let Some(routes) = cfg.get("routes") else {
        return Vec::new();
    };
    let mut endpoints = Vec::new();
    for key in ["main", "default", "subagent"] {
        if let Some(endpoint) = routes.get(key).and_then(route_endpoint) {
            endpoints.push(endpoint);
        }
    }
    for key in ["subagents", "model_routes"] {
        if let Some(map) = routes.get(key).and_then(Value::as_object) {
            endpoints.extend(map.values().filter_map(route_endpoint));
        }
    }
    // The `subscription` sentinel is a proxy-passthrough marker, not a real
    // endpoint — drop it so it never counts toward local/cloud endpoint detection.
    endpoints.retain(|endpoint| endpoint != SUBSCRIPTION_MAIN);
    endpoints
}

fn route_endpoint(route: &Value) -> Option<String> {
    route
        .get("endpoint")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn host_of(url: &str) -> Option<String> {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(ToOwned::to_owned))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_does_not_need_local_router() {
        assert!(!config_value_needs_local_router(&default_config_json(
            crate::ROUTER_PROD_URL
        )));
    }

    #[test]
    fn hosted_rcr_base_url_extracted_from_cloud_endpoint() {
        // Dev RCR endpoint → its base_url is the usage-callback target.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": "https://api-dev.rayline.ai", "models": ["rayline-router"] },
                { "id": "local-bundled", "protocol": "openai_responses",
                  "base_url": "http://127.0.0.1:8899", "models": ["m"] }
            ],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });
        assert_eq!(
            config_value_hosted_rcr_base_url(&cfg).as_deref(),
            Some("https://api-dev.rayline.ai")
        );
        // A hosted endpoint declared with a trailing /v1 (valid for
        // openai_responses; the Codex-native materializer normalizes to this)
        // must be stripped to the ROOT so the adapter's {root}/v1/usage/update
        // does not become …/v1/v1/usage/update.
        let with_v1 = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "openai_responses",
                  "base_url": "https://api.rayline.ai/v1", "models": ["rayline-router"] }
            ],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });
        assert_eq!(
            config_value_hosted_rcr_base_url(&with_v1).as_deref(),
            Some("https://api.rayline.ai")
        );
        // No hosted-RCR endpoint (all local/custom) → None.
        let local_only = json!({
            "endpoints": [
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["q"] }
            ],
            "routes": { "main": { "endpoint": "ollama", "model": "q" } }
        });
        assert_eq!(config_value_hosted_rcr_base_url(&local_only), None);
    }

    #[test]
    fn hosted_rcr_base_url_uses_the_may_local_routes_endpoint() {
        // Two hosted RCRs declared (prod + dev); the may-local subagent route
        // targets the SECOND (dev). The callback must go to dev, not the first
        // declared (prod).
        let cfg = json!({
            "endpoints": [
                { "id": "rcr-prod", "protocol": "anthropic_messages",
                  "base_url": "https://api.rayline.ai", "models": ["rayline-router"] },
                { "id": "rcr-dev", "protocol": "anthropic_messages",
                  "base_url": "https://api-dev.rayline.ai", "models": ["rayline-router"] },
                { "id": "local-bundled", "protocol": "openai_responses",
                  "base_url": "http://127.0.0.1:8899", "models": ["m"] }
            ],
            "routes": {
                "main": { "endpoint": "rcr-prod", "model": "rayline-router" },
                "subagent": { "endpoint": "rcr-dev", "model": "rayline-router",
                              "router": "rayline-cloud", "local_models": ["m"] }
            }
        });
        assert_eq!(
            config_value_hosted_rcr_base_url(&cfg).as_deref(),
            Some("https://api-dev.rayline.ai")
        );
    }

    #[test]
    fn subagent_to_local_needs_local_router() {
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                "subagents": { "Explore": { "endpoint": "local" } }
            }
        });
        assert!(config_value_needs_local_router(&cfg));
    }

    #[test]
    fn subagent_to_loopback_endpoint_needs_local_router() {
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] },
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                "subagents": { "Explore": { "endpoint": "ollama", "model": "qwen2.5-coder:7b" } }
            }
        });
        assert!(config_value_needs_local_router(&cfg));
    }

    #[test]
    fn direct_anthropic_main_needs_local_router() {
        // Routing main at a non-cloud-router endpoint also requires the local router.
        let cfg = json!({
            "endpoints": [
                { "id": "anthropic", "protocol": "anthropic_messages",
                  "base_url": "https://api.anthropic.com", "models": ["claude-sonnet-4-6"] }
            ],
            "routes": { "main": { "endpoint": "anthropic", "model": "claude-sonnet-4-6" } }
        });
        assert!(config_value_needs_local_router(&cfg));
    }

    fn tmp_home() -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("rl-router-config-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn ensure_default_config_creates_non_engaging_file() {
        let home = tmp_home();
        let path = ensure_default_config(&home, "prod", crate::ROUTER_PROD_URL).unwrap();
        assert!(path.exists());
        assert_eq!(path, default_config_path(&home));
        // The default file reproduces today's behavior → no local router.
        assert!(!config_needs_local_router(&path));
        assert!(config_uses_cloud_router(&path));
        assert!(!config_uses_local_endpoint(&path));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_config_path_prefers_flag_then_default() {
        let home = tmp_home();
        // Neither present.
        assert!(resolve_config_path(None, &home).is_none());
        // Flag wins even when absent on disk.
        let flag = PathBuf::from("/tmp/explicit.json");
        assert_eq!(resolve_config_path(Some(&flag), &home), Some(flag));
        // Default file is picked up once it exists.
        let default = ensure_default_config(&home, "prod", crate::ROUTER_PROD_URL).unwrap();
        assert_eq!(resolve_config_path(None, &home), Some(default));
        let _ = std::fs::remove_dir_all(&home);
    }

    fn examples_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/routing-modes")
    }

    /// Every shipped example config must parse, and the CLI's per-mode derivations
    /// (passthrough scope, local-plane engagement, cloud-key need) must match the
    /// mode's intent. This is the config↔mode cross-check.
    #[test]
    fn example_mode_configs_derive_expected_routing() {
        // (file, main_is_passthrough, needs_local_router, uses_cloud_router)
        let cases = [
            ("Rc-Rc.json", false, false, true),
            // Rcl-Rcl: may-local routes stay on the cloud router (the `ollama` endpoint
            // is a redirect target, not a route) → no on-device router engaged.
            ("Rcl-Rcl.json", false, false, true),
            // Rl-Rl: router rayline-local engages the on-device router even though
            // both routes target rayline-cloud (it pins the model on-device).
            ("Rl-Rl.json", false, true, true),
            ("Rc-L.json", false, true, true),
            ("Rc-L-per-type.json", false, true, true),
            ("Rc-K.json", false, true, true),
            // Rl-K/Rl-L/S-Rl/L-Rl: router rayline-local on the rayline class → on-device
            // routing; the other class is anthropic (API key) / ollama / subscription.
            ("Rl-K.json", false, true, true),
            ("Rl-L.json", false, true, true),
            ("S-Rl.json", true, true, true),
            ("L-Rl.json", false, true, true),
            ("S-Rc.json", true, false, true),
            // S-Rc-per-type: subscription main (passthrough) + only Explore routed to
            // the cloud router; no local endpoint. Unlisted subagents pass through at
            // the proxy (allowlist), so no default subagent and no LSR engagement.
            ("S-Rc-per-type.json", true, false, true),
            ("S-L.json", true, true, false),
            // S-L-per-type: subscription main (passthrough) + only Explore routed to a
            // local endpoint → engages the LSR; no cloud endpoint. Unlisted subagents
            // pass through at the proxy (not a route), so no default subagent needed.
            ("S-L-per-type.json", true, true, false),
            ("L-Rc.json", false, true, true),
            ("L-L.json", false, true, false),
            ("L-K.json", false, true, false),
            // K-K: one keyed endpoint (openrouter) serving two models (main + subagent) →
            // main is routed (not passthrough); the on-device router forwards to the
            // non-cloud endpoint; no hosted cloud-router key.
            ("K-K.json", false, true, false),
            // K: the minimal single-model form of K-K — one keyed endpoint, `routes.main`
            // only (subagents inherit). Same derivation: routed main, non-cloud endpoint,
            // no cloud key.
            ("K.json", false, true, false),
        ];
        for (file, passthrough, needs_local, uses_cloud) in cases {
            let path = examples_dir().join(file);
            assert!(path.exists(), "missing example config {file}");
            // Must be valid JSON the router can read.
            let raw = std::fs::read(&path).unwrap();
            serde_json::from_slice::<Value>(&raw).unwrap_or_else(|e| panic!("{file}: {e}"));
            assert_eq!(
                config_main_is_passthrough(&path),
                passthrough,
                "{file}: main passthrough"
            );
            assert_eq!(
                config_needs_local_router(&path),
                needs_local,
                "{file}: needs local router"
            );
            assert_eq!(
                config_uses_cloud_router(&path),
                uses_cloud,
                "{file}: uses cloud router"
            );
            // None of the examples use the bundled `"local"` endpoint (they name
            // ollama explicitly), so none require an on-device model.
            assert!(
                !config_uses_local_endpoint(&path),
                "{file}: should not use bundled local endpoint"
            );
        }
    }

    #[test]
    fn cl_modes_may_local_wiring() {
        // For each `--local-model=on` (*CL) config, report what the CLI derives:
        //   resolves    = config_may_local resolves a model+upstream
        //   needs_local = a route targets a non-cloud endpoint (engages the LSR)
        //   fires       = the CLI actually wires may-local advertisement, which is
        //                 gated on the cloud-only path (!needs_local) in claude.rs.
        // (file, expect_resolves, expect_needs_local, expect_fires)
        let cases = [
            ("Rcl-Rcl.json", true, false, true),
            ("S-Rcl.json", true, false, true),
            ("Rcl-K.json", true, true, false),
            ("Rcl-L.json", true, true, false),
            ("L-Rcl.json", true, true, false),
        ];
        for (file, resolves, needs_local, fires) in cases {
            let path = examples_dir().join(file);
            assert!(path.exists(), "missing {file}");
            assert_eq!(
                config_may_local(&path).is_some(),
                resolves,
                "{file}: resolves"
            );
            assert_eq!(
                config_needs_local_router(&path),
                needs_local,
                "{file}: needs_local"
            );
            let actually_fires =
                !config_needs_local_router(&path) && config_may_local(&path).is_some();
            assert_eq!(actually_fires, fires, "{file}: may-local wired by CLI");
        }
    }

    #[test]
    fn rrl_example_engages_local_router() {
        // Rl-Rl: `router: rayline-local` makes the on-device LSR the router even though
        // the routes target the hosted `rayline-cloud` endpoint — so the LSR must be
        // engaged (it pins the route's `model` on-device instead of letting the RCR
        // pick). It is not may-local, not a passthrough main, and uses the cloud key.
        let path = examples_dir().join("Rl-Rl.json");
        assert!(path.exists(), "missing example config Rl-Rl.json");
        serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap();
        assert!(
            config_needs_local_router(&path),
            "Rl-Rl: router rayline-local must engage the on-device router"
        );
        assert!(!config_main_is_passthrough(&path), "Rl-Rl: main is routed");
        assert!(
            config_uses_cloud_router(&path),
            "Rl-Rl: forwards to the cloud key"
        );
        assert!(
            !config_uses_local_endpoint(&path),
            "Rl-Rl: no bundled local model"
        );
        assert_eq!(config_may_local(&path), None, "Rl-Rl: not may-local");
    }

    #[test]
    fn rayline_local_router_engages_even_when_all_cloud() {
        // An all-cloud config normally stays on the hosted path...
        let all_cloud = json!({
            "endpoints": [{ "id": "rayline-cloud", "protocol": "anthropic_messages",
                "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });
        assert!(!config_value_needs_local_router(&all_cloud));
        // ...but `router: rayline-local` forces on-device routing.
        let mut local_decider = all_cloud.clone();
        local_decider["routes"]["main"]["router"] = json!("rayline-local");
        assert!(config_value_needs_local_router(&local_decider));
        assert!(config_value_uses_local_decider(&local_decider));
    }

    #[test]
    fn rrcl_example_resolves_may_local() {
        // The shipped Rcl-Rcl config advertises a local model fronted by the `ollama`
        // endpoint, and stays cloud-only for routing (no on-device router engaged).
        let path = examples_dir().join("Rcl-Rcl.json");
        assert!(path.exists(), "missing example config Rcl-Rcl.json");
        assert_eq!(
            config_may_local(&path),
            Some(MayLocal {
                model: "qwen2.5-coder:7b".to_owned(),
                upstream_url: "http://127.0.0.1:11434".to_owned(),
            })
        );
        // Rc-Rc (no may-local) must not resolve one.
        assert_eq!(config_may_local(&examples_dir().join("Rc-Rc.json")), None);
    }

    #[test]
    fn materialize_strips_subscription_main_for_local_router() {
        let home = tmp_home();
        // S-Rc: main = subscription (passthrough) → stripped; subagent stays.
        let out = materialize_for_local_router(&examples_dir().join("S-Rc.json"), &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert!(
            cfg["routes"].get("main").is_none(),
            "subscription main must be stripped"
        );
        assert_eq!(cfg["routes"]["subagent"]["endpoint"], "rayline-cloud");
        // Rc-L: main is a real endpoint → file used verbatim (path unchanged).
        let rl = examples_dir().join("Rc-L.json");
        assert_eq!(materialize_for_local_router(&rl, &home).unwrap(), rl);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_codex_subscription_rewrites_sentinel_to_client_bearer_endpoint() {
        let home = tmp_home();
        let path = home.join("codex-router.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "endpoints": [{
                    "id": "ollama",
                    "protocol": "openai_chat",
                    "base_url": "http://127.0.0.1:11434/v1",
                    "models": ["qwen"]
                }],
                "routes": {
                    "main": {"endpoint": "subscription", "model": "gpt-5.5"},
                    "subagent": {"endpoint": "ollama", "model": "qwen"}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = materialize_codex_subscription_for_local_router(&path, &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(
            cfg["routes"]["main"]["endpoint"],
            crate::codex::CODEX_SUBSCRIPTION_ENDPOINT_ID
        );
        assert_eq!(cfg["routes"]["main"]["model"], "gpt-5.5");
        assert!(cfg["endpoints"].as_array().unwrap().iter().any(|endpoint| {
            endpoint["id"] == crate::codex::CODEX_SUBSCRIPTION_ENDPOINT_ID
                && endpoint["auth"] == "client_bearer"
                && endpoint["base_url"] == crate::codex::CODEX_SUBSCRIPTION_BASE_URL
        }));
        assert_eq!(cfg["routes"]["subagent"]["endpoint"], "ollama");
        // Sentinel `--model` pinned to the (rewritten) subscription main so Codex
        // MAIN turns reach it; the router skips it for subagent turns.
        for model in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][model]["endpoint"],
                crate::codex::CODEX_SUBSCRIPTION_ENDPOINT_ID,
                "model_route {model} should target the subscription endpoint"
            );
        }

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_codex_config_pins_sentinel_models_to_main() {
        // Non-subscription Codex --config (local main): the sentinel `--model` pins
        // to routes.main (ollama) so MAIN turns reach it; the router skips it on
        // subagent turns so routes.subagent governs those.
        let home = tmp_home();
        let path = home.join("codex-local.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "endpoints": [{
                    "id": "ollama",
                    "protocol": "openai_chat",
                    "base_url": "http://127.0.0.1:11434/v1",
                    "models": ["qwen3.5:9b"]
                }],
                "routes": {
                    "main": {"endpoint": "ollama", "model": "qwen3.5:9b"},
                    "subagent": {"endpoint": "ollama", "model": "qwen3.5:9b"}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = materialize_codex_config_for_local_router(&path, &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        for model in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][model]["endpoint"], "ollama",
                "sentinel {model} should pin to the configured main endpoint"
            );
            assert_eq!(cfg["routes"]["model_routes"][model]["model"], "qwen3.5:9b");
        }
        assert_eq!(cfg["routes"]["main"]["endpoint"], "ollama");
        assert_eq!(cfg["routes"]["subagent"]["endpoint"], "ollama");

        let _ = std::fs::remove_dir_all(&home);
    }

    /// `rayline codex --config <file>` where the file declares only `routes.main`
    /// (a single-model setup): the sentinel `--model` pins to that main and NO
    /// `subagent` route is invented. The local router then inherits main for
    /// subagent turns, so one route entry drives the whole session.
    #[test]
    fn materialize_codex_config_leaves_main_only_config_without_subagent() {
        let home = tmp_home();
        let path = home.join("codex-main-only.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "endpoints": [{
                    "id": "openrouter",
                    "protocol": "anthropic_messages",
                    "base_url": "https://openrouter.ai/api",
                    "api_key_env": "OPENROUTER_API_KEY",
                    "auth": "bearer",
                    "models": ["moonshotai/kimi-k3"]
                }],
                "routes": {
                    "main": {"endpoint": "openrouter", "model": "moonshotai/kimi-k3"}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = materialize_codex_config_for_local_router(&path, &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(cfg["routes"]["main"]["endpoint"], "openrouter");
        assert!(
            cfg["routes"].get("subagent").is_none(),
            "no subagent route should be invented; the router inherits main"
        );
        for model in ["rayline-local", "rayline-codex"] {
            assert_eq!(
                cfg["routes"]["model_routes"][model]["endpoint"], "openrouter",
                "sentinel {model} should pin to the configured main endpoint"
            );
            assert_eq!(
                cfg["routes"]["model_routes"][model]["model"],
                "moonshotai/kimi-k3"
            );
        }

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_codex_config_skips_subscription_passthrough_main() {
        // A passthrough (subscription) main has no concrete endpoint here, so no
        // sentinel model_routes are injected and main is stripped (like the
        // non-codex materialization) — that combo is for --auth subscription.
        let home = tmp_home();
        let path = home.join("codex-passthrough.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "endpoints": [{
                    "id": "ollama",
                    "protocol": "openai_chat",
                    "base_url": "http://127.0.0.1:11434/v1",
                    "models": ["qwen3.5:9b"]
                }],
                "routes": {
                    "main": {"endpoint": "subscription"},
                    "subagent": {"endpoint": "ollama", "model": "qwen3.5:9b"}
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = materialize_codex_config_for_local_router(&path, &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert!(
            cfg["routes"].get("main").is_none(),
            "passthrough main stripped"
        );
        assert!(
            cfg["routes"].get("model_routes").is_none(),
            "no sentinel routes injected for a passthrough main"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn materialize_fills_missing_model_on_may_local_cloud_route() {
        // Regression: a may-local `rayline-cloud` route may declare only
        // `local_models` and omit `model` (accepted by config_may_local). The local
        // router rejects a non-local route with an empty model at startup, so
        // materialization must fill the `rayline-router` sentinel first.
        let home = tmp_home();
        let path = home.join("codex-maylocal-no-model.json");
        std::fs::write(
            &path,
            serde_json::to_vec_pretty(&json!({
                "endpoints": [
                    { "id": "rayline-cloud", "protocol": "anthropic_messages",
                      "base_url": "https://api-dev.rayline.ai",
                      "api_key_env": "RAYLINE_ROUTER_API_KEY", "models": ["rayline-router"] },
                    { "id": "local-bundled", "protocol": "openai_responses",
                      "base_url": "http://127.0.0.1:8899", "models": ["qwen3.6-27b-iq3xxs"] }
                ],
                "routes": {
                    // No `model` — only `local_models`.
                    "subagent": { "endpoint": "rayline-cloud",
                                  "local_models": ["qwen3.6-27b-iq3xxs"] }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = materialize_codex_config_for_local_router(&path, &home).unwrap();
        let cfg: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        // The RCR sentinel model is filled, so the local router's non-empty-model
        // check accepts the route; local_models is preserved.
        assert_eq!(cfg["routes"]["subagent"]["model"], "rayline-router");
        assert_eq!(
            cfg["routes"]["subagent"]["local_models"][0],
            "qwen3.6-27b-iq3xxs"
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn may_local_resolves_model_and_upstream_for_rrcl() {
        // Rcl-Rcl: rayline-cloud routes carrying `local_models`, plus a local endpoint
        // that serves the advertised model.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] },
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-cloud",
                          "local_models": ["qwen2.5-coder:7b"] },
                "subagent": { "endpoint": "rayline", "router": "rayline-cloud",
                              "local_models": ["qwen2.5-coder:7b"] }
            }
        });
        assert_eq!(
            config_value_may_local(&cfg),
            Some(MayLocal {
                model: "qwen2.5-coder:7b".to_owned(),
                upstream_url: "http://127.0.0.1:11434".to_owned(),
            })
        );
    }

    #[test]
    fn may_local_off_when_no_local_models() {
        // Rc-Rc: rayline-cloud, no `local_models` → may-local off.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-cloud" },
                "subagent": { "endpoint": "rayline", "router": "rayline-cloud" }
            }
        });
        assert_eq!(config_value_may_local(&cfg), None);
        assert_eq!(config_advertised_local_model(&cfg), None);
    }

    #[test]
    fn may_local_ignored_for_rayline_local_router() {
        // Rl-Rl-shaped: `rayline-local` routes never advertise may-local (N/A), even
        // if a stray `local_models` is present.
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] },
                { "id": "ollama", "protocol": "openai_chat",
                  "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-local",
                          "local_models": ["qwen2.5-coder:7b"] }
            }
        });
        assert_eq!(config_value_may_local(&cfg), None);
    }

    #[test]
    fn may_local_none_when_model_endpoint_undeclared() {
        // `local_models` names a model no local endpoint serves → cannot resolve an
        // upstream, so may-local does not engage (the CLI surfaces a clear error
        // path instead of silently advertising an unreachable model).
        let cfg = json!({
            "endpoints": [
                { "id": "rayline", "protocol": "anthropic_messages",
                  "base_url": crate::ROUTER_PROD_URL, "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline", "router": "rayline-cloud",
                          "local_models": ["qwen2.5-coder:7b"] }
            }
        });
        assert_eq!(
            config_advertised_local_model(&cfg),
            Some("qwen2.5-coder:7b".to_owned())
        );
        assert_eq!(endpoint_base_url_for_model(&cfg, "qwen2.5-coder:7b"), None);
        assert_eq!(config_value_may_local(&cfg), None);
    }

    #[test]
    fn all_cloud_router_routes_stay_remote() {
        let cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": "https://api.rayline.ai", "models": ["rayline-router"] }
            ],
            "routes": {
                "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                "default": { "endpoint": "rayline-cloud", "model": "rayline-router" }
            }
        });
        assert!(!config_value_needs_local_router(&cfg));
    }

    #[test]
    fn dev_hosted_rcr_routes_behave_exactly_like_prod() {
        // Same config twice, prod host vs dev host. Every routing answer must
        // match: the dev RCR is the same hosted router on a different hostname.
        let build = |base_url: &str| {
            json!({
                "endpoints": [
                    { "id": "rayline", "protocol": "anthropic_messages",
                      "base_url": base_url, "models": ["rayline-router"] },
                    { "id": "ollama", "protocol": "openai_chat",
                      "base_url": "http://127.0.0.1:11434/v1", "models": ["qwen2.5-coder:7b"] }
                ],
                "routes": {
                    "main": { "endpoint": "rayline", "router": "rayline-cloud",
                              "local_models": ["qwen2.5-coder:7b"] },
                    "subagent": { "endpoint": "rayline", "router": "rayline-cloud",
                                  "local_models": ["qwen2.5-coder:7b"] }
                }
            })
        };
        let prod = build(crate::ROUTER_PROD_URL);
        let dev = build("https://api-dev.rayline.ai");

        // An all-cloud config must not engage the on-device LSR — on either host.
        assert!(!config_value_needs_local_router(&prod));
        assert!(!config_value_needs_local_router(&dev));

        // May-local must resolve to the *local* endpoint, never back to the
        // hosted RCR the redirect came from.
        let expected = Some(MayLocal {
            model: "qwen2.5-coder:7b".to_owned(),
            upstream_url: "http://127.0.0.1:11434".to_owned(),
        });
        assert_eq!(config_value_may_local(&prod), expected);
        assert_eq!(config_value_may_local(&dev), expected);
    }

    // ── Codex native-Responses forwarding to the hosted RCR (edge half of #36) ──

    #[test]
    fn codex_native_flips_only_hosted_rayline_cloud() {
        // A hosted `rayline-cloud` (anthropic_messages) endpoint alongside a
        // user's own custom `anthropic_messages` endpoint. Only the hosted one
        // must flip to native Responses; the custom endpoint stays untouched.
        let mut cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": "https://api.rayline.ai", "api_key_env": "RAYLINE_ROUTER_API_KEY",
                  "auth": "api_key", "models": ["rayline-router"] },
                { "id": "my-anthropic", "protocol": "anthropic_messages",
                  "base_url": "https://my-gateway.example.com", "auth": "api_key" }
            ],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });

        let changed = rewrite_rayline_cloud_for_codex_native(&mut cfg);
        assert!(changed);

        let endpoints = cfg["endpoints"].as_array().unwrap();
        let hosted = &endpoints[0];
        assert_eq!(hosted["protocol"], "openai_responses");
        assert_eq!(hosted["base_url"], "https://api.rayline.ai/v1");

        let custom = &endpoints[1];
        assert_eq!(
            custom["protocol"], "anthropic_messages",
            "custom endpoint untouched"
        );
        assert_eq!(custom["auth"], "api_key");
        assert!(
            custom.get("headers").is_none(),
            "no client header on custom endpoint"
        );

        // Idempotent: a second pass changes nothing.
        assert!(!rewrite_rayline_cloud_for_codex_native(&mut cfg));
    }

    #[test]
    fn codex_native_stamps_client_header_and_bearer_auth() {
        // Pins the exact contract with the RCR: native Responses protocol, bearer
        // auth (so the rlk- key rides on `Authorization: Bearer`, the header the
        // RCR accepts), and the x-rayline-client: codex header.
        let mut cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": "https://api-dev.rayline.ai", "api_key_env": "RAYLINE_ROUTER_API_KEY",
                  "auth": "api_key", "models": ["rayline-router"] }
            ],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });

        assert!(rewrite_rayline_cloud_for_codex_native(&mut cfg));

        let ep = &cfg["endpoints"][0];
        assert_eq!(ep["protocol"], "openai_responses");
        assert_eq!(ep["auth"], "bearer");
        assert_eq!(ep["headers"]["x-rayline-client"], "codex");
        // base_url gains `/v1` so the native passthrough reaches `/v1/responses`
        // (it strips the inbound `/v1/` prefix before appending `responses`).
        assert_eq!(ep["base_url"], "https://api-dev.rayline.ai/v1");
        // api_key_env is preserved so the rlk- key still loads.
        assert_eq!(ep["api_key_env"], "RAYLINE_ROUTER_API_KEY");
    }

    #[test]
    fn codex_native_base_url_gets_v1_suffix_idempotently() {
        // Bare host root → `/v1`; a base_url already ending in `/v1` is untouched.
        let mut ep = serde_json::Map::new();
        ep.insert("base_url".to_owned(), json!("https://api.rayline.ai"));
        assert!(ensure_rcr_base_url_has_v1(&mut ep));
        assert_eq!(ep["base_url"], "https://api.rayline.ai/v1");
        assert!(!ensure_rcr_base_url_has_v1(&mut ep), "idempotent");

        let mut trailing = serde_json::Map::new();
        trailing.insert("base_url".to_owned(), json!("https://api.rayline.ai/"));
        assert!(ensure_rcr_base_url_has_v1(&mut trailing));
        assert_eq!(trailing["base_url"], "https://api.rayline.ai/v1");
    }

    #[test]
    fn codex_native_via_materialize_config() {
        // End-to-end through the codex `--config` materializer (Rc-Rc shape).
        let dir = std::env::temp_dir().join(format!("rayline-codex-native-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("Rc-Rc.json");
        std::fs::write(
            &cfg_path,
            serde_json::to_vec(&json!({
                "endpoints": [
                    { "id": "rayline-cloud", "protocol": "anthropic_messages",
                      "base_url": "https://api.rayline.ai", "api_key_env": "RAYLINE_ROUTER_API_KEY",
                      "auth": "api_key", "models": ["rayline-router"] }
                ],
                "routes": {
                    "main": { "endpoint": "rayline-cloud", "model": "rayline-router" },
                    "subagent": { "endpoint": "rayline-cloud", "model": "rayline-router" }
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let out = materialize_codex_config_for_local_router(&cfg_path, &dir).unwrap();
        let materialized: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        let ep = &materialized["endpoints"][0];
        assert_eq!(ep["protocol"], "openai_responses");
        assert_eq!(ep["auth"], "bearer");
        assert_eq!(ep["headers"]["x-rayline-client"], "codex");
        assert_eq!(ep["base_url"], "https://api.rayline.ai/v1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hosted_rcr_route_detection_covers_prod_dev_not_local() {
        let dir = std::env::temp_dir().join(format!("rayline-hosted-rcr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let write = |name: &str, base_url: &str| {
            let path = dir.join(name);
            std::fs::write(
                &path,
                serde_json::to_vec(&json!({
                    "endpoints": [
                        { "id": "rayline-cloud", "protocol": "anthropic_messages",
                          "base_url": base_url, "api_key_env": "RAYLINE_ROUTER_API_KEY",
                          "auth": "api_key", "models": ["rayline-router"] }
                    ],
                    "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
                }))
                .unwrap(),
            )
            .unwrap();
            path
        };

        // Prod and dev hosts both count — matching the Codex-native host guard.
        assert!(config_routes_to_hosted_rcr(&write(
            "prod.json",
            "https://api.rayline.ai"
        )));
        assert!(config_routes_to_hosted_rcr(&write(
            "dev.json",
            "https://api-dev.rayline.ai"
        )));
        // A user's own custom host must not trigger provisioning.
        assert!(!config_routes_to_hosted_rcr(&write(
            "custom.json",
            "https://not-the-rcr.example.com"
        )));

        // A purely local config (no hosted endpoint) never provisions.
        let local = dir.join("local.json");
        std::fs::write(
            &local,
            r#"{"endpoints":[{"id":"ollama","protocol":"openai_chat","base_url":"http://127.0.0.1:11434/v1","models":["qwen"]}],"routes":{"main":{"endpoint":"ollama","model":"qwen"}}}"#,
        )
        .unwrap();
        assert!(!config_routes_to_hosted_rcr(&local));
        // `config_uses_cloud_router` agrees on every host. It used to answer
        // prod-only, which is what left dev-targeted configs unprovisioned.
        assert!(config_uses_cloud_router(&write(
            "dev2.json",
            "https://api-dev.rayline.ai"
        )));
        assert!(config_uses_cloud_router(&write(
            "prod2.json",
            "https://api.rayline.ai"
        )));
        assert!(!config_uses_cloud_router(&write(
            "custom2.json",
            "https://not-the-rcr.example.com"
        )));
        assert!(!config_uses_cloud_router(&local));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_native_normalizes_already_native_endpoint_auth() {
        // A partially-migrated hosted endpoint: already `openai_responses` but
        // still `auth: api_key`. Must be normalized to bearer (+ /v1 + header),
        // not left sending the rlk- key as x-api-key.
        let mut cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "openai_responses",
                  "base_url": "https://api.rayline.ai", "api_key_env": "RAYLINE_ROUTER_API_KEY",
                  "auth": "api_key", "models": ["rayline-router"] }
            ],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });
        assert!(rewrite_rayline_cloud_for_codex_native(&mut cfg));
        let ep = &cfg["endpoints"][0];
        assert_eq!(ep["protocol"], "openai_responses");
        assert_eq!(
            ep["auth"], "bearer",
            "stale api_key auth must be normalized"
        );
        assert_eq!(ep["base_url"], "https://api.rayline.ai/v1");
        assert_eq!(ep["headers"]["x-rayline-client"], "codex");
        // Fully normalized → idempotent.
        assert!(!rewrite_rayline_cloud_for_codex_native(&mut cfg));
    }

    #[test]
    fn codex_native_ignores_custom_host() {
        // A user's own anthropic endpoint at a non-RCR host must never flip.
        let mut cfg = json!({
            "endpoints": [
                { "id": "rayline-cloud", "protocol": "anthropic_messages",
                  "base_url": "https://not-the-rcr.example.com", "auth": "api_key" }
            ],
            "routes": { "main": { "endpoint": "rayline-cloud", "model": "rayline-router" } }
        });
        assert!(!rewrite_rayline_cloud_for_codex_native(&mut cfg));
        assert_eq!(cfg["endpoints"][0]["protocol"], "anthropic_messages");
    }

    #[test]
    fn hosted_rcr_host_detection() {
        assert!(endpoint_base_url_is_hosted_rcr(Some(
            "https://api.rayline.ai"
        )));
        assert!(endpoint_base_url_is_hosted_rcr(Some(
            "https://api-dev.rayline.ai"
        )));
        assert!(!endpoint_base_url_is_hosted_rcr(Some(
            "https://api.rayline.ai.evil.com"
        )));
        assert!(!endpoint_base_url_is_hosted_rcr(Some(
            "https://openrouter.ai/api"
        )));
        assert!(!endpoint_base_url_is_hosted_rcr(None));
    }
}
