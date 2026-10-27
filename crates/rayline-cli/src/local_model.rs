//! Local model routing endpoint config (`<cli> local …`).
//!
//! Selects which local inference endpoint this machine offers to the cloud
//! router for hybrid local routing. Two modes:
//!
//! - **Recommended** (`mode: "recommended"`, legacy `"auto"`): the bundled
//!   llama.cpp server run by the daemon, serving a model picked from the
//!   curated catalog (`model_id`/repo/file plus revision/SHA). Without a pick
//!   the bundled default model applies.
//! - **Custom** (`mode: "custom"`): the user's own Ollama / LM Studio /
//!   llama.cpp server, specified by `base_url` + `model`.
//!
//! The config is stored under the `local_model` key of
//! `~/.config/<brand>/settings.json` — the same file the menu bar app reads,
//! so the two surfaces stay in lockstep.
//!
//! This is **config only**. Whether local routing actually engages is gated by
//! the server-side `enable_local_router` toggle (flipped from the menu bar, or
//! a forthcoming `rayline local on`); a configured endpoint here is necessary but
//! not sufficient. `rayline claude` engagement and the server read live in a later
//! phase and are intentionally not handled here.

use std::io;
use std::path::Path;
use std::time::Duration;

use serde_json::{Map, Value, json};

use crate::status;

/// Probe timeout. A cold local model server (first request loads weights) can
/// take a while to answer, so keep this generous.
const TEST_TIMEOUT_SECONDS: u64 = 30;

/// Which local endpoint this machine offers. Serialized as
/// `"recommended"`/`"custom"` under `local_model.mode` (legacy `"auto"` reads
/// as `Recommended`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LocalModelMode {
    /// Use the bundled llama.cpp server with a curated-catalog model.
    Recommended,
    /// Use the user-provided endpoint at `base_url` advertising `model`.
    Custom,
}

impl LocalModelMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Recommended => "recommended",
            Self::Custom => "custom",
        }
    }
}

/// A saved custom endpoint from the shared `custom_endpoints` list. The CLI
/// reads these to count "added models" and to activate a sole endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SavedCustomEndpoint {
    pub base_url: String,
    pub model: String,
}

/// The stored `local_model` config. `base_url`/`model` are only meaningful in
/// `Custom` mode and model identity/trust metadata in `Recommended`
/// mode, but each set is retained across a mode flip so the user can switch
/// back without re-picking.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalModelConfig {
    pub mode: LocalModelMode,
    pub provider: Option<String>,
    pub protocol: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Curated catalog id of the recommended pick (e.g. `qwen3.6-27b-q4km`).
    pub model_id: Option<String>,
    /// HuggingFace repo of the recommended pick.
    pub model_repo: Option<String>,
    /// GGUF filename of the recommended pick.
    pub model_file: Option<String>,
    /// HuggingFace revision/commit for the recommended pick.
    pub model_revision: Option<String>,
    /// Expected GGUF SHA256 for the recommended pick.
    pub model_sha256: Option<String>,
    /// All saved custom endpoints, including a complete active `base_url`/
    /// `model` pair written by another Rayline client.
    pub custom_endpoints: Vec<SavedCustomEndpoint>,
}

impl LocalModelConfig {
    /// Whether this config can actually drive `rayline claude` local routing.
    /// Recommended needs an explicit verified pick (repo/file + revision/SHA) —
    /// there is deliberately NO bundled-default fallback. A pickless config
    /// (e.g. legacy `mode: "auto"`) is resolved at engagement time: the best
    /// already-downloaded curated model is adopted as the pick, else routing
    /// stays cloud with a warning. Custom needs BOTH a base URL and a model
    /// name — without the model the adapter would rewrite requests to a model
    /// id the user's custom server rejects.
    pub fn is_engageable(&self) -> bool {
        match self.mode {
            LocalModelMode::Recommended => self.has_recommended_pick(),
            LocalModelMode::Custom => self.base_url.is_some() && self.model.is_some(),
        }
    }

