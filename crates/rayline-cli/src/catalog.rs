//! Curated local-model catalog for the `<cli> local` Recommended picker.
//!
//! The list comes from the published Rayline model registry
//! (`registry.rayline.ai/models.json`) filtered to entries whose `curated` tags
//! include this build's CLI binary name.
//! Hardware fit is computed client-side with the same
//! formula as the desktop's `recommendation.ts`:
//! `baseRamBytes + kvCacheBytesPerToken * context`, green below 70% of total
//! RAM, amber below 90%, red above.
//!
//! Downloads reuse `rayline-hf` — the identical implementation the `rld`
//! daemon uses — so files land in the standard HF hub cache and the daemon
//! sees them as a warm cache on its next start.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::{Value, json};

const REGISTRY_PROD_URL: &str = "https://registry.rayline.ai/models.json";
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// Context budgets mirroring the desktop's `recommendedContextLength`:
/// 64K tokens on <=16 GB machines, 128K above.
const SMALL_RAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const CONTEXT_SMALL: u64 = 65_536;
const CONTEXT_LARGE: u64 = 131_072;

/// Leave ~10% of a budget for the OS / driver / runtime allocator. Applied to
/// discrete-VRAM and CPU-RAM ceilings; the Apple ⅔ figure already bakes in the
/// OS reserve, so it is used as-is.
const MEMORY_HEADROOM_NUM: u64 = 9;
const MEMORY_HEADROOM_DEN: u64 = 10;
/// Apple Silicon caps GPU-wired memory at ~⅔ of unified RAM (the figure the old
/// `fit()` comment cited). Conservative by default; advanced users can raise
/// `iogpu.wired_limit_pct`.
const APPLE_UNIFIED_GPU_NUM: u64 = 2;
const APPLE_UNIFIED_GPU_DEN: u64 = 3;

/// Two derived memory budgets for a machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Budgets {
    /// Accelerator memory the model's working set must fit in.
    ceiling_bytes: u64,
    /// All memory the resident weights can occupy (host + device).
    total_bytes: u64,
}

/// Derive the hot ceiling and total resident budget from detected hardware.
/// `None` when no memory could be detected (no verdict possible).
fn budgets(hw: &rayline_llama::HardwareInfo) -> Option<Budgets> {
    let ram = hw.total_ram_bytes;
    let vram = hw.gpu_vram_bytes;
    if ram == 0 && vram == 0 {
        return None;
    }
    let budgets = match hw.gpu_type.as_str() {
        // Discrete GPU: the binding constraint is VRAM; weights may also spill
        // to host RAM, so total = RAM + VRAM guards loadability.
        "nvidia" if vram > 0 => Budgets {
            ceiling_bytes: vram * MEMORY_HEADROOM_NUM / MEMORY_HEADROOM_DEN,
            total_bytes: ram.saturating_add(vram),
        },
        // Apple Silicon: one unified pool; GPU-wired cap ~⅔ of RAM.
        "apple-silicon" => Budgets {
            ceiling_bytes: ram * APPLE_UNIFIED_GPU_NUM / APPLE_UNIFIED_GPU_DEN,
            total_bytes: ram,
        },
        // CPU-only or an accelerator detection missed: budget against RAM.
        _ => Budgets {
            ceiling_bytes: ram * MEMORY_HEADROOM_NUM / MEMORY_HEADROOM_DEN,
            total_bytes: ram,
        },
    };
    Some(budgets)
}

/// Detect this machine's hardware once per process (cached), mirroring
/// `detect_total_ram`'s `OnceLock`. Returns `None` only if a future detector
/// signals total failure; today `detect_hardware` always returns a value.
pub fn detect_hardware() -> Option<&'static rayline_llama::HardwareInfo> {
    static HARDWARE: std::sync::OnceLock<rayline_llama::HardwareInfo> = std::sync::OnceLock::new();
    Some(HARDWARE.get_or_init(rayline_llama::detect_hardware))
}

/// One curated registry entry (the subset of `ModelEntry` fields this CLI
/// needs; unknown fields are ignored).
#[derive(Clone, Debug, PartialEq)]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    pub repo: String,
    pub filename: String,
    pub revision: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub base_ram_bytes: u64,
    pub kv_cache_bytes_per_token: u64,
    pub max_context_window: u64,
    pub quality_score: u64,
    pub description: String,
    /// Mixture-of-Experts model. Defaults to `false` (dense) when the registry
    /// omits it. Carried for the JSON payload + phase-2 MoE-aware sizing.
    pub is_moe: bool,
    /// Hot weight footprint (non-expert + active experts). Equals
    /// `base_ram_bytes` for dense models. NOT used in the v1 verdict (the daemon
    /// launches `-ngl 99`, so MoE savings can't be realized yet) — phase 2.
    pub active_ram_bytes: u64,
}

/// Hardware fit at the recommended context length, matching the desktop's
/// green/amber/red thresholds (70% / 90% of total RAM).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fit {
    Green,
    Amber,
    Red,
    /// RAM could not be detected on this platform; no verdict.
    Unknown,
}

impl Fit {
    fn as_str(self) -> &'static str {
        match self {
            Self::Green => "green",
            Self::Amber => "amber",
            Self::Red => "red",
            Self::Unknown => "unknown",
        }
    }

    /// Preference order for the auto-pick: comfortable fits first, but a red
    /// fit still ranks (a legacy download on a small machine beats nothing).
    fn rank(self) -> u8 {
        match self {
            Self::Green => 0,
            Self::Amber => 1,
            Self::Unknown => 2,
            Self::Red => 3,
        }
    }
}

/// Fetch the registry catalog and keep only the entries curated for this CLI,
/// smallest first. Entries without registry-provided revision/SHA pins are
/// ignored, because curated downloads must be verifiable without embedding a
/// model allowlist in the binary. Multi-file (sharded) entries are skipped —
/// the download path handles a single GGUF. `RAYLINE_MODELS_REGISTRY_URL`
/// overrides the registry URL (testing against a local/staged catalog).
pub async fn fetch_curated(env_name: &str) -> Vec<CatalogModel> {
    match try_fetch_curated(env_name).await {
        Ok(models) if !models.is_empty() => models,
        _ => fallback_curated(),
    }
}

/// One client per process: reuses the connection pool / TLS session across
/// the catalog fetch and any retries within a command.
fn http_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .build()
            .expect("default reqwest client config is infallible")
    })
}

async fn try_fetch_curated(env_name: &str) -> Result<Vec<CatalogModel>, String> {
    let override_url = std::env::var("RAYLINE_MODELS_REGISTRY_URL").ok();
    let url = override_url.as_deref().unwrap_or(registry_url(env_name));
    let response = http_client()
        .get(url)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(response.status().to_string());
    }
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    Ok(parse_curated(&body))
}

/// There is intentionally no embedded trust fallback: model revision/SHA pins
/// live in the registry so new curated models can be added without a CLI
/// release. If the registry is unavailable or missing pins, the recommended
/// picker shows no download candidates instead of starting an unverified model.
fn fallback_curated() -> Vec<CatalogModel> {
    Vec::new()
}

fn registry_url(_env_name: &str) -> &'static str {
    REGISTRY_PROD_URL
}

fn parse_curated(body: &Value) -> Vec<CatalogModel> {
    let Some(entries) = body.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut models = entries
        .iter()
        .filter(|entry| {
            // Catalog curation tags are a stable wire contract, independent of
            // the public executable name.
            entry
                .get("curated")
                .and_then(Value::as_array)
                .is_some_and(|tags| tags.iter().any(|tag| tag.as_str() == Some("rayline")))
                && entry.get("shardedFilenames").is_none()
        })
        .filter_map(parse_model)
        .collect::<Vec<_>>();
    models.sort_by_key(|model| model.base_ram_bytes);
    models
}

fn parse_model(entry: &Value) -> Option<CatalogModel> {
    let revision = parse_revision(entry)?;
    let sha256 = parse_sha256(entry)?;
    let base_ram_bytes = entry.get("baseRamBytes")?.as_u64()?;
    let is_moe = entry.get("isMoe").and_then(Value::as_bool).unwrap_or(false);
    let active_ram_bytes = entry
        .get("activeRamBytes")
        .and_then(Value::as_u64)
        .unwrap_or(base_ram_bytes);
    Some(CatalogModel {
        id: string_field(entry, "id")?,
        name: string_field(entry, "name")?,
        repo: string_field(entry, "repo")?,
        filename: string_field(entry, "filename")?,
        revision,
        sha256,
        size_bytes: entry.get("sizeBytes")?.as_u64()?,
        base_ram_bytes,
        kv_cache_bytes_per_token: entry.get("kvCacheBytesPerToken")?.as_u64()?,
        max_context_window: entry.get("maxContextWindow")?.as_u64()?,
        quality_score: entry.get("qualityScore")?.as_u64()?,
        description: entry.get("description")?.as_str()?.to_owned(),
        is_moe,
        active_ram_bytes,
    })
}