    /// Whether a Recommended-mode model has been picked with trust metadata.
    pub fn has_recommended_pick(&self) -> bool {
        self.model_repo.is_some()
            && self.model_file.is_some()
            && self.model_revision.is_some()
            && self.model_sha256.is_some()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalCustomRequest {
    /// New base URL, if `--url` was given. When absent, the stored URL is kept
    /// (so `custom --model …` can switch models without re-typing the URL).
    pub base_url: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalTestRequest {
    /// Endpoint to probe. Falls back to the stored config when absent.
    pub base_url: Option<String>,
    pub model: Option<String>,
}

/// Strip the noise that distinguishes equivalent endpoints so a user can paste
/// either `http://host:port` or `http://host:port/v1`. Claude Code (and the
/// probe) append `/v1/messages` themselves, so the stored value is the server
/// root. This mirrors hosted-router normalization before handing the URL to the
/// Anthropic-compatible SDK.
pub fn normalize_base_url(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    let stripped = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    stripped.trim_end_matches('/').to_owned()
}

/// Read the stored local-model config, or `None` when the `local_model` key is
/// absent. Lenient about `mode`: legacy `"auto"` reads as `recommended`, and a
/// missing/unrecognized mode defaults to `custom` when a `base_url` exists,
/// otherwise `recommended`.
pub fn read_from_home(home: &Path) -> Option<LocalModelConfig> {
    let settings = status::read_settings(home)?;
    let entry = settings.get("local_model")?;
    if !entry.is_object() {
        return None;
    }

    let string_field = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    };
    let base_url = string_field("base_url");
    let model = string_field("model");
    let provider = string_field("provider");
    let protocol = string_field("protocol");
    let model_id = string_field("model_id");
    let model_repo = string_field("model_repo");
    let model_file = string_field("model_file");
    let model_revision = string_field("model_revision");
    let model_sha256 = string_field("model_sha256");

    let mode = match entry.get("mode").and_then(Value::as_str) {
        // "auto" predates the catalog picker; it means the same bundled-server
        // path with the default model.
        Some("recommended") | Some("auto") => LocalModelMode::Recommended,
        Some("custom") => LocalModelMode::Custom,
        // Be lenient: an unset/unknown mode falls back to custom when a URL is
        // present (the only thing custom needs), else recommended.
        _ if base_url.is_some() => LocalModelMode::Custom,
        _ => LocalModelMode::Recommended,
    };

    let mut custom_endpoints = entry
        .get("custom_endpoints")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let endpoint = SavedCustomEndpoint {
                        base_url: item.get("base_url")?.as_str()?.trim().to_owned(),
                        model: item.get("model")?.as_str()?.trim().to_owned(),
                    };
                    (!endpoint.base_url.is_empty() && !endpoint.model.is_empty())
                        .then_some(endpoint)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    // A complete active pair written by this CLI or another Rayline client
    // counts as a saved endpoint too.
    if let (Some(url), Some(model_name)) = (&base_url, &model) {
        let active = SavedCustomEndpoint {
            base_url: url.clone(),
            model: model_name.clone(),
        };
        if !custom_endpoints.contains(&active) {
            custom_endpoints.insert(0, active);
        }
    }

    Some(LocalModelConfig {
        mode,
        provider,
        protocol,
        base_url,
        model,
        model_id,
        model_repo,
        model_file,
        model_revision,
        model_sha256,
        custom_endpoints,
    })
}

/// Select a curated catalog model: switch to Recommended mode and persist the
/// pick's identity. Preserves any stored custom `base_url`/`model` so the user
/// can flip back to custom without re-typing them. Returns the resulting
/// config.
pub fn set_recommended(model: &crate::catalog::CatalogModel) -> Result<LocalModelConfig, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    set_recommended_in_home(&home, model)
        .map_err(|error| format!("failed to write settings: {error}"))
}

pub(crate) fn set_recommended_in_home(
    home: &Path,
    model: &crate::catalog::CatalogModel,
) -> io::Result<LocalModelConfig> {
    let existing = read_from_home(home);
    let config = LocalModelConfig {
        mode: LocalModelMode::Recommended,
        provider: Some("llamacpp".to_owned()),
        protocol: Some("anthropic_messages".to_owned()),
        base_url: existing.as_ref().and_then(|c| c.base_url.clone()),
        model: existing.and_then(|c| c.model),
        model_id: Some(model.id.clone()),
        model_repo: Some(model.repo.clone()),
        model_file: Some(model.filename.clone()),
        model_revision: Some(model.revision.clone()),
        model_sha256: Some(model.sha256.clone()),
        custom_endpoints: Vec::new(), // read-only mirror; never written
    };
    write_in_home(home, &config)?;
    Ok(config)
}

/// Set Custom mode with the provided endpoint. At least one of URL/model must
/// be given; a base URL must exist afterward (provided or stored from a prior
/// `custom`). The omitted flag keeps its stored value.
pub fn set_custom(request: &LocalCustomRequest) -> Result<LocalModelConfig, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    set_custom_in_home(&home, request)
}

fn set_custom_in_home(
    home: &Path,
    request: &LocalCustomRequest,
) -> Result<LocalModelConfig, String> {
    let existing = read_from_home(home);

    // Both flags omitted is only valid when a usable endpoint is already
    // stored (e.g. flipping back to custom after `auto`); otherwise the user
    // must supply at least a URL.
    if request.base_url.is_none()
        && request.model.is_none()
        && existing
            .as_ref()
            .and_then(|config| config.base_url.as_ref())
            .is_none()
    {
        return Err(format!(
            "Nothing to set. Provide --url and --model. Run: {} local custom --url <URL> --model <NAME>",
            crate::CLI_BIN
        ));
    }

    let base_url = match request.base_url.as_deref() {
        Some(raw) => {
            let normalized = normalize_base_url(raw);
            if normalized.is_empty() {
                return Err("--url must be a non-empty server URL".to_owned());
            }
            normalized
        }
        None => existing
            .as_ref()
            .and_then(|config| config.base_url.clone())
            .ok_or_else(|| {
                format!(
                    "No local model URL stored yet. Run: {} local custom --url <URL> --model <NAME>",
                    crate::CLI_BIN
                )
            })?,
    };
    let model = match request.model.as_deref().map(str::trim) {
        Some(model) if !model.is_empty() => Some(model.to_owned()),
        Some(_) => None, // explicit empty --model clears the stored model
        None => existing.as_ref().and_then(|config| config.model.clone()),
    };
    // No client-side family allowlist here: custom endpoints use the legacy
    // custom-route signal, and curated registry entries are validated before
    // publication.

    let config = LocalModelConfig {
        mode: LocalModelMode::Custom,
        provider: None,
        protocol: None,
        base_url: Some(base_url),
        model,
        // Preserve the recommended pick so flipping back needs no re-pick.
        model_id: existing.as_ref().and_then(|c| c.model_id.clone()),
        model_repo: existing.as_ref().and_then(|c| c.model_repo.clone()),
        model_file: existing.as_ref().and_then(|c| c.model_file.clone()),
        model_revision: existing.as_ref().and_then(|c| c.model_revision.clone()),
        model_sha256: existing.and_then(|c| c.model_sha256),
        custom_endpoints: Vec::new(), // read-only mirror; never written
    };
    write_in_home(home, &config).map_err(|error| format!("failed to write settings: {error}"))?;
    Ok(config)
}

pub fn set_provider_endpoint(
    provider: &str,
    base_url: &str,
    model: &str,
    protocol: &str,
) -> Result<LocalModelConfig, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    set_provider_endpoint_in_home(&home, provider, base_url, model, protocol)
}