fn string_field(entry: &Value, field: &str) -> Option<String> {
    let value = entry.get(field)?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_revision(entry: &Value) -> Option<String> {
    let revision = entry
        .get("revision")
        .or_else(|| entry.get("hfRevision"))?
        .as_str()?
        .trim();
    if revision.len() != 40 || !revision.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    Some(revision.to_ascii_lowercase())
}

fn parse_sha256(entry: &Value) -> Option<String> {
    let digest = entry
        .get("sha256")
        .or_else(|| entry.get("digest"))?
        .as_str()?;
    rayline_hf::normalize_sha256(digest).ok()
}

/// RAM the full (dense) weight set + KV cache needs at `context`.
fn required_bytes(model: &CatalogModel, context: u64) -> u64 {
    model
        .base_ram_bytes
        .saturating_add(model.kv_cache_bytes_per_token.saturating_mul(context))
}

/// Context tier for this machine, capped to the model's max window.
fn context_for(budgets: Budgets, model: &CatalogModel) -> u64 {
    let tier = if budgets.total_bytes <= SMALL_RAM_BYTES {
        CONTEXT_SMALL
    } else {
        CONTEXT_LARGE
    };
    tier.min(model.max_context_window)
}

/// Hardware-aware fit verdict. Budgets the model against this machine's
/// accelerator ceiling (discrete VRAM / Apple unified ~⅔ cap / system RAM) with
/// a hard loadability gate against total host+device memory. `Unknown` when
/// hardware is absent or no memory could be detected.
///
/// v1 budgets the full `base_ram_bytes` footprint (the daemon launches with no
/// expert offload, so MoE active-param savings cannot be realized yet — phase 2).
pub fn fit(model: &CatalogModel, hardware: Option<&rayline_llama::HardwareInfo>) -> Fit {
    let Some(hw) = hardware else {
        return Fit::Unknown;
    };
    let Some(budgets) = budgets(hw) else {
        return Fit::Unknown;
    };
    if budgets.ceiling_bytes == 0 {
        return Fit::Red;
    }
    let context = context_for(budgets, model);
    let required = required_bytes(model, context);
    if required > budgets.total_bytes {
        return Fit::Red; // cannot even load into host+device memory
    }
    let ratio = required as f64 / budgets.ceiling_bytes as f64;
    if ratio <= 0.70 {
        Fit::Green
    } else if ratio <= 0.90 {
        Fit::Amber
    } else {
        Fit::Red
    }
}

/// Total physical RAM, detected once per process (the macOS path spawns a
/// `sysctl` subprocess — no reason to repeat it within a command).
/// `None` when detection fails (no fit verdict then).
pub fn detect_total_ram() -> Option<u64> {
    static TOTAL_RAM: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *TOTAL_RAM.get_or_init(detect_total_ram_uncached)
}

fn detect_total_ram_uncached() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(target_os = "linux")]
    {
        let meminfo = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = meminfo
            .lines()
            .find(|line| line.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
    #[cfg(target_os = "windows")]
    {
        #[repr(C)]
        struct MemoryStatusEx {
            length: u32,
            memory_load: u32,
            total_phys: u64,
            avail_phys: u64,
            total_page_file: u64,
            avail_page_file: u64,
            total_virtual: u64,
            avail_virtual: u64,
            avail_extended_virtual: u64,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
        }
        let mut status = MemoryStatusEx {
            length: std::mem::size_of::<MemoryStatusEx>() as u32,
            memory_load: 0,
            total_phys: 0,
            avail_phys: 0,
            total_page_file: 0,
            avail_page_file: 0,
            total_virtual: 0,
            avail_virtual: 0,
            avail_extended_virtual: 0,
        };
        // SAFETY: `status` is a properly initialized MEMORYSTATUSEX with
        // `length` set, as the Win32 API requires.
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 {
            Some(status.total_phys)
        } else {
            None
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

/// Whether the model's GGUF is present in the HF hub cache at its pinned
/// revision and with its expected digest.
pub fn is_downloaded(model: &CatalogModel) -> bool {
    downloaded_path(model).is_some()
}

/// Resolve the model to serve when local routing is enabled but no model has
/// been picked yet: the best already-downloaded curated model, or `None` when
/// nothing is downloaded. Never downloads anything.
pub async fn auto_select_downloaded(env_name: &str) -> Option<CatalogModel> {
    let models = fetch_curated(env_name).await;
    let hardware = detect_hardware();
    let downloaded = models.into_iter().filter(is_downloaded).collect::<Vec<_>>();
    choose_auto_pick(downloaded, hardware)
}

/// Pure recommendation core: rank by (fit, already-downloaded, quality) and
/// return the best. `downloaded_ids` lists models already in the cache, which
/// break a fit tie (prefer not re-downloading). Does NOT filter unfit models —
/// callers decide whether a Red model is acceptable.
fn recommend_from(
    models: Vec<CatalogModel>,
    downloaded_ids: &[String],
    hardware: Option<&rayline_llama::HardwareInfo>,
) -> Option<CatalogModel> {
    models.into_iter().min_by_key(|model| {
        let already_downloaded = downloaded_ids.iter().any(|id| id == &model.id);
        (
            fit(model, hardware).rank(),
            u8::from(!already_downloaded), // downloaded (0) sorts before not (1)
            std::cmp::Reverse(model.quality_score),
        )
    })
}

/// Best curated model to *use* on this machine: the highest-quality model that
/// fits (Green preferred, then Amber), preferring an already-downloaded one.
/// Excludes models that won't fit (Red). `None` when nothing fits or the catalog
/// is empty/unreachable. Never downloads anything.
pub async fn recommend_for_hardware(
    env_name: &str,
    hardware: Option<&rayline_llama::HardwareInfo>,
) -> Option<CatalogModel> {
    let models = fetch_curated(env_name).await;
    let downloaded_ids = models
        .iter()
        .filter(|model| is_downloaded(model))
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    let candidates = models
        .into_iter()
        .filter(|model| fit(model, hardware) != Fit::Red)
        .collect::<Vec<_>>();
    recommend_from(candidates, &downloaded_ids, hardware)
}

/// Best of the already-downloaded curated models: hardware fit first, quality
/// second. Pure so the policy is unit-testable. All downloaded models remain
/// eligible even when Red — a downloaded model beats nothing.
fn choose_auto_pick(
    downloaded: Vec<CatalogModel>,
    hardware: Option<&rayline_llama::HardwareInfo>,
) -> Option<CatalogModel> {
    let ids = downloaded
        .iter()
        .map(|model| model.id.clone())
        .collect::<Vec<_>>();
    recommend_from(downloaded, &ids, hardware)
}

fn downloaded_path(model: &CatalogModel) -> Option<PathBuf> {
    rayline_hf::verified_hf_cache_file(
        &model.repo,
        &model.filename,
        &model.revision,
        Some(&model.sha256),
    )
    .ok()
    .flatten()
}

/// A curated model annotated with this machine's status, for `local models`.
pub struct ModelListing {
    pub model: CatalogModel,
    pub fit: Fit,
    pub downloaded: bool,
    pub selected: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PickerEntry {
    Provider(crate::providers::ProviderModel),
    BuiltIn(CatalogModel),
}

pub fn listings(
    models: Vec<CatalogModel>,
    hardware: Option<&rayline_llama::HardwareInfo>,
    selected_id: Option<&str>,
) -> Vec<ModelListing> {
    models
        .into_iter()
        .map(|model| ModelListing {
            fit: fit(&model, hardware),
            downloaded: is_downloaded(&model),
            selected: selected_id == Some(model.id.as_str()),
            model,
        })
        .collect()
}

/// Machine-readable `local models --json` payload (consumed by the menu bar
/// app). One JSON object on stdout.
pub fn render_listings_json(
    listings: &[ModelListing],
    hardware: Option<&rayline_llama::HardwareInfo>,
) -> String {
    render_listings_json_with_providers(&[], listings, hardware, None)
}

pub fn render_listings_json_with_providers(
    provider_groups: &[(
        crate::providers::ProviderId,
        Vec<crate::providers::ProviderModel>,
    )],
    listings: &[ModelListing],
    hardware: Option<&rayline_llama::HardwareInfo>,
    selected_provider: Option<(&str, &str)>,
) -> String {
    let total_ram_bytes = hardware.map(|hw| hw.total_ram_bytes);
    let provider_count = provider_groups
        .iter()
        .map(|(_, models)| models.len())
        .sum::<usize>();
    let builtin_selections = ordered_selectable(listings)
        .into_iter()
        .enumerate()
        .map(|(index, listing)| (listing.model.id.as_str(), provider_count + index + 1))
        .collect::<Vec<_>>();
    let provider_models = provider_groups
        .iter()
        .flat_map(|(provider, models)| {
            models.iter().map(move |model| {
                let selected =
                    selected_provider.is_some_and(|(selected_provider, selected_model)| {
                        selected_provider == provider.as_str() && selected_model == model.model
                    });
                json!({
                    "id": format!("{}:{}", provider.as_str(), model.model),
                    "source": "provider",
                    "provider": provider.as_str(),
                    "provider_label": provider.label(),
                    "model": model.model,
                    "name": model.model,
                    "size_bytes": model.size_bytes,
                    "selected": selected,
                })
            })
        })
        .enumerate()
        .map(|(index, mut value)| {
            if let Some(object) = value.as_object_mut() {
                object.insert("selection".to_owned(), json!(index + 1));
            }
            value
        })
        .collect::<Vec<_>>();
    let models = listings
        .iter()
        .map(|listing| {
            let selection = builtin_selections
                .iter()
                .find_map(|(id, selection)| (*id == listing.model.id).then_some(*selection));
            json!({
                "id": listing.model.id,
                "source": "builtin",
                "name": listing.model.name,
                "repo": listing.model.repo,
                "filename": listing.model.filename,
                "size_bytes": listing.model.size_bytes,
                "quality_score": listing.model.quality_score,
                "description": listing.model.description,
                "required_ram_bytes": hardware
                    .and_then(budgets)
                    .map(|b| required_bytes(&listing.model, context_for(b, &listing.model))),
                "fit": listing.fit.as_str(),
                "downloaded": listing.downloaded,
                "selected": listing.selected,
                "is_moe": listing.model.is_moe,
                "active_ram_bytes": listing.model.active_ram_bytes,
                "selection": selection,
            })
        })
        .collect::<Vec<_>>();
    let payload = json!({
        "total_ram_bytes": total_ram_bytes,
        "provider_models": provider_models,
        "models": models,
    });
    format!("{payload}\n")
}

/// Compact single-line-per-model listing with 1-based selection numbers.
/// Two sections: installed (downloaded) models first, then available
/// downloads (red-fit entries hidden). `recommended_id` highlights the
/// recommended model with a `★` marker. `color` gates all ANSI output —
/// callers decide based on the target fd and `NO_COLOR`.
pub fn render_listings_human(
    listings: &[ModelListing],
    hardware: Option<&rayline_llama::HardwareInfo>,
    recommended_id: Option<&str>,
    color: bool,
) -> String {
    let cli = crate::CLI_BIN;
    let mut output = String::new();

    // Header
    match hardware.map(|hw| hw.total_ram_bytes) {
        Some(total) => {
            let dim_part = paint(
                &format!(" · this machine: {} RAM", format_bytes(total)),
                "2",
                color,
            );
            output.push_str(&format!("Local models{dim_part}\n"));
        }
        None => output.push_str("Local models\n"),
    }
    output.push('\n');

    // Compute selectable order and max name width for column alignment.
    let selectable = ordered_selectable(listings);
    let max_name_w = selectable
        .iter()
        .map(|l| l.model.name.len())
        .max()
        .unwrap_or(0);
    let max_id_w = selectable
        .iter()
        .map(|l| l.model.id.len())
        .max()
        .unwrap_or(0);

    // Assign 1-based numbers from ordered_selectable.
    let indexed: Vec<(usize, &ModelListing)> = selectable
        .into_iter()
        .enumerate()
        .map(|(i, l)| (i + 1, l))
        .collect();
    let (installed_items, available_items): (Vec<_>, Vec<_>) =
        indexed.iter().partition(|(_, l)| l.downloaded);

    // Render one model line: "  {n:>3}  {name<pad>}  {size:>8}{markers}".
    let render_line = |(n, listing): &(usize, &ModelListing)| -> String {
        let num_str = paint(&format!("{n:>3}"), "36", color);
        let name = &listing.model.name;
        let pad = max_name_w.saturating_sub(name.len());
        let padded_name = format!("{name}{}", " ".repeat(pad));
        let size_str = paint(
            &format!("{:>8}", format_bytes(listing.model.size_bytes)),
            "2",
            color,
        );
        // Model id (dim) — the token for `local use <model-id>` and scripts.
        let id = &listing.model.id;
        let id_pad = max_id_w.saturating_sub(id.len());
        let id_str = paint(&format!("{id}{}", " ".repeat(id_pad)), "2", color);

        let mut markers = String::new();
        if recommended_id == Some(listing.model.id.as_str()) {
            markers.push_str(&format!("  {}", paint("★ recommended", "32", color)));
        }
        if listing.selected {
            markers.push_str(&format!("  {}", paint("● selected", "36", color)));
        }
        match listing.fit {
            Fit::Amber => {
                markers.push_str(&format!("  {}", paint("· tight fit", "33", color)));
            }
            Fit::Red if listing.downloaded => {
                // Red only appears in the Installed section; available filters it out.
                markers.push_str(&format!("  {}", paint("· too large", "31", color)));
            }
            _ => {}
        }

        format!("  {num_str}  {padded_name}  {size_str}  {id_str}{markers}\n")
    };

    // Installed section
    output.push_str(&format!("{}\n", paint("Installed", "2", color)));
    if installed_items.is_empty() {
        output.push_str(&format!("  {}\n", paint("(none yet)", "2", color)));
    } else {
        for item in &installed_items {
            output.push_str(&render_line(item));
        }
    }

    output.push('\n');

    // Available to download section
    output.push_str(&format!("{}\n", paint("Available to download", "2", color)));
    let hidden = listings
        .iter()
        .filter(|l| !l.downloaded && l.fit == Fit::Red)
        .count();
    if available_items.is_empty() {
        output.push_str("  No models suitable to download for this machine.\n");
    } else {
        for item in &available_items {
            output.push_str(&render_line(item));
        }
    }
    if hidden > 0 {
        output.push_str(&format!(
            "  {}\n",
            paint(
                &format!(
                    "({hidden} larger model{s} hidden — too large)",
                    s = if hidden == 1 { "" } else { "s" }
                ),
                "2",
                color
            )
        ));
    }

    // Footer
    output.push('\n');
    let footer = format!(
        "Pick a number to use it (downloads first if needed) — or  {cli} local use <number|id>"
    );
    output.push_str(&format!("{}\n", paint(&footer, "2", color)));

    output
}

pub fn picker_entries(
    provider_groups: &[(
        crate::providers::ProviderId,
        Vec<crate::providers::ProviderModel>,
    )],
    listings: &[ModelListing],
) -> Vec<PickerEntry> {
    provider_groups
        .iter()
        .flat_map(|(_, models)| models.iter().cloned().map(PickerEntry::Provider))
        .chain(
            ordered_selectable(listings)
                .into_iter()
                .map(|listing| PickerEntry::BuiltIn(listing.model.clone())),
        )
        .collect()
}

pub fn resolve_picker_selection(entries: &[PickerEntry], token: &str) -> Option<PickerEntry> {
    if let Ok(index) = token.parse::<usize>() {
        if (1..=entries.len()).contains(&index) {
            return Some(entries[index - 1].clone());
        }
        return None;
    }
    entries
        .iter()
        .find_map(|entry| match entry {
            PickerEntry::BuiltIn(model) if model.id == token => Some(entry.clone()),
            _ => None,
        })
        .or_else(|| {
            entries.iter().find_map(|entry| match entry {
                PickerEntry::Provider(model)
                    if format!("{}:{}", model.provider.as_str(), model.model) == token =>
                {
                    Some(entry.clone())
                }
                _ => None,
            })
        })
        .or_else(|| {
            entries.iter().find_map(|entry| match entry {
                PickerEntry::Provider(model) if model.model == token => Some(entry.clone()),
                _ => None,
            })
        })
}

pub fn render_listings_human_with_providers(
    provider_groups: &[(
        crate::providers::ProviderId,
        Vec<crate::providers::ProviderModel>,
    )],
    listings: &[ModelListing],
    hardware: Option<&rayline_llama::HardwareInfo>,
    recommended_id: Option<&str>,
    selected_provider: Option<(&str, &str)>,
    color: bool,
) -> String {
    if provider_groups.is_empty() {
        return render_listings_human(listings, hardware, recommended_id, color);
    }

    let cli = crate::CLI_BIN;
    let entries = picker_entries(provider_groups, listings);
    let mut output = String::new();
    match hardware.map(|hw| hw.total_ram_bytes) {
        Some(total) => {
            let dim_part = paint(
                &format!(" · this machine: {} RAM", format_bytes(total)),
                "2",
                color,
            );
            output.push_str(&format!("Local models{dim_part}\n"));
        }
        None => output.push_str("Local models\n"),
    }
    output.push('\n');

    let max_name_w = entries
        .iter()
        .map(|entry| match entry {
            PickerEntry::Provider(model) => model.model.len(),
            PickerEntry::BuiltIn(model) => model.name.len(),
        })
        .max()
        .unwrap_or(0);

    let mut next_number = 1usize;
    for (provider, models) in provider_groups {
        output.push_str(&format!("{}\n", paint(provider.label(), "2", color)));
        for model in models {
            let num_str = paint(&format!("{next_number:>3}"), "36", color);
            let pad = max_name_w.saturating_sub(model.model.len());
            let padded_name = format!("{}{}", model.model, " ".repeat(pad));
            let size = model
                .size_bytes
                .map(format_bytes)
                .unwrap_or_else(|| "external".to_owned());
            let size_str = paint(&format!("{size:>8}"), "2", color);
            let mut markers = String::new();
            if selected_provider.is_some_and(|(selected_provider, selected_model)| {
                selected_provider == provider.as_str() && selected_model == model.model
            }) {
                markers.push_str(&format!("  {}", paint("● selected", "36", color)));
            }
            output.push_str(&format!(
                "  {num_str}  {padded_name}  {size_str}{markers}\n"
            ));
            next_number += 1;
        }
        output.push('\n');
    }

    let built_in_entries = ordered_selectable(listings);
    let max_id_w = built_in_entries
        .iter()
        .map(|listing| listing.model.id.len())
        .max()
        .unwrap_or(0);
    let render_builtin_line = |number: usize, listing: &ModelListing| -> String {
        let num_str = paint(&format!("{number:>3}"), "36", color);
        let pad = max_name_w.saturating_sub(listing.model.name.len());
        let padded_name = format!("{}{}", listing.model.name, " ".repeat(pad));
        let size_str = paint(
            &format!("{:>8}", format_bytes(listing.model.size_bytes)),
            "2",
            color,
        );
        let id_pad = max_id_w.saturating_sub(listing.model.id.len());
        let id_str = paint(
            &format!("{}{}", listing.model.id, " ".repeat(id_pad)),
            "2",
            color,
        );
        let mut markers = String::new();
        if recommended_id == Some(listing.model.id.as_str()) {
            markers.push_str(&format!("  {}", paint("★ recommended", "32", color)));
        }
        if listing.selected {
            markers.push_str(&format!("  {}", paint("● selected", "36", color)));
        }
        match listing.fit {
            Fit::Amber => markers.push_str(&format!("  {}", paint("· tight fit", "33", color))),
            Fit::Red if listing.downloaded => {
                markers.push_str(&format!("  {}", paint("· too large", "31", color)))
            }
            _ => {}
        }
        format!("  {num_str}  {padded_name}  {size_str}  {id_str}{markers}\n")
    };

    output.push_str(&format!(
        "{}\n",
        paint("Built-in · managed llama.cpp", "2", color)
    ));
    let indexed_builtins = built_in_entries
        .into_iter()
        .enumerate()
        .map(|(offset, listing)| (next_number + offset, listing))
        .collect::<Vec<_>>();
    let installed_items = indexed_builtins
        .iter()
        .copied()
        .filter(|(_, listing)| listing.downloaded)
        .collect::<Vec<_>>();
    let available_items = indexed_builtins
        .iter()
        .copied()
        .filter(|(_, listing)| !listing.downloaded)
        .collect::<Vec<_>>();
    output.push_str(&format!("{}\n", paint("Installed", "2", color)));
    if installed_items.is_empty() {
        output.push_str(&format!("  {}\n", paint("(none yet)", "2", color)));
    } else {
        for (number, listing) in &installed_items {
            output.push_str(&render_builtin_line(*number, listing));
        }
    }
    output.push('\n');
    output.push_str(&format!("{}\n", paint("Available to download", "2", color)));
    let hidden = listings
        .iter()
        .filter(|listing| !listing.downloaded && listing.fit == Fit::Red)
        .count();
    if available_items.is_empty() {
        output.push_str("  No models suitable to download for this machine.\n");
    } else {
        for (number, listing) in &available_items {
            output.push_str(&render_builtin_line(*number, listing));
        }
    }
    if hidden > 0 {
        output.push_str(&format!(
            "  {}\n",
            paint(
                &format!(
                    "({hidden} larger model{s} hidden — too large)",
                    s = if hidden == 1 { "" } else { "s" }
                ),
                "2",
                color
            )
        ));
    }

    output.push('\n');
    let footer = format!(
        "Pick a number to use it (provider rows configure instantly; built-in rows download first) — or  {cli} local use <number|id>"
    );
    output.push_str(&format!("{}\n", paint(&footer, "2", color)));
    output
}

pub fn format_bytes(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

/// Wrap `text` in the given ANSI SGR code when `on` is true; return the text
/// unchanged when `on` is false (e.g. non-TTY, `NO_COLOR`).
/// Codes used: dim=2, cyan=36, green=32, yellow=33, red=31.
fn paint(text: &str, ansi: &str, on: bool) -> String {
    if on {
        format!("\x1b[{ansi}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// Ordered list of selectable models: installed (downloaded) models first,
/// then available downloads (`!downloaded && fit != Red`), each group in
/// original `listings` order. The 1-based index into this slice is the
/// selection number shown in `render_listings_human`.
pub fn ordered_selectable(listings: &[ModelListing]) -> Vec<&ModelListing> {
    let installed = listings.iter().filter(|l| l.downloaded);
    let available = listings
        .iter()
        .filter(|l| !l.downloaded && l.fit != Fit::Red);
    installed.chain(available).collect()
}

/// Resolve a user-supplied selection token to a catalog model.
/// If `token` parses as a `usize` in range `[1, ordered.len()]`, returns
/// `ordered[n-1].model`; if it parses as a number but is out of range,
/// returns `None`. Otherwise looks up by model id. Returns `None` when
/// neither a valid number nor a known id.
pub fn resolve_selection(ordered: &[&ModelListing], token: &str) -> Option<CatalogModel> {
    if let Ok(n) = token.parse::<usize>() {
        if n >= 1 && n <= ordered.len() {
            return Some(ordered[n - 1].model.clone());
        }
        return None; // parsed as number but out of range
    }
    // Not a number — resolve by model id
    ordered
        .iter()
        .find(|l| l.model.id == token)
        .map(|l| l.model.clone())
}

/// Download `model` into the HF hub cache, reporting progress. With
/// `json` the progress stream is NDJSON on stdout (`download_progress`
/// events mirroring the daemon's `RAYLINE_PROGRESS` payload, then `complete`);
/// otherwise a live progress bar is drawn on stderr. Resumes partial
/// downloads (rayline-hf keeps a `.tmp` + URL sidecar).
pub async fn download(model: &CatalogModel, json: bool) -> Result<PathBuf, String> {
    if let Some(path) = downloaded_path(model) {
        if json {
            emit_json_line(&json!({
                "event": "complete",
                "id": model.id,
                "path": path.display().to_string(),
                "cached": true,
            }));
        } else {
            eprintln!("{} is already downloaded ({}).", model.id, path.display());
        }
        return Ok(path);
    }

    if json {
        emit_json_line(&json!({
            "event": "start",
            "id": model.id,
            "repo": model.repo,
            "filename": model.filename,
            "total": model.size_bytes,
        }));
    } else {
        eprintln!(
            "Downloading {} ({}) from {}…",
            model.id,
            format_bytes(model.size_bytes),
            model.repo
        );
    }

    let repo = model.repo.clone();
    let filename = model.filename.clone();
    let revision = model.revision.clone();
    let sha256 = model.sha256.clone();
    let path = tokio::task::spawn_blocking(move || {
        let callback = |progress: rayline_hf::DownloadProgress| {
            if json {
                emit_json_line(&json!({
                    "event": "download_progress",
                    "stage": progress.stage,
                    "filename": progress.filename,
                    "bytes": progress.bytes_downloaded,
                    "total": progress.total_bytes,
                    "percent": progress.percent,
                }));
            } else {
                render_progress_bar(&progress);
            }
        };
        // Report the real GGUF name in progress events, not the `.tmp` blob.
        rayline_hf::download_to_hf_cache(
            &repo,
            &filename,
            &revision,
            Some(&sha256),
            Some(&callback),
            "model",
            None,
            0,
            0,
            Some(filename.as_str()),
        )
    })
    .await
    .map_err(|error| format!("download task failed: {error}"))??;

    if json {
        emit_json_line(&json!({
            "event": "complete",
            "id": model.id,
            "path": path.display().to_string(),
            "cached": false,
        }));
    } else {
        // Terminate the in-place progress bar line before the summary.
        eprintln!();
        eprintln!("Downloaded to {}.", path.display());
    }
    Ok(path)
}

/// `<cli> local models [--json]`. `color` is determined by the caller based
/// on the output fd and `NO_COLOR`; ignored when `json=true`.
pub async fn models_command(
    env_name: Option<&str>,
    json: bool,
    color: bool,
) -> Result<String, String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let env_name = crate::status::resolve_env(env_name, Some(&home));
    let models = fetch_curated(&env_name).await;
    let hardware = detect_hardware();
    let local_config = crate::local_model::read_from_home(&home);
    // Only a Recommended-mode pick counts as the selected catalog model. In
    // Custom mode `model_id` is still populated (preserved for switching back),
    // but the active selection is the custom endpoint, not this row.
    let selected_id = local_config.as_ref().and_then(|config| {
        (config.mode == crate::local_model::LocalModelMode::Recommended)
            .then(|| config.model_id.clone())
            .flatten()
    });
    let selected_provider = local_config.as_ref().and_then(|config| {
        (config.mode == crate::local_model::LocalModelMode::Custom)
            .then(|| Some((config.provider.clone()?, config.model.clone()?)))
            .flatten()
    });
    let listings = listings(models, hardware, selected_id.as_deref());
    let provider_groups = crate::providers::discover_all().await;
    Ok(if json {
        render_listings_json_with_providers(
            &provider_groups,
            &listings,
            hardware,
            selected_provider
                .as_ref()
                .map(|(provider, model)| (provider.as_str(), model.as_str())),
        )
    } else {
        // Compute recommended_id: same policy as recommend_for_hardware —
        // best non-Red model by (fit, already-downloaded, quality) — pure,
        // no extra network call.
        let downloaded_ids: Vec<String> = listings
            .iter()
            .filter(|l| l.downloaded)
            .map(|l| l.model.id.clone())
            .collect();
        let candidates: Vec<CatalogModel> = listings
            .iter()
            .filter(|l| l.fit != Fit::Red)
            .map(|l| l.model.clone())
            .collect();
        let recommended = recommend_from(candidates, &downloaded_ids, hardware);
        let recommended_id = recommended.as_ref().map(|m| m.id.as_str());
        render_listings_human_with_providers(
            &provider_groups,
            &listings,
            hardware,
            recommended_id,
            selected_provider
                .as_ref()
                .map(|(provider, model)| (provider.as_str(), model.as_str())),
            color,
        )
    })
}

/// `<cli> local download <model-id> [--json]`. One catalog fetch serves both
/// the id lookup and the post-download auto-select.
pub async fn download_command(
    env_name: Option<&str>,
    model_id: &str,
    json: bool,
) -> Result<(), String> {
    let (home, models) = fetch_for_command(env_name).await?;
    let model = find_in(&models, model_id)?;
    download(&model, json).await?;
    auto_select_if_sole_model(&home, &models, &model, json);
    Ok(())
}

/// Resolve home + env and fetch the curated catalog once for a command.
async fn fetch_for_command(
    env_name: Option<&str>,
) -> Result<(std::path::PathBuf, Vec<CatalogModel>), String> {
    let home = dirs::home_dir().ok_or_else(|| "home directory not found".to_owned())?;
    let env_name = crate::status::resolve_env(env_name, Some(&home));
    let models = fetch_curated(&env_name).await;
    Ok((home, models))
}

/// After a successful download, adopt the model as the selection when it is
/// the only added model on this machine (nothing else downloaded, no complete
/// custom endpoint) — a first download shouldn't need a second "select" step.
/// Other Rayline clients can reload config afterwards; unknown NDJSON events are
/// ignored by older consumers.
fn auto_select_if_sole_model(
    home: &std::path::Path,
    models: &[CatalogModel],
    model: &CatalogModel,
    json: bool,
) {
    let cfg = crate::local_model::read_from_home(home);
    let downloaded_ids = models
        .iter()
        .filter(|m| is_downloaded(m))
        .map(|m| m.id.clone())
        .collect::<Vec<_>>();
    if !should_auto_select(cfg.as_ref(), &downloaded_ids, &model.id) {
        return;
    }
    if crate::local_model::set_recommended_in_home(home, model).is_ok() {
        if json {
            emit_json_line(&json!({ "event": "auto_selected", "id": model.id }));
        } else {
            eprintln!("Selected `{}` — your only added model.", model.id);
        }
    }
}

/// `<cli> local remove <model-id>`: delete the model's GGUF from the local
/// cache. Clears the pick when the removed model was the selected one (the
/// selection would otherwise point at a file that no longer exists).
pub async fn remove_command(env_name: Option<&str>, model_id: &str) -> Result<String, String> {
    let (home, models) = fetch_for_command(env_name).await?;
    let model = find_in(&models, model_id)?;
    let Some(path) = downloaded_path(&model) else {
        return Err(format!("`{model_id}` is not downloaded."));
    };
    rayline_hf::delete_model_and_shards_from_hf_cache(&path)?;

    let mut output = format!(
        "Removed {id} ({size} freed). The file came out of the shared Hugging Face cache, so other apps using it lose it too.",
        id = model.id,
        size = format_bytes(model.size_bytes),
    );
    if let Some(cfg) = crate::local_model::read_from_home(&home) {
        if cfg.model_id.as_deref() == Some(model.id.as_str()) {
            crate::local_model::clear_recommended_pick_in_home(&home)
                .map_err(|error| format!("failed to update settings: {error}"))?;
            output.push_str("\nIt was your selected model — the selection has been cleared.");
        }
    }
    // When exactly one added model remains and nothing valid is selected,
    // select it — even a custom endpoint.
    if let Some(note) = ensure_sole_added_model_selected(&models, &home) {
        output.push_str(&note);
    }
    Ok(output)
}

/// When no valid selection exists and the added-models list (downloaded
/// curated models + saved custom endpoints) has exactly one entry, select it.
/// Returns a user-facing note when a selection was made.
fn ensure_sole_added_model_selected(
    models: &[CatalogModel],
    home: &std::path::Path,
) -> Option<String> {
    let cfg = crate::local_model::read_from_home(home);
    if cfg.as_ref().is_some_and(|cfg| cfg.is_engageable()) {
        return None;
    }
    let downloaded = models
        .iter()
        .filter(|m| is_downloaded(m))
        .cloned()
        .collect::<Vec<_>>();
    let endpoints = cfg.map(|cfg| cfg.custom_endpoints).unwrap_or_default();
    match (downloaded.as_slice(), endpoints.as_slice()) {
        ([model], []) => crate::local_model::set_recommended_in_home(home, model)
            .ok()
            .map(|_| format!("\nSelected `{}` — your only added model.", model.id)),
        ([], [endpoint]) => crate::local_model::activate_custom_endpoint_in_home(home, endpoint)
            .ok()
            .map(|_| {
                format!(
                    "\nSelected your custom endpoint `{}` ({}) — your only added model.",
                    endpoint.model, endpoint.base_url,
                )
            }),
        _ => None,
    }
}

/// Pure decision: auto-select only when the just-downloaded model is the sole
/// added model — no other curated download and no saved custom endpoint
/// (`read_from_home` already counts a bare active URL+model pair as one).
fn should_auto_select(
    cfg: Option<&crate::local_model::LocalModelConfig>,
    downloaded_ids: &[String],
    just_downloaded: &str,
) -> bool {
    let endpoints_added = cfg.is_some_and(|cfg| !cfg.custom_endpoints.is_empty());
    !endpoints_added && downloaded_ids == [just_downloaded]
}

/// `<cli> local use <number|model-id>`: resolve by 1-based selection number
/// (from `raydock local models`) or model id, download if missing, then select.
pub async fn use_command(env_name: Option<&str>, selection: &str) -> Result<String, String> {
    let (home, models) = fetch_for_command(env_name).await?;
    let hardware = detect_hardware();
    let listing_vec = listings(models, hardware, None);
    let provider_groups = crate::providers::discover_all().await;
    let entries = picker_entries(&provider_groups, &listing_vec);
    let entry = resolve_picker_selection(&entries, selection).ok_or_else(|| {
        let range = if entries.is_empty() {
            "no models available".to_owned()
        } else {
            format!("1–{}", entries.len())
        };
        let ids = entries
            .iter()
            .map(|entry| match entry {
                PickerEntry::Provider(model) => model.model.as_str(),
                PickerEntry::BuiltIn(model) => model.id.as_str(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        format!("Unknown selection `{selection}`. Valid numbers: {range}. Valid ids: {ids}")
    })?;
    match entry {
        PickerEntry::Provider(model) => {
            let endpoint = crate::providers::provider_endpoint(model.provider)
                .map_err(|error| format!("Could not resolve provider endpoint: {error}"))?
                .ok_or_else(|| "built-in provider has no endpoint".to_owned())?;
            crate::local_model::set_provider_endpoint_in_home(
                &home,
                model.provider.as_str(),
                &endpoint.base_url,
                &model.model,
                "openai_chat",
            )?;
            let cli = crate::CLI_BIN;
            Ok(format!(
                "Local model set to {provider} `{model}` ({url}).\nLocal routing uses it with `{cli} claude --local`.",
                provider = model.provider.label(),
                model = model.model,
                url = endpoint.base_url,
            ))
        }
        PickerEntry::BuiltIn(model) => {
            download(&model, false).await?;
            crate::local_model::set_recommended(&model)?;
            let cli = crate::CLI_BIN;
            Ok(format!(
                "Local model set to {id} ({name}).\nLocal routing uses it once enabled for your account (`{cli} local on`).",
                id = model.id,
                name = model.name,
            ))
        }
    }
}

fn find_in(models: &[CatalogModel], model_id: &str) -> Result<CatalogModel, String> {
    models
        .iter()
        .find(|model| model.id == model_id)
        .cloned()
        .ok_or_else(|| {
            let available = models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            format!("Unknown model id `{model_id}`. Available: {available}")
        })
}

/// Final `error` NDJSON event for `--json` consumers (the menu bar app).
pub fn emit_error_json(message: &str) {
    emit_json_line(&json!({ "event": "error", "message": message }));
}

fn emit_json_line(payload: &Value) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{payload}");
    let _ = stdout.flush();
}

fn render_progress_bar(progress: &rayline_hf::DownloadProgress) {
    const WIDTH: usize = 30;
    let percent = progress.percent.clamp(0.0, 100.0);
    let filled = ((percent / 100.0) * WIDTH as f64).round() as usize;
    let bar = format!("{}{}", "#".repeat(filled), "-".repeat(WIDTH - filled));
    let total = if progress.total_bytes > 0 {
        format_bytes(progress.total_bytes)
    } else {
        "?".to_owned()
    };
    let mut stderr = std::io::stderr().lock();
    let _ = write!(
        stderr,
        "\r  [{bar}] {percent:5.1}%  {downloaded} / {total}   ",
        downloaded = format_bytes(progress.bytes_downloaded),
    );
    let _ = stderr.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hw(
        total_ram_bytes: u64,
        gpu_type: &str,
        gpu_vram_bytes: u64,
    ) -> rayline_llama::HardwareInfo {
        rayline_llama::HardwareInfo {
            total_ram_bytes,
            gpu_type: gpu_type.to_owned(),
            gpu_model: String::new(),
            gpu_vram_bytes,
            os: "test".to_owned(),
            arch: "test".to_owned(),
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;

    #[test]
    fn budgets_apple_unified_caps_at_two_thirds_of_ram() {
        let b = budgets(&hw(24 * GIB, "apple-silicon", 0)).unwrap();
        assert_eq!(b.ceiling_bytes, 24 * GIB * 2 / 3);
        assert_eq!(b.total_bytes, 24 * GIB);
    }

    #[test]
    fn budgets_discrete_gpu_uses_vram_ceiling_and_summed_total() {
        let b = budgets(&hw(32 * GIB, "nvidia", 8 * GIB)).unwrap();
        assert_eq!(b.ceiling_bytes, 8 * GIB * 9 / 10); // 10% headroom
        assert_eq!(b.total_bytes, 32 * GIB + 8 * GIB);
    }

    #[test]
    fn budgets_cpu_only_uses_ram_with_headroom() {
        let b = budgets(&hw(32 * GIB, "none", 0)).unwrap();
        assert_eq!(b.ceiling_bytes, 32 * GIB * 9 / 10);
        assert_eq!(b.total_bytes, 32 * GIB);
    }

    #[test]
    fn budgets_unknown_when_no_memory_detected() {
        assert!(budgets(&hw(0, "none", 0)).is_none());
    }

    fn registry_model(id: &str, base_ram_bytes: u64) -> Value {
        json!({
            "id": id,
            "name": id,
            "repo": "example/repo",
            "filename": "model.gguf",
            "revision": "ffffffffffffffffffffffffffffffffffffffff",
            "sha256": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "sizeBytes": 1,
            "baseRamBytes": base_ram_bytes,
            "kvCacheBytesPerToken": 1,
            "maxContextWindow": 1024,
            "qualityScore": 1,
            "description": "test model",
            "curated": ["rayline"],
        })
    }

    #[test]
    fn parse_curated_trusts_pinned_registry_curated_entries() {
        let body = json!({
            "models": [
                registry_model("qwen2.5-coder-7b-q5km", 1),
                registry_model("qwen3.6-27b-q4km", 2),
            ],
        });

        let models = parse_curated(&body);

        assert_eq!(
            models
                .iter()
                .map(|model| model.id.as_str())
                .collect::<Vec<_>>(),
            vec!["qwen2.5-coder-7b-q5km", "qwen3.6-27b-q4km"]
        );
    }

    #[test]
    fn parse_curated_uses_registry_revision_and_sha_for_dynamic_model() {
        let body = json!({ "models": [registry_model("qwen3.6-27b-new", 2)] });

        let models = parse_curated(&body);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3.6-27b-new");
        assert_eq!(models[0].repo, "example/repo");
        assert_eq!(models[0].filename, "model.gguf");
        assert_eq!(
            models[0].revision,
            "ffffffffffffffffffffffffffffffffffffffff"
        );
        assert_eq!(
            models[0].sha256,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn parse_curated_rejects_entry_without_registry_revision_or_sha() {
        let mut model = registry_model("qwen3.6-27b-q4km", 2);
        let object = model.as_object_mut().unwrap();
        object.remove("revision");
        object.remove("sha256");
        let body = json!({ "models": [model] });

        let models = parse_curated(&body);

        assert!(models.is_empty());
    }

    #[test]
    fn parse_curated_rejects_invalid_registry_revision_or_sha() {
        let mut bad_revision = registry_model("qwen3.6-27b-bad-revision", 1);
        bad_revision
            .as_object_mut()
            .unwrap()
            .insert("revision".to_owned(), Value::String("main".to_owned()));
        let mut bad_sha = registry_model("qwen3.6-27b-bad-sha", 2);
        bad_sha
            .as_object_mut()
            .unwrap()
            .insert("sha256".to_owned(), Value::String("bad".to_owned()));
        let body = json!({ "models": [bad_revision, bad_sha] });

        let models = parse_curated(&body);

        assert!(models.is_empty());
    }

    #[test]
    fn parse_curated_accepts_prefixed_registry_sha_and_revision_alias() {
        let mut model = registry_model("qwen3.6-27b-q4km", 2);
        let object = model.as_object_mut().unwrap();
        object.remove("revision");
        object.insert(
            "hfRevision".to_owned(),
            Value::String("FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF".to_owned()),
        );
        object.insert(
            "sha256".to_owned(),
            Value::String(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
            ),
        );
        let body = json!({ "models": [model] });

        let models = parse_curated(&body);

        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].revision,
            "ffffffffffffffffffffffffffffffffffffffff"
        );
        assert_eq!(
            models[0].sha256,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        );
    }

    #[test]
    fn parse_curated_accepts_new_curated_model_when_registry_supplies_pins() {
        let mut model = registry_model("qwen3.5-9b-q4km", 2);
        model
            .as_object_mut()
            .unwrap()
            .insert("filename".to_owned(), Value::String("new.gguf".to_owned()));
        let body = json!({ "models": [model] });

        let models = parse_curated(&body);

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3.5-9b-q4km");
        assert_eq!(models[0].filename, "new.gguf");
    }

    #[test]
    fn parse_curated_defaults_moe_fields_to_dense() {
        let body = json!({ "models": [registry_model("dense-7b", 7)] });
        let models = parse_curated(&body);
        assert_eq!(models.len(), 1);
        assert!(!models[0].is_moe);
        assert_eq!(models[0].active_ram_bytes, models[0].base_ram_bytes);
    }

    #[test]
    fn parse_curated_reads_moe_fields_when_present() {
        let mut entry = registry_model("moe-35b-a3b", 35);
        let object = entry.as_object_mut().unwrap();
        object.insert("isMoe".to_owned(), json!(true));
        object.insert("activeRamBytes".to_owned(), json!(3u64));
        let body = json!({ "models": [entry] });
        let models = parse_curated(&body);
        assert_eq!(models.len(), 1);
        assert!(models[0].is_moe);
        assert_eq!(models[0].active_ram_bytes, 3);
    }

    fn fit_model(
        base_ram_bytes: u64,
        kv_per_token: u64,
        max_ctx: u64,
        quality: u64,
    ) -> CatalogModel {
        CatalogModel {
            id: "m".to_owned(),
            name: "m".to_owned(),
            repo: "r".to_owned(),
            filename: "f.gguf".to_owned(),
            revision: "f".repeat(40),
            sha256: "b".repeat(64),
            size_bytes: base_ram_bytes,
            base_ram_bytes,
            kv_cache_bytes_per_token: kv_per_token,
            max_context_window: max_ctx,
            quality_score: quality,
            description: "d".to_owned(),
            is_moe: false,
            active_ram_bytes: base_ram_bytes,
        }
    }

    #[test]
    fn fit_apple_unified_red_when_over_two_thirds_even_if_under_total_ram() {
        // 20 GB model on a 24 GiB Mac: ~78% of total RAM (old verdict = Green),
        // but > the ~16 GiB GPU-wired ceiling → Red. Regression for the false-green OOM.
        let model = fit_model(20 * GIB, 0, 4096, 50);
        assert_eq!(
            fit(&model, Some(&hw(24 * GIB, "apple-silicon", 0))),
            Fit::Red
        );
    }

    #[test]
    fn fit_discrete_gpu_red_when_over_vram_despite_ample_ram() {
        let model = fit_model(12 * GIB, 0, 4096, 50);
        assert_eq!(
            fit(&model, Some(&hw(64 * GIB, "nvidia", 8 * GIB))),
            Fit::Red
        );
    }

    #[test]
    fn fit_cpu_only_green_when_comfortably_under_ram() {
        let model = fit_model(10 * GIB, 0, 4096, 50);
        assert_eq!(fit(&model, Some(&hw(64 * GIB, "none", 0))), Fit::Green);
    }

    #[test]
    fn fit_red_when_total_footprint_exceeds_total_memory() {
        let model = fit_model(100 * GIB, 0, 4096, 50);
        assert_eq!(fit(&model, Some(&hw(16 * GIB, "none", 0))), Fit::Red);
    }

    #[test]
    fn fit_unknown_without_hardware() {
        let model = fit_model(GIB, 0, 4096, 50);
        assert_eq!(fit(&model, None), Fit::Unknown);
        assert_eq!(fit(&model, Some(&hw(0, "none", 0))), Fit::Unknown);
    }

    #[test]
    fn recommend_from_prefers_fit_then_downloaded_then_quality() {
        let hardware = hw(64 * GIB, "none", 0);
        let small_hi = {
            let mut m = fit_model(8 * GIB, 0, 4096, 90);
            m.id = "small-hi".into();
            m
        };
        let small_lo = {
            let mut m = fit_model(8 * GIB, 0, 4096, 50);
            m.id = "small-lo".into();
            m
        };
        let huge = {
            let mut m = fit_model(100 * GIB, 0, 4096, 99);
            m.id = "huge".into();
            m
        };
        // Nothing downloaded → among the two green fits, higher quality wins; the
        // red `huge` ranks last despite top quality.
        let pick = recommend_from(
            vec![huge.clone(), small_lo.clone(), small_hi.clone()],
            &[],
            Some(&hardware),
        );
        assert_eq!(pick.unwrap().id, "small-hi");
    }

    #[test]
    fn recommend_from_prefers_already_downloaded_on_a_fit_tie() {
        let hardware = hw(64 * GIB, "none", 0);
        let a = {
            let mut m = fit_model(8 * GIB, 0, 4096, 70);
            m.id = "a".into();
            m
        };
        let b = {
            let mut m = fit_model(8 * GIB, 0, 4096, 70);
            m.id = "b".into();
            m
        };
        // Same fit + quality; `b` is downloaded → preferred (avoids a download).
        let pick = recommend_from(vec![a, b], &["b".to_owned()], Some(&hardware));
        assert_eq!(pick.unwrap().id, "b");
    }

    // ── Compact listing (render_listings_human) ───────────────────────────

    fn test_listing(
        id: &str,
        name: &str,
        size_bytes: u64,
        fit: Fit,
        downloaded: bool,
        selected: bool,
    ) -> ModelListing {
        ModelListing {
            model: CatalogModel {
                id: id.to_owned(),
                name: name.to_owned(),
                repo: "r".to_owned(),
                filename: "f.gguf".to_owned(),
                revision: "f".repeat(40),
                sha256: "b".repeat(64),
                size_bytes,
                base_ram_bytes: size_bytes,
                kv_cache_bytes_per_token: 0,
                max_context_window: 4096,
                quality_score: 50,
                description: "test description — should not appear in human output".to_owned(),
                is_moe: false,
                active_ram_bytes: size_bytes,
            },
            fit,
            downloaded,
            selected,
        }
    }

    #[test]
    fn render_listings_human_single_line_per_model_no_description() {
        let listings = vec![
            test_listing(
                "model-a",
                "Model Alpha",
                4_500_000_000,
                Fit::Green,
                true,
                false,
            ),
            test_listing(
                "model-b",
                "Model Beta",
                7_200_000_000,
                Fit::Green,
                false,
                false,
            ),
        ];
        let output = render_listings_human(&listings, None, None, false);

        // Each model has exactly one output line that contains its name AND size.
        let line_a = output
            .lines()
            .find(|l| l.contains("Model Alpha"))
            .expect("Model Alpha line");
        assert!(
            line_a.contains("4.5 GB"),
            "size must be on same line as name"
        );

        let line_b = output
            .lines()
            .find(|l| l.contains("Model Beta"))
            .expect("Model Beta line");
        assert!(
            line_b.contains("7.2 GB"),
            "size must be on same line as name"
        );

        // The model id is shown on the same line (the token for
        // `local use <model-id>` and scripts).
        assert!(
            line_a.contains("model-a"),
            "id must be on same line as name"
        );
        assert!(
            line_b.contains("model-b"),
            "id must be on same line as name"
        );

        // Exactly one line per model (no duplicated rows).
        assert_eq!(
            output.lines().filter(|l| l.contains("Model Alpha")).count(),
            1,
            "exactly one line per model"
        );

        // Selection numbers are present on the same lines.
        assert!(line_a.contains("  1  ") || line_a.trim_start().starts_with("1 "));
        assert!(line_b.contains("  2  ") || line_b.trim_start().starts_with("2 "));

        // No description text in human output.
        assert!(
            !output.contains("test description"),
            "descriptions must not appear in human listing"
        );

        // Footer references <number|id>.
        assert!(
            output.contains("<number|id>"),
            "footer must mention number|id selection"
        );
    }

    #[test]
    fn render_listings_human_recommended_marker() {
        let listings = vec![
            test_listing("model-a", "Model A", 4_000_000_000, Fit::Green, true, false),
            test_listing(
                "model-b",
                "Model B",
                8_000_000_000,
                Fit::Green,
                false,
                false,
            ),
        ];
        let output = render_listings_human(&listings, None, Some("model-a"), false);

        let line_a = output.lines().find(|l| l.contains("Model A")).unwrap();
        assert!(
            line_a.contains("★ recommended"),
            "recommended marker on model-a"
        );
        assert!(
            !line_a.contains("● selected"),
            "no selected marker on model-a"
        );

        let line_b = output.lines().find(|l| l.contains("Model B")).unwrap();
        assert!(
            !line_b.contains("★ recommended"),
            "no recommended marker on model-b"
        );
    }

    #[test]
    fn render_listings_human_with_providers_numbers_provider_rows_first() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            false,
            false,
        )];
        let providers = vec![(
            crate::providers::ProviderId::Ollama,
            vec![crate::providers::ProviderModel {
                provider: crate::providers::ProviderId::Ollama,
                model: "qwen3-coder:30b".to_owned(),
                size_bytes: Some(18_000_000_000),
            }],
        )];

        let output = render_listings_human_with_providers(
            &providers,
            &listings,
            None,
            None,
            Some(("ollama", "qwen3-coder:30b")),
            false,
        );

        let provider_line = output
            .lines()
            .find(|line| line.contains("qwen3-coder:30b"))
            .unwrap();
        let builtin_line = output
            .lines()
            .find(|line| line.contains("Model A"))
            .unwrap();
        assert!(provider_line.contains("  1  ") || provider_line.trim_start().starts_with("1 "));
        assert!(provider_line.contains("● selected"));
        assert!(builtin_line.contains("  2  ") || builtin_line.trim_start().starts_with("2 "));
    }

    #[test]
    fn render_listings_json_includes_provider_models() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            false,
            false,
        )];
        let providers = vec![(
            crate::providers::ProviderId::Ollama,
            vec![crate::providers::ProviderModel {
                provider: crate::providers::ProviderId::Ollama,
                model: "qwen3-coder:30b".to_owned(),
                size_bytes: Some(18_000_000_000),
            }],
        )];

        let output = render_listings_json_with_providers(
            &providers,
            &listings,
            None,
            Some(("ollama", "qwen3-coder:30b")),
        );
        let value: Value = serde_json::from_str(&output).unwrap();
        let provider = &value["provider_models"][0];
        assert_eq!(provider["provider"], "ollama");
        assert_eq!(provider["model"], "qwen3-coder:30b");
        assert_eq!(provider["selection"], 1);
        assert_eq!(provider["selected"], true);
        assert_eq!(value["models"][0]["id"], "model-a");
        assert_eq!(value["models"][0]["selection"], 2);
    }

    #[test]
    fn resolve_picker_selection_maps_numbers_and_ids() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            false,
            false,
        )];
        let providers = vec![(
            crate::providers::ProviderId::LmStudio,
            vec![crate::providers::ProviderModel {
                provider: crate::providers::ProviderId::LmStudio,
                model: "qwen2.5-coder-7b".to_owned(),
                size_bytes: None,
            }],
        )];
        let entries = picker_entries(&providers, &listings);

        assert!(matches!(
            resolve_picker_selection(&entries, "1"),
            Some(PickerEntry::Provider(model)) if model.model == "qwen2.5-coder-7b"
        ));
        assert!(matches!(
            resolve_picker_selection(&entries, "2"),
            Some(PickerEntry::BuiltIn(model)) if model.id == "model-a"
        ));
        assert!(matches!(
            resolve_picker_selection(&entries, "qwen2.5-coder-7b"),
            Some(PickerEntry::Provider(_))
        ));
        assert!(matches!(
            resolve_picker_selection(&entries, "lmstudio:qwen2.5-coder-7b"),
            Some(PickerEntry::Provider(model))
                if model.provider == crate::providers::ProviderId::LmStudio
                    && model.model == "qwen2.5-coder-7b"
        ));
        assert!(matches!(
            resolve_picker_selection(&entries, "model-a"),
            Some(PickerEntry::BuiltIn(_))
        ));
    }

    #[test]
    fn resolve_picker_selection_disambiguates_provider_qualified_ids() {
        let providers = vec![
            (
                crate::providers::ProviderId::Ollama,
                vec![crate::providers::ProviderModel {
                    provider: crate::providers::ProviderId::Ollama,
                    model: "shared-model".to_owned(),
                    size_bytes: None,
                }],
            ),
            (
                crate::providers::ProviderId::LmStudio,
                vec![crate::providers::ProviderModel {
                    provider: crate::providers::ProviderId::LmStudio,
                    model: "shared-model".to_owned(),
                    size_bytes: None,
                }],
            ),
        ];
        let entries = picker_entries(&providers, &[]);

        assert!(matches!(
            resolve_picker_selection(&entries, "lmstudio:shared-model"),
            Some(PickerEntry::Provider(model))
                if model.provider == crate::providers::ProviderId::LmStudio
        ));
        assert!(matches!(
            resolve_picker_selection(&entries, "ollama:shared-model"),
            Some(PickerEntry::Provider(model))
                if model.provider == crate::providers::ProviderId::Ollama
        ));
    }

    #[test]
    fn resolve_picker_selection_prefers_builtin_id_over_provider_model_name() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            false,
            false,
        )];
        let providers = vec![(
            crate::providers::ProviderId::LmStudio,
            vec![crate::providers::ProviderModel {
                provider: crate::providers::ProviderId::LmStudio,
                model: "model-a".to_owned(),
                size_bytes: None,
            }],
        )];
        let entries = picker_entries(&providers, &listings);

        assert!(matches!(
            resolve_picker_selection(&entries, "1"),
            Some(PickerEntry::Provider(model)) if model.model == "model-a"
        ));
        assert!(matches!(
            resolve_picker_selection(&entries, "model-a"),
            Some(PickerEntry::BuiltIn(model)) if model.id == "model-a"
        ));
    }

    #[test]
    fn render_listings_human_selected_marker() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            true,
            true,
        )];
        let output = render_listings_human(&listings, None, None, false);
        let line_a = output.lines().find(|l| l.contains("Model A")).unwrap();
        assert!(line_a.contains("● selected"), "selected marker present");
    }

    #[test]
    fn render_listings_human_color_true_contains_ansi() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            true,
            false,
        )];
        let output = render_listings_human(&listings, None, None, true);
        assert!(
            output.contains("\x1b["),
            "color=true must emit ANSI escapes"
        );
    }

    #[test]
    fn render_listings_human_color_false_no_ansi() {
        let listings = vec![test_listing(
            "model-a",
            "Model A",
            4_000_000_000,
            Fit::Green,
            true,
            false,
        )];
        let output = render_listings_human(&listings, None, None, false);
        assert!(
            !output.contains("\x1b["),
            "color=false must not emit ANSI escapes"
        );
    }

    // ── ordered_selectable ────────────────────────────────────────────────

    #[test]
    fn ordered_selectable_installed_first_then_available_red_excluded() {
        let listings = vec![
            test_listing("avail-green", "Avail Green", 1, Fit::Green, false, false),
            test_listing("installed", "Installed", 1, Fit::Amber, true, false),
            test_listing("red-unavail", "Red", 1, Fit::Red, false, false),
        ];
        let sel = ordered_selectable(&listings);
        assert_eq!(sel.len(), 2, "red unavailable excluded");
        assert_eq!(sel[0].model.id, "installed", "installed first");
        assert_eq!(sel[1].model.id, "avail-green", "available second");
    }

    #[test]
    fn ordered_selectable_downloaded_red_still_included() {
        // A downloaded model with Red fit must still appear (it's on disk).
        let listings = vec![test_listing(
            "red-inst",
            "Red Inst",
            1,
            Fit::Red,
            true,
            false,
        )];
        let sel = ordered_selectable(&listings);
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].model.id, "red-inst");
    }

    // ── resolve_selection ─────────────────────────────────────────────────

    fn sel_listings() -> Vec<ModelListing> {
        vec![
            test_listing("model-a", "Model A", 1, Fit::Green, true, false),
            test_listing("model-b", "Model B", 1, Fit::Green, false, false),
        ]
    }

    #[test]
    fn resolve_selection_by_number_returns_correct_model() {
        let ls = sel_listings();
        let sel = ordered_selectable(&ls);
        assert_eq!(resolve_selection(&sel, "1").unwrap().id, "model-a");
        assert_eq!(resolve_selection(&sel, "2").unwrap().id, "model-b");
    }

    #[test]
    fn resolve_selection_by_id_returns_correct_model() {
        let ls = sel_listings();
        let sel = ordered_selectable(&ls);
        assert_eq!(resolve_selection(&sel, "model-b").unwrap().id, "model-b");
        assert_eq!(resolve_selection(&sel, "model-a").unwrap().id, "model-a");
    }

    #[test]
    fn resolve_selection_out_of_range_number_returns_none() {
        let ls = sel_listings();
        let sel = ordered_selectable(&ls);
        assert!(resolve_selection(&sel, "0").is_none(), "0 is out of range");
        assert!(
            resolve_selection(&sel, "99").is_none(),
            "99 is out of range"
        );
    }

    #[test]
    fn resolve_selection_unknown_id_returns_none() {
        let ls = sel_listings();
        let sel = ordered_selectable(&ls);
        assert!(resolve_selection(&sel, "no-such-model").is_none());
    }
}