pub(crate) fn set_provider_endpoint_in_home(
    home: &Path,
    provider: &str,
    base_url: &str,
    model: &str,
    protocol: &str,
) -> Result<LocalModelConfig, String> {
    let provider = provider.trim();
    let protocol = protocol.trim();
    let model = model.trim();
    if provider.is_empty() {
        return Err("provider must be non-empty".to_owned());
    }
    if protocol.is_empty() {
        return Err("protocol must be non-empty".to_owned());
    }
    if model.is_empty() {
        return Err("model must be non-empty".to_owned());
    }
    let base_url = normalize_base_url(base_url);
    if base_url.is_empty() {
        return Err("base URL must be non-empty".to_owned());
    }

    let existing = read_from_home(home);
    let config = LocalModelConfig {
        mode: LocalModelMode::Custom,
        provider: Some(provider.to_owned()),
        protocol: Some(protocol.to_owned()),
        base_url: Some(base_url),
        model: Some(model.to_owned()),
        model_id: existing.as_ref().and_then(|c| c.model_id.clone()),
        model_repo: existing.as_ref().and_then(|c| c.model_repo.clone()),
        model_file: existing.as_ref().and_then(|c| c.model_file.clone()),
        model_revision: existing.as_ref().and_then(|c| c.model_revision.clone()),
        model_sha256: existing.and_then(|c| c.model_sha256),
        custom_endpoints: Vec::new(),
    };
    write_in_home(home, &config).map_err(|error| format!("failed to write settings: {error}"))?;
    Ok(config)
}

/// Make a saved custom endpoint the active selection (mode custom + active
/// fields — the shape the engagement path reads). Used when a sole saved
/// endpoint is auto-selected.
pub(crate) fn activate_custom_endpoint_in_home(
    home: &Path,
    endpoint: &SavedCustomEndpoint,
) -> io::Result<LocalModelConfig> {
    let mut config = read_from_home(home).unwrap_or(LocalModelConfig {
        mode: LocalModelMode::Custom,
        provider: None,
        protocol: None,
        base_url: None,
        model: None,
        model_id: None,
        model_repo: None,
        model_file: None,
        model_revision: None,
        model_sha256: None,
        custom_endpoints: Vec::new(),
    });
    config.mode = LocalModelMode::Custom;
    config.provider = None;
    config.protocol = None;
    config.base_url = Some(endpoint.base_url.clone());
    config.model = Some(endpoint.model.clone());
    write_in_home(home, &config)?;
    Ok(config)
}

/// Drop the recommended pick (used when its file is deleted from disk),
/// keeping the mode and any custom endpoint fields.
pub(crate) fn clear_recommended_pick_in_home(home: &Path) -> io::Result<()> {
    let Some(mut config) = read_from_home(home) else {
        return Ok(());
    };
    config.model_id = None;
    config.model_repo = None;
    config.model_file = None;
    config.model_revision = None;
    config.model_sha256 = None;
    write_in_home(home, &config)
}

fn write_in_home(home: &Path, config: &LocalModelConfig) -> io::Result<()> {
    let mut settings = status::read_settings(home)
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    let object = settings
        .as_object_mut()
        .expect("settings is an object by construction");

    // Merge into the existing entry rather than rebuilding it: keys this CLI
    // doesn't manage (e.g. another Rayline client's `custom_endpoints` list)
    // must survive every write. Managed keys are set when present and removed
    // when None.
    let mut entry = object
        .get("local_model")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_else(Map::new);
    entry.insert(
        "mode".to_owned(),
        Value::String(config.mode.as_str().to_owned()),
    );
    for (key, value) in [
        ("base_url", &config.base_url),
        ("provider", &config.provider),
        ("protocol", &config.protocol),
        ("model", &config.model),
        ("model_id", &config.model_id),
        ("model_repo", &config.model_repo),
        ("model_file", &config.model_file),
        ("model_revision", &config.model_revision),
        ("model_sha256", &config.model_sha256),
    ] {
        match value {
            Some(value) => entry.insert(key.to_owned(), Value::String(value.clone())),
            None => entry.remove(key),
        };
    }
    object.insert("local_model".to_owned(), Value::Object(entry));
    status::write_settings(home, &settings)
}

/// Remove the stored local-model config. Returns whether anything was removed.
pub fn clear() -> Result<bool, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    clear_in_home(&home).map_err(|error| format!("failed to write settings: {error}"))
}

fn clear_in_home(home: &Path) -> io::Result<bool> {
    let Some(mut settings) = status::read_settings(home) else {
        return Ok(false);
    };
    let Some(object) = settings.as_object_mut() else {
        return Ok(false);
    };
    if object.remove("local_model").is_none() {
        return Ok(false);
    }
    status::write_settings(home, &settings)?;
    Ok(true)
}

/// Human-readable `show` output. Best-effort fetches the server
/// `enable_local_router` state; if that fails (offline / not signed in) the
/// local config is still shown with "unknown" for the toggle.
pub async fn render_show(env_name: Option<&str>, auth_token: Option<&str>) -> String {
    let Some(home) = dirs::home_dir() else {
        return "home directory not found\n".to_owned();
    };
    let server_state = read_server_enable_local_router(&home, env_name, auth_token).await;
    render_show_from_home(&home, server_state)
}

/// Pure renderer. `server_state` is the account `enable_local_router` toggle:
/// `Some(true/false)` when known, `None` when the fetch failed (offline).
fn render_show_from_home(home: &Path, server_state: Option<bool>) -> String {
    let cli = crate::CLI_BIN;
    let routing_line = match server_state {
        Some(true) => "Local routing (account): ON\n".to_owned(),
        Some(false) => {
            format!("Local routing (account): OFF — turn it on with `{cli} local on`\n")
        }
        None => "Local routing (account): unknown (offline or not signed in)\n".to_owned(),
    };
    match read_from_home(home) {
        Some(config) => match config.mode {
            LocalModelMode::Recommended => {
                let model_line = match config.model_id.as_deref() {
                    Some(model_id) => format!("  Model:  {model_id}\n"),
                    None => format!(
                        "  Model:  (default — pick one with `{cli} local use <model-id>`)\n"
                    ),
                };
                format!(
                    "Local model: Recommended (built-in llama server)\n{model_line}{routing_line}",
                )
            }
            LocalModelMode::Custom => {
                let url = config
                    .base_url
                    .as_deref()
                    .unwrap_or("(not set — pass --url)");
                let model = config
                    .model
                    .as_deref()
                    .unwrap_or("(not set — pass --model)");
                let title = match config.provider.as_deref() {
                    Some("ollama") => "Ollama",
                    Some("lmstudio") => "LM Studio",
                    Some("llamacpp") => "Built-in llama.cpp",
                    Some("custom") | None => "Custom endpoint",
                    Some(provider) => provider,
                };
                let protocol_line = config
                    .protocol
                    .as_deref()
                    .map(|protocol| format!("  Protocol: {protocol}\n"))
                    .unwrap_or_default();
                format!(
                    "Local model: {title}\n  URL:    {url}\n  Model:  {model}\n{protocol_line}{routing_line}Test it with `{cli} local test`.\n",
                )
            }
        },
        None => format!(
            "Local model: not configured.\n{routing_line}\nPick a mode:\n  {cli} local use <model-id>             Use the built-in llama server with a recommended model (see `{cli} local models`)\n  {cli} local custom --url <URL> --model <NAME>  Use your own server (Ollama / LM Studio / llama.cpp)\n",
        ),
    }
}

/// Read the account `enable_local_router` toggle from `GET /v1/settings`.
/// Returns `None` on any network/auth failure (best-effort — `show` still
/// prints the local config).
pub async fn read_server_enable_local_router(
    home: &Path,
    env_name: Option<&str>,
    auth_token: Option<&str>,
) -> Option<bool> {
    let env_name = crate::status::resolve_env(env_name, Some(home));
    let hosted = crate::status::resolve_hosted_environment(&env_name, Some(home)).ok()?;
    let settings = fetch_router_settings(&hosted, auth_token).await?;
    Some(
        settings
            .get("settings")
            .and_then(|settings| settings.get("enable_local_router"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    )
}

/// Flip the account `enable_local_router` toggle via `PATCH /v1/settings`.
///
/// Auth is an account bearer token (a settings write rejects API keys), the same
/// credential class the menu bar's `updateLocalRouter` uses. The body matches
/// `applyRouterSettingsUpdate`: `{"enable_local_router": <bool>}`.
pub async fn set_router_enabled(
    enabled: bool,
    env_name: Option<&str>,
    auth_token: Option<&str>,
) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    set_router_enabled_in_home(&home, enabled, env_name, auth_token).await
}

async fn set_router_enabled_in_home(
    home: &Path,
    enabled: bool,
    env_name: Option<&str>,
    auth_token: Option<&str>,
) -> Result<String, String> {
    let env_name = crate::status::resolve_env(env_name, Some(home));
    let hosted = crate::status::resolve_hosted_environment(&env_name, Some(home))
        .map_err(|error| error.to_string())?;
    let token_request = crate::status::AuthTokenRequest {
        env_name: Some(env_name.clone()),
        // Honor an explicit `--auth-token` (scripted / non-interactive use)
        // instead of always reading stored credentials.
        auth_token: auth_token.map(ToOwned::to_owned),
        root_env_explicit: false,
    };
    let bearer_token = match crate::status::resolve_auth_token(&token_request)
        .await
        .map_err(|error| format!("Not signed in: {error}. Run: {} auth login", crate::CLI_BIN))?
    {
        crate::status::AuthTokenOutcome::Available(token) => token,
        crate::status::AuthTokenOutcome::NotLoggedIn => {
            return Err(format!("Not signed in. Run: {} auth login", crate::CLI_BIN));
        }
    };
    let url = format!("{}/v1/settings", hosted.router_url);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| format!("failed to build HTTP client: {error}"))?
        .patch(&url)
        .bearer_auth(bearer_token)
        .json(&json!({ "enable_local_router": enabled }))
        .send()
        .await
        .map_err(|error| format!("Could not reach {url}: {error}"))?;

    let response_status = response.status();
    if response_status.is_success() {
        return Ok(if enabled {
            "Local routing turned ON for your account.\nIt engages when this machine has a local model configured (`local use <model-id>`/`local custom`).".to_owned()
        } else {
            "Local routing turned OFF for your account.".to_owned()
        });
    }
    let detail = response.text().await.unwrap_or_default();
    let detail = detail.trim();
    if detail.is_empty() {
        Err(format!("{url} returned {response_status}."))
    } else {
        Err(format!("{url} returned {response_status}: {detail}"))
    }
}

/// Fetch the caller's router settings once. `None` on any network/auth failure
/// or error payload. Mirrors the consolidated fetch in `claude.rs`; kept local
/// to the `rayline local` surface so `show`/`on`/`off` need no cross-module plumbing.
async fn fetch_router_settings(
    hosted: &crate::status::HostedEnvironment,
    auth_token: Option<&str>,
) -> Option<Value> {
    let token_request = crate::status::AuthTokenRequest {
        env_name: Some(hosted.credential_key.clone()),
        auth_token: auth_token.map(ToOwned::to_owned),
        root_env_explicit: false,
    };
    let bearer_token = match crate::status::resolve_auth_token(&token_request)
        .await
        .ok()?
    {
        crate::status::AuthTokenOutcome::Available(token) => token,
        crate::status::AuthTokenOutcome::NotLoggedIn => return None,
    };
    let url = format!("{}/v1/settings", hosted.router_url);
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?
        .get(url)
        .bearer_auth(bearer_token)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    if body.get("error").is_some() {
        return None;
    }
    Some(body)
}

/// Probe the configured local endpoint. An endpoint with a stored protocol is
/// tested with it; a custom endpoint without one (or `--url`) is tested with
/// both OpenAI Chat Completions (hybrid routing) and Anthropic Messages
/// (`--local` Claude Code), and fails only when neither works.
pub async fn test(request: &LocalTestRequest) -> Result<String, String> {
    let stored = dirs::home_dir().and_then(|home| read_from_home(&home));
    let base_url = match request.base_url.as_deref() {
        Some(raw) => normalize_base_url(raw),
        None => {
            stored
                .as_ref()
                .and_then(|config| config.base_url.clone())
                .ok_or_else(|| {
                    format!(
                        "No custom endpoint configured. Pass --url or run: {} local custom --url <URL> --model <NAME>",
                        crate::CLI_BIN
                    )
                })?
        }
    };
    if base_url.is_empty() {
        return Err("Base URL is empty".to_owned());
    }
    let model = request
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            stored
                .as_ref()
                .and_then(|config| config.model.as_ref())
                .cloned()
        })
        .unwrap_or_else(|| "local".to_owned());

    let protocol = if request.base_url.is_some() {
        None
    } else {
        stored
            .as_ref()
            .and_then(|config| config.protocol.as_deref())
    };
    match protocol {
        Some("openai_chat") => test_openai_chat(&base_url, &model).await,
        Some("anthropic_messages") => test_anthropic_messages(&base_url, &model).await,
        // A custom endpoint with no stated protocol serves two paths:
        // `rayline hybrid` sends OpenAI Chat Completions to it, and
        // `rayline claude --local` sends Anthropic Messages. Both are probed
        // and reported; the test fails only when neither answers.
        _ => {
            let chat = test_openai_chat(&base_url, &model).await;
            let messages = test_anthropic_messages(&base_url, &model).await;
            let first_line = |error: &String| error.lines().next().unwrap_or_default().to_owned();
            let chat_line = match &chat {
                Ok(ok) => format!("Hybrid routing (Chat Completions): {ok}"),
                Err(error) => format!(
                    "Hybrid routing (Chat Completions, `/v1/chat/completions`) cannot use this endpoint: {}",
                    first_line(error)
                ),
            };
            let messages_line = match &messages {
                Ok(ok) => format!("`--local` Claude Code (Anthropic Messages): {ok}"),
                Err(error) => format!(
                    "`--local` Claude Code (Anthropic Messages, `/v1/messages`) cannot use this endpoint: {}",
                    first_line(error)
                ),
            };
            let report = format!("{chat_line}\n{messages_line}");
            if chat.is_err() && messages.is_err() {
                Err(report)
            } else {
                Ok(report)
            }
        }
    }
}

async fn test_anthropic_messages(base_url: &str, model: &str) -> Result<String, String> {
    let url = format!("{base_url}/v1/messages");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| format!("failed to build HTTP client: {error}"))?;
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "ping" }],
    });
    let response = client
        .post(&url)
        .header("x-api-key", "local")
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("Could not reach {url}: {error}\nIs the server running?"))?;

    let response_status = response.status();
    if response_status.is_success() {
        return Ok(format!(
            "Connection OK — {url} answered {response_status} for model `{model}`."
        ));
    }
    let detail = response.text().await.unwrap_or_default();
    let detail = detail.trim();
    let hint = if response_status.as_u16() == 404 {
        "\nA 404 here usually means the server only exposes an OpenAI-compatible API. Claude Code needs an Anthropic Messages API (`/v1/messages`) endpoint."
    } else {
        ""
    };
    if detail.is_empty() {
        Err(format!("{url} returned {response_status}.{hint}"))
    } else {
        Err(format!("{url} returned {response_status}: {detail}{hint}"))
    }
}

async fn test_openai_chat(base_url: &str, model: &str) -> Result<String, String> {
    let url = format!("{base_url}/v1/chat/completions");
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(TEST_TIMEOUT_SECONDS))
        .build()
        .map_err(|error| format!("failed to build HTTP client: {error}"))?;
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{ "role": "user", "content": "ping" }],
    });
    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .map_err(|error| format!("Could not reach {url}: {error}\nIs the server running?"))?;

    let response_status = response.status();
    if response_status.is_success() {
        return Ok(format!(
            "Connection OK — {url} answered {response_status} for model `{model}`."
        ));
    }
    let detail = response.text().await.unwrap_or_default();
    let detail = detail.trim();
    let hint = if response_status.as_u16() == 404 {
        "\nA 404 here usually means the server does not expose an OpenAI-compatible Chat Completions API (`/v1/chat/completions`)."
    } else {
        ""
    };
    if detail.is_empty() {
        Err(format!("{url} returned {response_status}.{hint}"))
    } else {
        Err(format!("{url} returned {response_status}: {detail}{hint}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_home() -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "rayline-local-model-test-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn provider_endpoint_round_trips_with_protocol() {
        let home = temp_home();
        let config = set_provider_endpoint_in_home(
            &home,
            "ollama",
            "http://localhost:11434/v1",
            "qwen3-coder:30b",
            "openai_chat",
        )
        .unwrap();
        assert!(config.is_engageable());
        assert_eq!(config.provider.as_deref(), Some("ollama"));
        assert_eq!(config.protocol.as_deref(), Some("openai_chat"));
        assert_eq!(config.base_url.as_deref(), Some("http://localhost:11434"));
        assert_eq!(config.model.as_deref(), Some("qwen3-coder:30b"));

        let read = read_from_home(&home).unwrap();
        assert_eq!(read.mode, config.mode);
        assert_eq!(read.provider, config.provider);
        assert_eq!(read.protocol, config.protocol);
        assert_eq!(read.base_url, config.base_url);
        assert_eq!(read.model, config.model);
        assert_eq!(read.custom_endpoints.len(), 1);
    }

    #[test]
    fn recommended_without_pick_is_readable_but_not_engageable() {
        // The Codex hybrid-default gate keys on "a non-provider local model config
        // exists" (read_from_home + not-a-provider), NOT on is_engageable(). A
        // Recommended config with no explicit pick is a valid on-device candidate
        // (resolve_start_model can auto-select an already-downloaded curated
        // model), yet is_engageable() is false. Gating on is_engageable() would
        // wrongly skip it. Lock in the two facts the gate relies on.
        let home = temp_home();
        crate::status::write_settings(
            &home,
            &serde_json::json!({ "local_model": { "mode": "recommended" } }),
        )
        .unwrap();

        let cfg = read_from_home(&home).expect("recommended-no-pick config must be readable");
        assert_eq!(cfg.mode, LocalModelMode::Recommended);
        // Not a provider-backed local → the hybrid gate would engage it.
        assert!(crate::providers::provider_from_local_config(&cfg).is_none());
        // But it has no pick, so is_engageable() is false — the old prefilter's bug.
        assert!(!cfg.is_engageable());
    }

    #[test]
    fn render_show_labels_provider_configs() {
        let home = temp_home();
        set_provider_endpoint_in_home(
            &home,
            "lmstudio",
            "http://localhost:1234",
            "qwen2.5-coder-7b",
            "openai_chat",
        )
        .unwrap();
        let rendered = render_show_from_home(&home, Some(true));
        assert!(rendered.contains("Local model: LM Studio"));
        assert!(rendered.contains("Protocol: openai_chat"));
        assert!(rendered.contains("qwen2.5-coder-7b"));
    }
}
