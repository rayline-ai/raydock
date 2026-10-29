mod forecast;

use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rayline_subscriptions::{
    AccountRuntimeStatus, ClaimScope, CredentialHealth, CredentialReloadSummary,
    CredentialSourceConfig, LimitClaim, LimitResetGrant, LimitResetProgram, LimitResetStatus,
    ModelFamily, PoolRuntimeStatus, SUBSCRIPTION_CONFIG_SCHEMA, SubscriptionAccountConfig,
    SubscriptionPoolConfig, SubscriptionPoolRuntime, SubscriptionPoolsConfig,
    SubscriptionRuntimeOptions,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use self::forecast::{DepletionForecast, depletion_forecast};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SubscriptionCommand {
    Add {
        account_id: String,
        pool_id: String,
        config_path: Option<PathBuf>,
        claude_config_dir: PathBuf,
        control_config_dir: Option<PathBuf>,
    },
    Remove {
        account_id: String,
        pool_id: String,
        config_path: Option<PathBuf>,
    },
    List {
        config_path: Option<PathBuf>,
        json: bool,
    },
    Status {
        pool_id: String,
        config_path: Option<PathBuf>,
        json: bool,
        verbose: bool,
        live_only: bool,
        poll_usage: bool,
    },
    Reload {
        pool_id: String,
        config_path: Option<PathBuf>,
    },
}

pub async fn run(command: &SubscriptionCommand) -> Result<String, String> {
    match command {
        SubscriptionCommand::Add {
            account_id,
            pool_id,
            config_path,
            claude_config_dir,
            control_config_dir,
        } => with_registry_lock(config_path.as_deref(), || {
            add(
                account_id,
                pool_id,
                config_path.as_deref(),
                claude_config_dir,
                control_config_dir.as_deref(),
            )
        }),
        SubscriptionCommand::Remove {
            account_id,
            pool_id,
            config_path,
        } => with_registry_lock(config_path.as_deref(), || {
            remove(account_id, pool_id, config_path.as_deref())
        }),
        SubscriptionCommand::List { config_path, json } => list(config_path.as_deref(), *json),
        SubscriptionCommand::Status {
            pool_id,
            config_path,
            json,
            verbose,
            live_only,
            poll_usage,
        } => {
            status(
                pool_id,
                config_path.as_deref(),
                *json,
                *verbose,
                *live_only,
                *poll_usage,
            )
            .await
        }
        SubscriptionCommand::Reload {
            pool_id,
            config_path,
        } => reload(pool_id, config_path.as_deref()).await,
    }
}

pub fn default_config_path() -> Result<PathBuf, String> {
    dirs::home_dir()
        .map(|home| home.join(".config/rayline/subscriptions.json"))
        .ok_or_else(|| "home directory not found".to_owned())
}

pub fn load_config(path: Option<&Path>) -> Result<(PathBuf, SubscriptionPoolsConfig), String> {
    let path = path
        .map(Path::to_owned)
        .map_or_else(default_config_path, Ok)?;
    let metadata = fs::symlink_metadata(&path)
        .map_err(|error| format!("read subscription config {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(format!(
            "subscription config {} must be a regular file, not a symlink",
            path.display()
        ));
    }
    let path = path
        .canonicalize()
        .map_err(|error| format!("resolve subscription config {}: {error}", path.display()))?;
    let bytes = fs::read(&path)
        .map_err(|error| format!("read subscription config {}: {error}", path.display()))?;
    if bytes.len() > 1024 * 1024 {
        return Err(format!(
            "subscription config {} exceeds 1 MiB",
            path.display()
        ));
    }
    let config: SubscriptionPoolsConfig = serde_json::from_slice(&bytes)
        .map_err(|error| format!("parse subscription config {}: {error}", path.display()))?;
    config
        .validate()
        .map_err(|error| format!("validate subscription config {}: {error}", path.display()))?;
    Ok((path, config))
}

pub fn resolve_pool(
    path: Option<&Path>,
    pool_id: &str,
) -> Result<(PathBuf, SubscriptionPoolConfig), String> {
    let (path, config) = load_config(path)?;
    let pool = config
        .pools
        .get(pool_id)
        .cloned()
        .ok_or_else(|| format!("subscription pool {pool_id:?} does not exist"))?;
    Ok((path, pool))
}

pub fn resolve_control_config_dir(pool: &SubscriptionPoolConfig) -> Result<PathBuf, String> {
    canonical_directory(&pool.control_config_dir, "control config directory")
}

fn add(
    account_id: &str,
    pool_id: &str,
    config_path: Option<&Path>,
    claude_config_dir: &Path,
    control_config_dir: Option<&Path>,
) -> Result<String, String> {
    let path = config_path
        .map(Path::to_owned)
        .map_or_else(default_config_path, Ok)?;
    let source = canonical_directory(claude_config_dir, "credential source")?;
    let mut config = if path.exists() {
        load_config(Some(&path))?.1
    } else {
        SubscriptionPoolsConfig {
            schema: SUBSCRIPTION_CONFIG_SCHEMA,
            pools: Default::default(),
        }
    };
    let control = match control_config_dir {
        Some(path) => canonical_directory(path, "control config directory")?,
        None if config.pools.contains_key(pool_id) => {
            let existing = &config.pools[pool_id].control_config_dir;
            canonical_directory(existing, "control config directory")?
        }
        None => {
            let default = dirs::home_dir()
                .map(|home| home.join(".claude"))
                .ok_or_else(|| "home directory not found".to_owned())?;
            canonical_directory(&default, "control config directory")?
        }
    };
    let pool = config
        .pools
        .entry(pool_id.to_owned())
        .or_insert_with(|| SubscriptionPoolConfig {
            control_config_dir: control.clone(),
            accounts: Vec::new(),
            policy: Default::default(),
        });
    if pool.control_config_dir != control && control_config_dir.is_some() {
        return Err(format!(
            "subscription pool {pool_id:?} already uses control config directory {}",
            pool.control_config_dir.display()
        ));
    }
    if pool.accounts.iter().any(|account| account.id == account_id) {
        return Err(format!(
            "subscription account {account_id:?} already exists in pool {pool_id:?}"
        ));
    }
    pool.accounts.push(SubscriptionAccountConfig {
        id: account_id.to_owned(),
        credential_source: CredentialSourceConfig {
            claude_config_dir: source.clone(),
        },
    });
    let selected_control = pool.control_config_dir.clone();
    config
        .validate()
        .map_err(|error| format!("validate subscription config: {error}"))?;
    write_config(&path, &config)?;
    Ok(format!(
        "Added subscription account {account_id:?} to pool {pool_id:?} from {}.\n\
         Claude Code will keep using the shared control directory {}.\n",
        source.display(),
        selected_control.display()
    ))
}

fn remove(account_id: &str, pool_id: &str, config_path: Option<&Path>) -> Result<String, String> {
    let (path, mut config) = load_config(config_path)?;
    let pool = config
        .pools
        .get_mut(pool_id)
        .ok_or_else(|| format!("subscription pool {pool_id:?} does not exist"))?;
    if pool.accounts.len() == 1 && pool.accounts[0].id == account_id {
        return Err(
            "cannot remove the last account from a pool; add a replacement first".to_owned(),
        );
    }
    let before = pool.accounts.len();
    pool.accounts.retain(|account| account.id != account_id);
    if before == pool.accounts.len() {
        return Err(format!(
            "subscription account {account_id:?} does not exist in pool {pool_id:?}"
        ));
    }
    write_config(&path, &config)?;
    Ok(format!(
        "Removed subscription account {account_id:?} from pool {pool_id:?}.\n"
    ))
}

fn list(config_path: Option<&Path>, json: bool) -> Result<String, String> {
    let (path, config) = load_config(config_path)?;
    if json {
        return serde_json::to_string_pretty(&config)
            .map(|value| format!("{value}\n"))
            .map_err(|error| format!("encode subscription config: {error}"));
    }
    let mut output = format!("Subscription config: {}\n", path.display());
    for (pool_id, pool) in config.pools {
        output.push_str(&format!(
            "\nPool {pool_id} (control config: {})\n",
            pool.control_config_dir.display()
        ));
        for account in pool.accounts {
            output.push_str(&format!(
                "  {}  {}\n",
                account.id,
                account.credential_source.claude_config_dir.display()
            ));
        }
    }
    Ok(output)
}

async fn status(
    pool_id: &str,
    config_path: Option<&Path>,
    json: bool,
    verbose: bool,
    live_only: bool,
    poll_usage: bool,
) -> Result<String, String> {
    let (_, pool) = resolve_pool(config_path, pool_id)?;
    let status = status_snapshot(
        pool_id,
        pool,
        daemon_port(),
        live_only,
        SubscriptionRuntimeOptions {
            poll_usage_read_only: poll_usage,
            ..SubscriptionRuntimeOptions::default()
        },
    )
    .await?;
    if json {
        return serde_json::to_string_pretty(&status)
            .map(|value| format!("{value}\n"))
            .map_err(|error| format!("encode subscription status: {error}"));
    }
    if verbose {
        return Ok(render_verbose_status(&status));
    }
    let color = io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    Ok(render_compact_status(&status, color))
}

async fn status_snapshot(
    pool_id: &str,
    pool: SubscriptionPoolConfig,
    port: u16,
    live_only: bool,
    mut options: SubscriptionRuntimeOptions,
) -> Result<PoolRuntimeStatus, String> {
    if let Some(status) = live_pool_status_at(pool_id, port).await {
        return Ok(status);
    }
    if live_only {
        return Err(format!(
            "subscription pool {pool_id:?} has no running daemon; start `csub serve` (or `csub service install`) or omit --live-only to read a standalone snapshot"
        ));
    }
    // Standalone status never rotates or persists credentials. Usage GETs with
    // existing valid grants require explicit --poll-usage opt-in.
    options.read_only = true;
    let runtime = SubscriptionPoolRuntime::start(pool_id, pool, options)
        .await
        .map_err(|error| error.to_string())?;
    Ok(runtime.status_without_live_placement())
}

fn render_verbose_status(status: &PoolRuntimeStatus) -> String {
    render_verbose_status_at(status, OffsetDateTime::now_utc())
}

fn render_verbose_status_at(status: &PoolRuntimeStatus, now: OffsetDateTime) -> String {
    let mut output = format!("Subscription pool: {}\n", status.pool_id);
    if let Some(placement) = &status.placement {
        output.push_str(&format!(
            "Live placement (lease TTL {}s):\n",
            placement.active_lease_ttl_seconds
        ));
        for account in &placement.accounts {
            let model_leases = account
                .active_model_leases
                .iter()
                .map(|(model, count)| format!("{model}={count}"))
                .collect::<Vec<_>>()
                .join(",");
            output.push_str(&format!(
                "  {}  launches={}  models={}  primaries={}  overrides={}\n",
                account.id,
                account.active_launch_leases,
                if model_leases.is_empty() {
                    "none"
                } else {
                    &model_leases
                },
                account.primary_assignments,
                account.model_overrides
            ));
        }
    } else {
        output.push_str(
            "Live placement: unavailable (no running pool daemon was found); read-only local credentials, capacity not measured.\n",
        );
    }
    for account in &status.accounts {
        output.push_str(&format!(
            "\n{}  credential={:?}  usage={}\n",
            account.id,
            account.credential_health,
            if account.claims.is_empty() {
                "unknown (not measured)"
            } else if account.usage_snapshot_fresh {
                "fresh"
            } else {
                "stale"
            }
        ));
        if let Some(subscription_type) = &account.subscription_type {
            output.push_str(&format!("  plan: {subscription_type}\n"));
        }
        for claim in &account.claims {
            let utilization = claim
                .utilization
                .map(|value| format!("{:.1}%", value * 100.0))
                .unwrap_or_else(|| "unknown".to_owned());
            let scope = match &claim.scope {
                ClaimScope::Global => "global".to_owned(),
                ClaimScope::Model(model) => format!("model={model}"),
                ClaimScope::Surface(surface) => format!("surface={surface}"),
                ClaimScope::Unknown => "unknown".to_owned(),
            };
            let effective_status = if claim.is_hard_exhausted() {
                "Exhausted".to_owned()
            } else {
                format!("{:?}", claim.status)
            };
            output.push_str(&format!(
                "  {} [{scope}]: {} ({effective_status}) reset={}\n",
                claim.key,
                utilization,
                claim.resets_at.as_deref().unwrap_or("unknown")
            ));
        }
        if let Some(error) = &account.last_error {
            output.push_str(&format!("  warning: {error}\n"));
        }
    }
    output.push_str(&render_saved_limit_resets(status, now));
    output
}

#[derive(Debug, PartialEq, Eq)]
struct SavedResetCounts {
    banked: u64,
    usable_now: Option<u64>,
}

fn reset_inventory_fresh(inventory: &LimitResetStatus, now: OffsetDateTime) -> bool {
    let age_ms = now.unix_timestamp_nanos() / 1_000_000 - i128::from(inventory.checked_at_unix_ms);
    inventory.fresh && (0..=600_000).contains(&age_ms) && !inventory.programs.is_empty()
}

fn reset_grant_unexpired(grant: &LimitResetGrant, now: OffsetDateTime) -> bool {
    grant
        .expires_at
        .as_deref()
        .is_none_or(|expiry| parse_reset_timestamp(expiry).is_some_and(|expiry| expiry > now))
}

fn reset_time_reached(value: Option<&str>, now: OffsetDateTime) -> bool {
    value.is_none_or(|value| parse_reset_timestamp(value).is_some_and(|timestamp| timestamp <= now))
}

fn reset_grant_usable(
    program: &LimitResetProgram,
    grant: &LimitResetGrant,
    now: OffsetDateTime,
) -> bool {
    program.eligible
        && grant.usable_now
        && !grant.paused
        && grant.remaining_count > 0
        && reset_grant_unexpired(grant, now)
        && reset_time_reached(grant.starts_at.as_deref(), now)
        && reset_time_reached(program.cooldown_until.as_deref(), now)
        && reset_time_reached(program.next_available_at.as_deref(), now)
}

fn saved_reset_counts(inventory: &LimitResetStatus, now: OffsetDateTime) -> SavedResetCounts {
    let mut banked = 0_u64;
    let mut usable = 0_u64;
    for program in &inventory.programs {
        for grant in &program.grants {
            if reset_grant_unexpired(grant, now) {
                banked = banked.saturating_add(grant.remaining_count);
            }
            if reset_grant_usable(program, grant, now) {
                usable = usable.saturating_add(grant.remaining_count);
            }
        }
    }
    SavedResetCounts {
        banked,
        usable_now: reset_inventory_fresh(inventory, now).then_some(usable),
    }
}

fn render_saved_limit_resets(status: &PoolRuntimeStatus, now: OffsetDateTime) -> String {
    let mut output =
        String::from("\nSaved limit resets (discretionary offers, not scheduled quota renewal):\n");
    for account in &status.accounts {
        let Some(inventory) = &account.limit_resets else {
            output.push_str(&format!("  {}: unknown (not measured)\n", account.id));
            continue;
        };
        if inventory.programs.is_empty() {
            output.push_str(&format!(
                "  {}: unknown (no evaluated programs)\n",
                account.id
            ));
            continue;
        }
        let counts = saved_reset_counts(inventory, now);
        match counts.usable_now {
            Some(usable) => output.push_str(&format!(
                "  {}: {} banked · {} usable now (OAuth inventory)\n",
                account.id, counts.banked, usable
            )),
            None => output.push_str(&format!(
                "  {}: stale/uncertain · {} last-known banked · usable now unknown\n",
                account.id, counts.banked
            )),
        }
        for program in &inventory.programs {
            output.push_str(&format!(
                "    {}: eligible={}",
                program.program, program.eligible
            ));
            if let Some(reason) = &program.reason {
                output.push_str(&format!(" · reason={reason}"));
                if reason == "surface" {
                    output.push_str(" (web-only: check Claude web Usage)");
                }
            }
            if let Some(until) = &program.cooldown_until {
                output.push_str(&format!(" · cooldown until {until}"));
            }
            if let Some(next) = &program.next_available_at {
                output.push_str(&format!(" · next available {next}"));
            }
            output.push('\n');
            for grant in &program.grants {
                let windows = grant
                    .clears
                    .iter()
                    .map(|window| match window.as_str() {
                        "five_hour" | "session" => "session (5h)",
                        "seven_day" | "weekly" => "weekly (7d)",
                        other => other,
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                output.push_str(&format!(
                    "      {}: remaining={} · resets={} · {}",
                    grant.title,
                    grant.remaining_count,
                    if windows.is_empty() {
                        "unspecified"
                    } else {
                        &windows
                    },
                    if counts.usable_now.is_none() {
                        "usability unknown"
                    } else if reset_grant_usable(program, grant, now) {
                        "usable now"
                    } else {
                        "not usable now"
                    },
                ));
                if let Some(expiry) = &grant.expires_at {
                    output.push_str(&format!(" · expires {expiry}"));
                    if !reset_grant_unexpired(grant, now) {
                        output.push_str(" (expired or invalid expiry; excluded)");
                    }
                }
                if let Some(start) = &grant.starts_at {
                    output.push_str(&format!(" · starts {start}"));
                }
                if grant.paused {
                    output.push_str(" · paused");
                }
                if grant.requires_limit {
                    output.push_str(" · requires hitting a limit");
                }
                output.push('\n');
            }
        }
    }
    output.push_str(
        "  OAuth discovery may not include all web offers. Check/redeem manually at https://claude.ai/settings/usage\n  Sign in to the correct account shown above; redemption is irreversible. Normal weekly renewal is unchanged.\n",
    );
    output
}

#[derive(Clone, Copy)]
enum StatusTone {
    Dim,
    Good,
    Warning,
    Bad,
}

impl StatusTone {
    fn ansi(self) -> &'static str {
        match self {
            Self::Dim => "2",
            Self::Good => "32",
            Self::Warning => "33",
            Self::Bad => "31",
        }
    }
}

struct StatusCell {
    text: String,
    tone: StatusTone,
}

struct CompactStatusRow {
    account: String,
    plan: String,
    five_hour: StatusCell,
    seven_day: StatusCell,
    fable: StatusCell,
    fable_reset: String,
    five_hour_forecast: StatusCell,
    seven_day_forecast: StatusCell,
    fable_forecast: StatusCell,
    availability: StatusCell,
    reset: String,
    active: Option<String>,
}

fn render_compact_status(status: &PoolRuntimeStatus, color: bool) -> String {
    render_compact_status_at(status, color, OffsetDateTime::now_utc())
}

fn render_compact_status_at(
    status: &PoolRuntimeStatus,
    color: bool,
    now: OffsetDateTime,
) -> String {
    let mut output = format!(
        "Subscription pool: {}\n",
        status_paint(&status.pool_id, "1", color)
    );
    match &status.placement {
        Some(placement) => output.push_str(&format!(
            "Snapshot: live allowance + placement · active lease TTL {}\n\n",
            compact_duration(placement.active_lease_ttl_seconds)
        )),
        None => output.push_str("Snapshot: read-only local credentials · capacity not measured · live placement unavailable\n\n"),
    }

    let rows = status
        .accounts
        .iter()
        .map(|account| {
            let active = status.placement.as_ref().map(|placement| {
                placement
                    .accounts
                    .iter()
                    .find(|entry| entry.id == account.id)
                    .map(|entry| entry.active_launch_leases)
                    .unwrap_or_default()
                    .to_string()
            });
            compact_status_row(account, active, now)
        })
        .collect::<Vec<_>>();

    let account_width = rows
        .iter()
        .map(|row| row.account.chars().count())
        .max()
        .unwrap_or_default()
        .max("ACCOUNT".len());
    let plan_width = rows
        .iter()
        .map(|row| row.plan.chars().count())
        .max()
        .unwrap_or_default()
        .max("PLAN".len());
    let availability_width = rows
        .iter()
        .map(|row| row.availability.text.chars().count())
        .max()
        .unwrap_or_default()
        .max("AVAILABLE".len());
    let fable_reset_width = rows
        .iter()
        .map(|row| row.fable_reset.chars().count())
        .max()
        .unwrap_or_default()
        .max("FABLE RESET".len());
    let reset_width = rows
        .iter()
        .map(|row| row.reset.chars().count())
        .max()
        .unwrap_or_default()
        .max("LIMIT RESET".len());

    let reset_header = if status.placement.is_some() {
        format!("{:<reset_width$}", "LIMIT RESET")
    } else {
        "LIMIT RESET".to_owned()
    };
    let header = format!(
        "{:<account_width$}  {:<plan_width$}  {:>9}  {:>9}  {:>11}  {:<fable_reset_width$}  {:<availability_width$}  {reset_header}{}",
        "ACCOUNT",
        "PLAN",
        "5H LEFT",
        "7D LEFT",
        "FABLE LEFT",
        "FABLE RESET",
        "AVAILABLE",
        if status.placement.is_some() {
            "  ACTIVE"
        } else {
            ""
        },
    );
    output.push_str(&status_paint(&header, "1", color));
    output.push('\n');

    for row in &rows {
        output.push_str(&status_paint(
            &format!("{:<account_width$}", row.account),
            "1",
            color,
        ));
        output.push_str("  ");
        output.push_str(&format!("{:<plan_width$}", row.plan));
        output.push_str("  ");
        output.push_str(&render_status_cell(&row.five_hour, 9, true, color));
        output.push_str("  ");
        output.push_str(&render_status_cell(&row.seven_day, 9, true, color));
        output.push_str("  ");
        output.push_str(&render_status_cell(&row.fable, 11, true, color));
        output.push_str("  ");
        output.push_str(&status_paint(
            &format!("{:<fable_reset_width$}", row.fable_reset),
            "2",
            color,
        ));
        output.push_str("  ");
        output.push_str(&render_status_cell(
            &row.availability,
            availability_width,
            false,
            color,
        ));
        output.push_str("  ");
        let rendered_reset = if row.active.is_some() {
            format!("{:<reset_width$}", row.reset)
        } else {
            row.reset.clone()
        };
        output.push_str(&status_paint(&rendered_reset, "2", color));
        if let Some(active) = &row.active {
            output.push_str("  ");
            output.push_str(&format!("{active:>6}"));
        }
        output.push('\n');
    }

    output.push('\n');
    output.push_str(&status_paint(
        "PROJECTED RUN-OUT  current-window average\n",
        "1",
        color,
    ));
    let forecast_account_width = account_width;
    let five_hour_forecast_width =
        forecast_width(rows.iter().map(|row| &row.five_hour_forecast), "5H");
    let seven_day_forecast_width =
        forecast_width(rows.iter().map(|row| &row.seven_day_forecast), "7D");
    let forecast_header = format!(
        "{:<forecast_account_width$}  {:<five_hour_forecast_width$}  {:<seven_day_forecast_width$}  {}",
        "ACCOUNT", "5H", "7D", "FABLE"
    );
    output.push_str(&status_paint(&forecast_header, "1", color));
    output.push('\n');
    for row in &rows {
        output.push_str(&status_paint(
            &format!("{:<forecast_account_width$}", row.account),
            "1",
            color,
        ));
        output.push_str("  ");
        output.push_str(&render_status_cell(
            &row.five_hour_forecast,
            five_hour_forecast_width,
            false,
            color,
        ));
        output.push_str("  ");
        output.push_str(&render_status_cell(
            &row.seven_day_forecast,
            seven_day_forecast_width,
            false,
            color,
        ));
        output.push_str("  ");
        output.push_str(&status_paint(
            &row.fable_forecast.text,
            row.fable_forecast.tone.ansi(),
            color,
        ));
        output.push('\n');
    }
    output.push_str(&status_paint(
        "risk = projected depletion before reset · reset first = renewal wins · learning = too little signal\n\n",
        "2",
        color,
    ));
    output.push_str(&render_saved_limit_resets(status, now));
    output.push('\n');
    output.push_str(&status_paint(
        "Tip: use --verbose to show every normalized claim and placement detail.\n",
        "2",
        color,
    ));
    output
}

fn compact_status_row(
    account: &AccountRuntimeStatus,
    active: Option<String>,
    now: OffsetDateTime,
) -> CompactStatusRow {
    let five_hour = current_claim(account, |claim| {
        claim.key == "five_hour" && claim.scope == ClaimScope::Global
    });
    let seven_day = current_claim(account, |claim| {
        claim.key == "seven_day" && claim.scope == ClaimScope::Global
    });
    let fable_family = ModelFamily::from_display_name("Fable");
    let fable = account
        .claims
        .iter()
        .filter(|claim| {
            !claim.reset_has_passed()
                && matches!(&claim.scope, ClaimScope::Model(model) if model == &fable_family)
        })
        .max_by(|left, right| claim_pressure(left).total_cmp(&claim_pressure(right)));
    let fresh = account.usage_snapshot_fresh;

    CompactStatusRow {
        account: account.id.clone(),
        plan: account
            .subscription_type
            .clone()
            .unwrap_or_else(|| "—".to_owned()),
        five_hour: limit_status_cell(five_hour, "?", fresh),
        seven_day: limit_status_cell(seven_day, "?", fresh),
        fable: limit_status_cell(fable, "—", fresh),
        fable_reset: claim_reset(fable),
        five_hour_forecast: forecast_status_cell(five_hour, fresh, 5 * 60 * 60, now),
        seven_day_forecast: forecast_status_cell(seven_day, fresh, 7 * 24 * 60 * 60, now),
        fable_forecast: forecast_status_cell(fable, fresh, 7 * 24 * 60 * 60, now),
        availability: allowance_status_cell(account, five_hour, seven_day, fable),
        reset: important_reset(five_hour, seven_day, fable),
        active,
    }
}

fn forecast_width<'a>(cells: impl Iterator<Item = &'a StatusCell>, header: &str) -> usize {
    cells
        .map(|cell| cell.text.chars().count())
        .max()
        .unwrap_or_default()
        .max(header.len())
}

fn forecast_status_cell(
    claim: Option<&LimitClaim>,
    fresh: bool,
    window_seconds: i64,
    now: OffsetDateTime,
) -> StatusCell {
    let forecast = depletion_forecast(claim, fresh, window_seconds, now);
    let (text, tone) = match forecast {
        DepletionForecast::Exhausted => ("exhausted".to_owned(), StatusTone::Bad),
        DepletionForecast::NoBurn => ("no burn".to_owned(), StatusTone::Good),
        DepletionForecast::ResetFirst => ("reset first".to_owned(), StatusTone::Good),
        DepletionForecast::RunsOutAt(timestamp) => {
            let timestamp = timestamp.to_offset(time::UtcOffset::UTC);
            (
                format!(
                    "risk ~{} {:02} {:02}Z",
                    month_abbreviation(timestamp.month()),
                    timestamp.day(),
                    timestamp.hour()
                ),
                StatusTone::Warning,
            )
        }
        DepletionForecast::Learning => ("learning".to_owned(), StatusTone::Dim),
        DepletionForecast::Stale => ("stale".to_owned(), StatusTone::Dim),
        DepletionForecast::Unavailable => ("—".to_owned(), StatusTone::Dim),
    };
    StatusCell { text, tone }
}

fn current_claim(
    account: &AccountRuntimeStatus,
    predicate: impl Fn(&LimitClaim) -> bool,
) -> Option<&LimitClaim> {
    account
        .claims
        .iter()
        .find(|claim| !claim.reset_has_passed() && predicate(claim))
}

fn claim_pressure(claim: &LimitClaim) -> f64 {
    if claim.is_hard_exhausted() {
        2.0
    } else {
        claim.utilization.unwrap_or(-1.0)
    }
}

fn limit_status_cell(claim: Option<&LimitClaim>, missing: &str, fresh: bool) -> StatusCell {
    let Some(claim) = claim else {
        return StatusCell {
            text: missing.to_owned(),
            tone: StatusTone::Dim,
        };
    };
    if claim.is_hard_exhausted() {
        return StatusCell {
            text: if fresh {
                "exhausted"
            } else {
                "exhausted stale"
            }
            .to_owned(),
            tone: if fresh {
                StatusTone::Bad
            } else {
                StatusTone::Dim
            },
        };
    }
    let Some(utilization) = claim.utilization else {
        return StatusCell {
            text: "?".to_owned(),
            tone: StatusTone::Dim,
        };
    };
    let remaining = 1.0 - utilization.clamp(0.0, 1.0);
    let tone = if !fresh {
        StatusTone::Dim
    } else if remaining <= 0.1 {
        StatusTone::Bad
    } else if remaining <= 0.3 {
        StatusTone::Warning
    } else {
        StatusTone::Good
    };
    StatusCell {
        text: format!(
            "{:.0}%{}",
            remaining * 100.0,
            if fresh { "" } else { " stale" }
        ),
        tone,
    }
}

fn allowance_status_cell(
    account: &AccountRuntimeStatus,
    five_hour: Option<&LimitClaim>,
    seven_day: Option<&LimitClaim>,
    fable: Option<&LimitClaim>,
) -> StatusCell {
    let (text, tone) = match account.credential_health {
        CredentialHealth::Refreshing => ("refreshing", StatusTone::Warning),
        CredentialHealth::Unavailable => ("credential unavailable", StatusTone::Bad),
        CredentialHealth::Quarantined => ("quarantined", StatusTone::Bad),
        CredentialHealth::Healthy if account.claims.is_empty() => {
            ("usage unknown", StatusTone::Warning)
        }
        CredentialHealth::Healthy if !account.usage_snapshot_fresh => {
            ("usage stale", StatusTone::Warning)
        }
        CredentialHealth::Healthy if !account.complete_global_snapshot => {
            ("usage incomplete", StatusTone::Warning)
        }
        CredentialHealth::Healthy
            if [five_hour, seven_day]
                .into_iter()
                .flatten()
                .any(LimitClaim::is_hard_exhausted) =>
        {
            ("none", StatusTone::Bad)
        }
        CredentialHealth::Healthy
            if [five_hour, seven_day]
                .into_iter()
                .any(|claim| claim.is_none_or(|claim| claim.utilization.is_none())) =>
        {
            ("usage unknown", StatusTone::Warning)
        }
        CredentialHealth::Healthy if fable.is_some_and(LimitClaim::is_hard_exhausted) => {
            ("non-Fable", StatusTone::Warning)
        }
        CredentialHealth::Healthy => ("all", StatusTone::Good),
    };
    StatusCell {
        text: text.to_owned(),
        tone,
    }
}

fn important_reset(
    five_hour: Option<&LimitClaim>,
    seven_day: Option<&LimitClaim>,
    fable: Option<&LimitClaim>,
) -> String {
    let claims = [("5h", five_hour), ("7d", seven_day), ("Fable", fable)]
        .into_iter()
        .filter_map(|(label, claim)| claim.map(|claim| (label, claim)))
        .collect::<Vec<_>>();
    let exhausted = claims
        .iter()
        .copied()
        .filter(|(_, claim)| claim.is_hard_exhausted())
        .collect::<Vec<_>>();
    let selected = if exhausted.is_empty() {
        claims.into_iter().max_by(|(_, left), (_, right)| {
            left.utilization
                .unwrap_or(-1.0)
                .total_cmp(&right.utilization.unwrap_or(-1.0))
        })
    } else {
        exhausted.into_iter().min_by_key(|(_, claim)| {
            claim
                .resets_at
                .as_deref()
                .and_then(parse_reset_timestamp)
                .map(|timestamp| timestamp.unix_timestamp())
                .unwrap_or(i64::MAX)
        })
    };
    let Some((label, claim)) = selected else {
        return "—".to_owned();
    };
    let Some(timestamp) = claim.resets_at.as_deref().and_then(parse_reset_timestamp) else {
        return format!("{label} unknown");
    };
    let timestamp = timestamp.to_offset(time::UtcOffset::UTC);
    format!(
        "{label} {} {:02} {:02}:{:02}Z",
        month_abbreviation(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute()
    )
}

fn claim_reset(claim: Option<&LimitClaim>) -> String {
    let Some(claim) = claim else {
        return "—".to_owned();
    };
    let Some(timestamp) = claim.resets_at.as_deref().and_then(parse_reset_timestamp) else {
        return "unknown".to_owned();
    };
    let timestamp = timestamp.to_offset(time::UtcOffset::UTC);
    format!(
        "{} {:02} {:02}:{:02}Z",
        month_abbreviation(timestamp.month()),
        timestamp.day(),
        timestamp.hour(),
        timestamp.minute()
    )
}

fn parse_reset_timestamp(value: &str) -> Option<OffsetDateTime> {
    let value = value.trim();
    if let Ok(timestamp) = value.parse::<i64>() {
        let seconds = if timestamp.unsigned_abs() >= 100_000_000_000 {
            timestamp / 1000
        } else {
            timestamp
        };
        return OffsetDateTime::from_unix_timestamp(seconds).ok();
    }
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

fn month_abbreviation(month: time::Month) -> &'static str {
    match month {
        time::Month::January => "Jan",
        time::Month::February => "Feb",
        time::Month::March => "Mar",
        time::Month::April => "Apr",
        time::Month::May => "May",
        time::Month::June => "Jun",
        time::Month::July => "Jul",
        time::Month::August => "Aug",
        time::Month::September => "Sep",
        time::Month::October => "Oct",
        time::Month::November => "Nov",
        time::Month::December => "Dec",
    }
}

fn compact_duration(seconds: u64) -> String {
    if seconds.is_multiple_of(60 * 60) {
        format!("{}h", seconds / (60 * 60))
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

fn render_status_cell(cell: &StatusCell, width: usize, right: bool, color: bool) -> String {
    let padded = if right {
        format!("{:>width$}", cell.text)
    } else {
        format!("{:<width$}", cell.text)
    };
    status_paint(&padded, cell.tone.ansi(), color)
}

fn status_paint(text: &str, ansi: &str, color: bool) -> String {
    if color {
        format!("\x1b[{ansi}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// The port of the running daemon, which serves the status and reload API on
/// its one listener: `CSUB_PORT`, as `csub serve` reads it, then the legacy
/// Rayline variable, then the serve default.
fn daemon_port() -> u16 {
    ["CSUB_PORT", "RAYLINE_SUBSCRIPTION_METRICS_PORT"]
        .iter()
        .find_map(|name| std::env::var(name).ok()?.parse::<u16>().ok())
        .unwrap_or(crate::serve::DEFAULT_PORT)
}

async fn live_pool_status_at(pool_id: &str, port: u16) -> Option<PoolRuntimeStatus> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(300))
        .build()
        .ok()?;
    let response = client
        .get(format!("http://127.0.0.1:{port}/v1/subscriptions/status"))
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let status = response.json::<PoolRuntimeStatus>().await.ok()?;
    (status.pool_id == pool_id).then_some(status)
}

/// Asks the running pool daemon to re-read every credential source, so a
/// profile the user signed in to again is adopted without restarting Claude
/// Code. Credential work stays in the daemon; this command only reports it.
async fn reload(pool_id: &str, config_path: Option<&Path>) -> Result<String, String> {
    reload_at(pool_id, config_path, daemon_port(), RELOAD_TIMEOUT).await
}

async fn reload_at(
    pool_id: &str,
    config_path: Option<&Path>,
    port: u16,
    timeout: Duration,
) -> Result<String, String> {
    // Reject a pool the user never registered before reporting on a daemon.
    resolve_pool(config_path, pool_id)?;
    match reload_pool_credentials_at(pool_id, port, timeout).await {
        ReloadOutcome::Reloaded(summary) => Ok(render_reload_summary(&summary)),
        // No daemon means no in-memory credential to correct: the next launch
        // reads the sources anyway, so the user's goal already holds.
        ReloadOutcome::NoDaemon => Ok(format!(
            "no running subscription daemon for pool {pool_id:?}; credentials are read fresh at the next launch\n"
        )),
        ReloadOutcome::Failed(error) => Err(error),
    }
}

/// A reload performs OAuth and usage round-trips for every account, so it needs
/// a far longer budget than the status snapshot's 300ms read.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// What the daemon did with the reload. Only `NoDaemon` may read as success:
/// every other outcome leaves a running daemon still holding the credential the
/// user asked it to replace, so reporting "no daemon" there would be false and
/// would hide that the pool is still broken.
enum ReloadOutcome {
    Reloaded(CredentialReloadSummary),
    NoDaemon,
    Failed(String),
}

async fn reload_pool_credentials_at(pool_id: &str, port: u16, timeout: Duration) -> ReloadOutcome {
    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(error) => return ReloadOutcome::Failed(format!("build the reload client: {error}")),
    };
    let response = match client
        .post(format!("http://127.0.0.1:{port}/v1/subscriptions/reload"))
        // Required by the daemon, and set explicitly: reqwest sends no content
        // type at all for a request with no body.
        .header("content-type", "application/json")
        .send()
        .await
    {
        Ok(response) => response,
        // Nothing is listening, so no daemon holds a credential to correct.
        Err(error) if error.is_connect() => return ReloadOutcome::NoDaemon,
        Err(error) if error.is_timeout() => {
            return ReloadOutcome::Failed(format!(
                "the reload timed out after {timeout:?}; the subscription daemon on 127.0.0.1:{port} may still be reloading, so check `csub status`"
            ));
        }
        Err(error) => {
            return ReloadOutcome::Failed(format!(
                "the reload could not reach the subscription daemon on 127.0.0.1:{port}: {error}"
            ));
        }
    };
    let status = response.status();
    if !status.is_success() {
        return ReloadOutcome::Failed(format!(
            "the subscription daemon answered the reload with HTTP {status}; its accounts still hold the credentials they had"
        ));
    }
    let summary = match response.json::<CredentialReloadSummary>().await {
        Ok(summary) => summary,
        Err(error) => {
            return ReloadOutcome::Failed(format!(
                "could not read the reload answer from the subscription daemon: {error}"
            ));
        }
    };
    // The endpoint takes no pool selector, so a daemon serving another pool has
    // already reloaded that one. Say so instead of claiming no daemon exists.
    if summary.pool_id != pool_id {
        let served = &summary.pool_id;
        return ReloadOutcome::Failed(format!(
            "the subscription daemon on 127.0.0.1:{port} serves pool {served:?}, not {pool_id:?}, and reloaded {served:?} instead; point CSUB_PORT at the daemon for {pool_id:?}"
        ));
    }
    ReloadOutcome::Reloaded(summary)
}

fn render_reload_summary(summary: &CredentialReloadSummary) -> String {
    let mut output = format!("Subscription pool: {}\n", summary.pool_id);
    for account in &summary.accounts {
        output.push_str(&format!(
            "  {}  {:?} → {:?}  {}\n",
            account.id, account.previous_health, account.health, account.detail
        ));
    }
    output
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf, String> {
    let path = expand_home(path)?;
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("{label} {}: {error}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "{label} {} is not a directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn expand_home(path: &Path) -> Result<PathBuf, String> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return dirs::home_dir().ok_or_else(|| "home directory not found".to_owned());
    }
    if let Some(suffix) = raw.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(suffix))
            .ok_or_else(|| "home directory not found".to_owned());
    }
    Ok(path.to_owned())
}

/// Holds an exclusive lock beside the registry across a whole
/// read-modify-write, so concurrent `add`/`remove` runs cannot lose each
/// other's changes. The atomic rename alone only prevents torn files.
fn with_registry_lock(
    config_path: Option<&Path>,
    update: impl FnOnce() -> Result<String, String>,
) -> Result<String, String> {
    use fs2::FileExt as _;
    let path = config_path
        .map(Path::to_owned)
        .map_or_else(default_config_path, Ok)?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("subscription config {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create subscription config directory: {error}"))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("subscription config {} has no file name", path.display()))?;
    let mut lock_name = std::ffi::OsString::from(".");
    lock_name.push(file_name);
    lock_name.push(".lock");
    let lock_path = parent.join(lock_name);
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options
        .open(&lock_path)
        .map_err(|error| format!("open registry lock {}: {error}", lock_path.display()))?;
    lock.lock_exclusive()
        .map_err(|error| format!("lock registry {}: {error}", lock_path.display()))?;
    let result = update();
    let _ = fs2::FileExt::unlock(&lock);
    result
}

fn write_config(path: &Path, config: &SubscriptionPoolsConfig) -> Result<(), String> {
    if path.exists() {
        let metadata = fs::symlink_metadata(path)
            .map_err(|error| format!("inspect subscription config {}: {error}", path.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(format!(
                "subscription config {} must be a regular file, not a symlink",
                path.display()
            ));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("subscription config {} has no parent", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("create subscription config directory: {error}"))?;
    let temporary = parent.join(format!(
        ".subscriptions.json.tmp-{}-{}",
        std::process::id(),
        rand::random::<u64>()
    ));
    let bytes = serde_json::to_vec_pretty(config)
        .map_err(|error| format!("encode subscription config: {error}"))?;
    let write_result = (|| -> io::Result<()> {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result.map_err(|error| format!("write subscription config {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_inventory(now: OffsetDateTime) -> LimitResetStatus {
        LimitResetStatus {
            checked_at_unix_ms: (now.unix_timestamp_nanos() / 1_000_000) as i64,
            fresh: true,
            programs: vec![LimitResetProgram {
                program: "cedar_ember".to_owned(),
                eligible: true,
                reason: None,
                cooldown_until: None,
                next_available_at: None,
                grants: vec![LimitResetGrant {
                    id: "saved-reset".to_owned(),
                    title: "Saved session reset".to_owned(),
                    remaining_count: 2,
                    usable_now: true,
                    requires_limit: false,
                    clears: vec!["five_hour".to_owned(), "seven_day".to_owned()],
                    starts_at: None,
                    expires_at: Some("2030-01-02T12:00:00Z".to_owned()),
                    paused: false,
                }],
            }],
        }
    }

    #[test]
    fn saved_resets_expire_at_the_deadline_not_after_it() {
        let now = OffsetDateTime::parse("2030-01-02T12:00:00Z", &Rfc3339).expect("now");
        let mut inventory = reset_inventory(now);
        assert_eq!(
            saved_reset_counts(&inventory, now),
            SavedResetCounts {
                banked: 0,
                usable_now: Some(0)
            }
        );
        inventory.checked_at_unix_ms -= 1;
        assert_eq!(
            saved_reset_counts(&inventory, now - time::Duration::milliseconds(1)),
            SavedResetCounts {
                banked: 2,
                usable_now: Some(2)
            }
        );
        inventory.programs[0].grants[0].expires_at = Some("invalid".to_owned());
        assert_eq!(saved_reset_counts(&inventory, now).banked, 0);
        assert_eq!(saved_reset_counts(&inventory, now).usable_now, Some(0));
    }

    #[test]
    fn saved_resets_never_claim_usability_for_stale_or_future_inventory() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");
        let mut inventory = reset_inventory(now);
        inventory.checked_at_unix_ms -= 600_000;
        assert_eq!(saved_reset_counts(&inventory, now).usable_now, Some(2));
        inventory.checked_at_unix_ms -= 1;
        assert_eq!(
            saved_reset_counts(&inventory, now),
            SavedResetCounts {
                banked: 2,
                usable_now: None
            }
        );
        inventory.checked_at_unix_ms += 600_002;
        assert_eq!(saved_reset_counts(&inventory, now).usable_now, None);
        inventory.checked_at_unix_ms -= 1;
        inventory.fresh = false;
        assert_eq!(saved_reset_counts(&inventory, now).usable_now, None);

        let mut account = status_account("saved", 0.5, 0.5, 0.5);
        account.limit_resets = Some(inventory);
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![account],
            placement: None,
        };
        for output in [
            render_compact_status_at(&status, false, now),
            render_verbose_status_at(&status, now),
        ] {
            let section = output.split("Saved limit resets").nth(1).expect("section");
            assert!(section.contains("stale/uncertain"));
            assert!(section.contains("usable now unknown"));
            assert!(!section.contains("2 usable now"));
            assert!(section.contains("last-known banked"));
        }
    }

    #[test]
    fn banked_resets_are_not_necessarily_usable() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");
        let inventory = reset_inventory(now);
        assert_eq!(saved_reset_counts(&inventory, now).usable_now, Some(2));
        let mut variants = Vec::new();
        let mut paused = inventory.clone();
        paused.programs[0].grants[0].paused = true;
        variants.push(paused);
        let mut future = inventory.clone();
        future.programs[0].grants[0].starts_at = Some("2030-01-01T13:00:00Z".to_owned());
        variants.push(future);
        let mut cooldown = inventory.clone();
        cooldown.programs[0].cooldown_until = Some("2030-01-01T13:00:00Z".to_owned());
        variants.push(cooldown);
        let mut next = inventory.clone();
        next.programs[0].next_available_at = Some("2030-01-01T13:00:00Z".to_owned());
        variants.push(next);
        let mut unavailable = inventory.clone();
        unavailable.programs[0].grants[0].usable_now = false;
        variants.push(unavailable);
        let mut ineligible = inventory;
        ineligible.programs[0].eligible = false;
        variants.push(ineligible);
        for variant in variants {
            assert_eq!(
                saved_reset_counts(&variant, now),
                SavedResetCounts {
                    banked: 2,
                    usable_now: Some(0)
                }
            );
        }
    }

    #[test]
    fn web_only_resets_stay_actionable_in_both_status_formats() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");
        let mut inventory = reset_inventory(now);
        let program = &mut inventory.programs[0];
        program.eligible = false;
        program.reason = Some("surface".to_owned());
        program.cooldown_until = Some("2030-01-01T13:00:00Z".to_owned());
        program.grants[0].paused = true;
        program.grants[0].requires_limit = true;
        let mut account = status_account("web-account", 0.5, 0.5, 0.5);
        account.limit_resets = Some(inventory);
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![account],
            placement: None,
        };
        for output in [
            render_compact_status_at(&status, false, now),
            render_verbose_status_at(&status, now),
        ] {
            let section = output.split("Saved limit resets").nth(1).expect("section");
            assert!(section.contains("2 banked"));
            assert!(section.contains("0 usable now"));
            assert!(section.contains("web-only"));
            assert!(section.contains("Claude web Usage"));
            assert!(section.contains("https://claude.ai/settings/usage"));
            assert!(section.contains("correct account"));
            assert!(section.contains("session (5h)"));
            assert!(section.contains("weekly (7d)"));
            assert!(section.contains("2030-01-02T12:00:00Z"));
            assert!(section.contains("paused"));
            assert!(section.contains("requires hitting a limit"));
            assert!(section.contains("cooldown"));
        }
    }

    #[test]
    fn old_daemon_status_without_reset_inventory_is_unknown_not_zero() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");
        let account = status_account("old", 0.5, 0.5, 0.5);
        let mut json = serde_json::to_value(account).expect("serialize");
        json.as_object_mut()
            .expect("account object")
            .remove("limit_resets");
        let account: AccountRuntimeStatus =
            serde_json::from_value(json).expect("old daemon account");
        assert!(account.limit_resets.is_none());
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![account],
            placement: None,
        };
        for output in [
            render_compact_status_at(&status, false, now),
            render_verbose_status_at(&status, now),
        ] {
            let section = output.split("Saved limit resets").nth(1).expect("section");
            assert!(section.contains("unknown"));
            assert!(!section.contains("0 banked"));
            assert!(!section.contains("0 usable now"));
        }
        let mut inventory = reset_inventory(now);
        inventory.programs.clear();
        assert_eq!(saved_reset_counts(&inventory, now).usable_now, None);
    }

    #[test]
    fn fresh_saved_resets_report_banked_and_usable_counts_separately() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");
        let mut account = status_account("ready", 0.5, 0.5, 0.5);
        account.limit_resets = Some(reset_inventory(now));
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![account],
            placement: None,
        };
        for output in [
            render_compact_status_at(&status, false, now),
            render_verbose_status_at(&status, now),
        ] {
            let section = output.split("Saved limit resets").nth(1).expect("section");
            assert!(section.contains("2 banked"));
            assert!(section.contains("2 usable now"));
            assert!(section.contains("Saved session reset"));
            assert!(section.contains("discretionary"));
            assert!(section.contains("not scheduled quota renewal"));
        }
    }

    fn claim(key: &str, scope: ClaimScope, utilization: f64, resets_at: &str) -> LimitClaim {
        LimitClaim {
            key: key.to_owned(),
            scope,
            utilization: Some(utilization),
            status: rayline_subscriptions::ClaimStatus::Allowed,
            resets_at: Some(resets_at.to_owned()),
            source: rayline_subscriptions::LimitSource::UsageEndpoint,
        }
    }

    fn status_account(
        id: &str,
        five_hour: f64,
        seven_day: f64,
        fable: f64,
    ) -> AccountRuntimeStatus {
        AccountRuntimeStatus {
            id: id.to_owned(),
            config_dir: PathBuf::from(format!("/private/{id}")),
            credential_health: CredentialHealth::Healthy,
            subscription_type: Some("max".to_owned()),
            usage_snapshot_fresh: true,
            complete_global_snapshot: true,
            claims: vec![
                claim(
                    "five_hour",
                    ClaimScope::Global,
                    five_hour,
                    "2030-01-01T13:40:00Z",
                ),
                claim(
                    "seven_day",
                    ClaimScope::Global,
                    seven_day,
                    "2030-01-02T02:00:00Z",
                ),
                claim(
                    "fable_weekly",
                    ClaimScope::Model(ModelFamily::from_display_name("Fable")),
                    fable,
                    "2030-01-03T21:00:00Z",
                ),
            ],
            extra_usage: rayline_subscriptions::ExtraUsageState::default(),
            limit_resets: None,
            last_error: None,
        }
    }

    #[test]
    fn compact_status_prioritizes_remaining_capacity_and_availability() {
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![
                status_account("af", 0.0, 1.0, 1.0),
                status_account("mx", 0.81, 0.75, 1.0),
                status_account("ws", 0.03, 0.77, 0.70),
            ],
            placement: None,
        };

        let output = render_compact_status(&status, false);
        assert!(output.contains("capacity not measured"));
        assert!(output.contains("5H LEFT"));
        assert!(output.contains("FABLE LEFT"));
        assert!(output.contains("FABLE RESET"));
        assert!(output.contains("AVAILABLE"));
        let account_fields = |id: &str| {
            output
                .lines()
                .find(|line| line.split_whitespace().next() == Some(id))
                .expect("account row")
                .split_whitespace()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            account_fields("af"),
            [
                "af",
                "max",
                "100%",
                "exhausted",
                "exhausted",
                "Jan",
                "03",
                "21:00Z",
                "none",
                "7d",
                "Jan",
                "02",
                "02:00Z",
            ]
        );
        assert_eq!(
            &account_fields("mx")[..5],
            ["mx", "max", "19%", "25%", "exhausted"]
        );
        assert_eq!(
            &account_fields("ws")[..5],
            ["ws", "max", "97%", "23%", "30%"]
        );
        assert_eq!(
            &account_fields("mx")[5..9],
            ["Jan", "03", "21:00Z", "non-Fable"]
        );
        assert_eq!(&account_fields("ws")[5..9], ["Jan", "03", "21:00Z", "all"]);
        assert!(output.contains("7d Jan 02 02:00Z"));
        assert!(output.contains("Fable Jan 03 21:00Z"));
        assert!(!output.contains("credential=Healthy"));
        assert!(!output.contains("session [unknown]"));
        assert!(output.contains("PROJECTED RUN-OUT"));
    }

    #[test]
    fn compact_status_forecasts_only_depletion_before_reset() {
        let now = OffsetDateTime::parse("2030-01-01T12:00:00Z", &Rfc3339).expect("now");
        let mut account = status_account("af", 0.5, 0.25, 0.1);
        account.claims[0].resets_at = Some("2030-01-01T15:00:00Z".to_owned());
        account.claims[1].resets_at = Some("2030-01-07T10:00:00Z".to_owned());
        account.claims[2].resets_at = Some("2030-01-07T10:00:00Z".to_owned());
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![account],
            placement: None,
        };

        let output = render_compact_status_at(&status, false, now);
        assert!(output.contains("risk ~Jan 01 14Z"));
        assert!(output.contains("risk ~Jan 04 18Z"));
        assert!(output.contains("reset first"));
        assert!(output.contains("current-window average"));
    }

    #[test]
    fn fable_reset_distinguishes_missing_claim_from_unknown_reset() {
        assert_eq!(claim_reset(None), "—");
        let mut fable = claim(
            "fable_weekly",
            ClaimScope::Model(ModelFamily::from_display_name("Fable")),
            0.5,
            "2030-01-03T21:00:00Z",
        );
        fable.resets_at = None;
        assert_eq!(claim_reset(Some(&fable)), "unknown");
    }

    #[test]
    fn verbose_status_preserves_normalized_claim_details() {
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![status_account("af", 0.0, 1.0, 1.0)],
            placement: None,
        };

        let output = render_verbose_status(&status);
        assert!(output.contains("af  credential=Healthy  usage=fresh"));
        assert!(output.contains("seven_day [global]: 100.0% (Exhausted)"));
        assert!(output.contains("fable_weekly [model=fable]: 100.0% (Exhausted)"));
        assert!(!output.contains("5H LEFT"));
    }

    #[test]
    fn compact_live_status_adds_active_session_count() {
        let status = PoolRuntimeStatus {
            schema: rayline_subscriptions::POOL_STATUS_SCHEMA,
            pool_id: "default".to_owned(),
            accounts: vec![status_account("ws", 0.03, 0.77, 0.70)],
            placement: Some(rayline_subscriptions::PoolPlacementRuntimeStatus {
                active_lease_ttl_seconds: 900,
                accounts: vec![rayline_subscriptions::AccountPlacementRuntimeStatus {
                    id: "ws".to_owned(),
                    active_launch_leases: 3,
                    active_model_leases: Default::default(),
                    primary_assignments: 3,
                    model_overrides: 0,
                }],
            }),
        };

        let output = render_compact_status(&status, false);
        assert!(output.contains("Snapshot: live allowance + placement · active lease TTL 15m"));
        assert!(output.contains("ACTIVE"));
        assert!(
            output
                .lines()
                .any(|line| line.starts_with("ws") && line.ends_with("     3"))
        );
    }

    #[test]
    fn add_keeps_one_control_dir_and_only_registers_credential_sources() {
        let temp = tempfile::tempdir().expect("tempdir");
        let control = temp.path().join("control");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        fs::create_dir_all(&control).expect("control");
        fs::create_dir_all(&first).expect("first");
        fs::create_dir_all(&second).expect("second");
        let config_path = temp.path().join("subscriptions.json");

        add(
            "first",
            "default",
            Some(&config_path),
            &first,
            Some(&control),
        )
        .expect("add first");
        add("second", "default", Some(&config_path), &second, None).expect("add second");

        let (_, config) = load_config(Some(&config_path)).expect("load");
        let pool = &config.pools["default"];
        assert_eq!(
            pool.control_config_dir,
            control.canonicalize().expect("control canonical")
        );
        assert_eq!(pool.accounts.len(), 2);
        assert_eq!(
            pool.accounts[0].credential_source.claude_config_dir,
            first.canonicalize().expect("first canonical")
        );
        assert_eq!(
            pool.accounts[1].credential_source.claude_config_dir,
            second.canonicalize().expect("second canonical")
        );
    }

    #[test]
    fn concurrent_adds_keep_every_account() {
        let temp = tempfile::tempdir().expect("tempdir");
        let control = temp.path().join("control");
        fs::create_dir_all(&control).expect("control");
        let config_path = temp.path().join("subscriptions.json");
        let threads = (0..8)
            .map(|index| {
                let source = temp.path().join(format!("source-{index}"));
                fs::create_dir_all(&source).expect("source");
                let (config_path, control) = (config_path.clone(), control.clone());
                std::thread::spawn(move || {
                    with_registry_lock(Some(&config_path), || {
                        add(
                            &format!("account-{index}"),
                            "default",
                            Some(&config_path),
                            &source,
                            Some(&control),
                        )
                    })
                    .expect("add")
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("thread");
        }
        let (_, config) = load_config(Some(&config_path)).expect("load");
        assert_eq!(config.pools["default"].accounts.len(), 8);
    }

    #[cfg(unix)]
    #[test]
    fn registry_is_written_with_private_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let control = temp.path().join("control");
        let source = temp.path().join("source");
        fs::create_dir_all(&control).expect("control");
        fs::create_dir_all(&source).expect("source");
        let config_path = temp.path().join("subscriptions.json");
        add(
            "account",
            "default",
            Some(&config_path),
            &source,
            Some(&control),
        )
        .expect("add");

        assert_eq!(
            fs::metadata(&config_path)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[tokio::test]
    async fn live_pool_status_reads_daemon_placement_without_credentials() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind status fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let body = serde_json::to_vec(&serde_json::json!({
            "pool_id": "default",
            "accounts": [{
                "id": "a",
                "config_dir": "/private/profile",
                "credential_health": "healthy",
                "subscription_type": "max",
                "usage_snapshot_fresh": true,
                "complete_global_snapshot": true,
                "claims": [],
                "extra_usage": {"enabled": false, "in_use": false},
                "last_error": null
            }],
            "placement": {
                "active_lease_ttl_seconds": 900,
                "accounts": [{
                    "id": "a",
                    "active_launch_leases": 3,
                    "active_model_leases": {"sonnet": 2, "fable": 1},
                    "primary_assignments": 3,
                    "model_overrides": 1
                }]
            }
        }))
        .expect("status JSON");
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept status request");
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write status headers");
            stream.write_all(&body).await.expect("write status body");
        });

        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");
        let (_, pool) = resolve_pool(Some(&config_path), "default").expect("pool");
        let status = status_snapshot(
            "default",
            pool,
            port,
            true,
            SubscriptionRuntimeOptions::default(),
        )
        .await
        .expect("live pool status without local credentials");
        let placement = status.placement.expect("live placement");
        assert_eq!(placement.accounts[0].active_launch_leases, 3);
        assert_eq!(placement.accounts[0].active_model_leases["fable"], 1);
    }

    #[tokio::test]
    async fn standalone_expired_status_never_contacts_provider_or_mutates_credentials() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");
        let credential_path = temp.path().join("source/.credentials.json");
        let document = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "expired-access",
                "refreshToken": "expired-refresh",
                "expiresAt": 1,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max"
            },
            "unrelated": {"preserve": true}
        }))
        .expect("credential JSON");
        fs::write(&credential_path, &document).expect("write expired credential");
        let modified = fs::metadata(&credential_path)
            .expect("credential metadata")
            .modified()
            .expect("credential modification time");
        let (_, pool) = resolve_pool(Some(&config_path), "default").expect("pool");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind provider fixture");
        let address = listener.local_addr().expect("provider address");
        let endpoint = format!("http://{address}");
        let token_url = format!("https://{address}/oauth/token");
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        // Answer unexpected calls promptly so a regression reports its request
        // count instead of depending on an external service or hanging.
        let provider = tokio::spawn(async move {
            let mut stopped = stopped;
            let mut requests = Vec::new();
            loop {
                tokio::select! {
                    _ = &mut stopped => break,
                    accepted = listener.accept() => {
                        let (mut stream, _) = accepted.expect("accept provider request");
                        let mut request = [0_u8; 4096];
                        let count = stream.read(&mut request).await.expect("read provider request");
                        requests.push(String::from_utf8_lossy(&request[..count]).into_owned());
                        stream.write_all(http_response("500 Internal Server Error", "{}").as_bytes())
                            .await.expect("write provider response");
                    }
                }
            }
            requests
        });
        let options = SubscriptionRuntimeOptions {
            token_url,
            anthropic_base_url: endpoint,
            poll_interval: Duration::from_millis(1),
            near_limit_poll_interval: Duration::from_millis(1),
            home_dir: Some(temp.path().to_owned()),
            ..SubscriptionRuntimeOptions::default()
        };
        let (port, daemon) = fake_daemon(http_response("404 Not Found", "{}")).await;
        let snapshot = status_snapshot("default", pool, port, false, options)
            .await
            .expect("standalone expired status");
        assert!(
            daemon
                .await
                .expect("daemon request")
                .starts_with("GET /v1/subscriptions/status ")
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
        stop.send(()).expect("stop provider fixture");
        assert!(
            provider.await.expect("provider fixture").is_empty(),
            "standalone status must perform zero OAuth grants and zero usage polls"
        );
        assert_eq!(
            fs::read(&credential_path).expect("read credential"),
            document
        );
        assert_eq!(
            fs::metadata(&credential_path)
                .expect("credential metadata")
                .modified()
                .expect("modification time"),
            modified
        );
        assert!(!temp.path().join("control/.credentials.json").exists());
        assert!(snapshot.placement.is_none());
        assert!(snapshot.accounts[0].claims.is_empty());
        assert!(!snapshot.accounts[0].usage_snapshot_fresh);
        assert!(!snapshot.accounts[0].complete_global_snapshot);
        assert!(snapshot.accounts[0].limit_resets.is_none());
        assert_eq!(
            snapshot.accounts[0].credential_health,
            CredentialHealth::Unavailable
        );
        let compact = render_compact_status(&snapshot, false);
        assert!(compact.contains("capacity not measured"));
        assert!(compact.contains("credential unavailable"));
        assert!(!compact.contains("100%"));
        let verbose = render_verbose_status(&snapshot);
        assert!(verbose.contains("unknown (not measured)"));
        assert!(verbose.contains("expired"));
        for output in [&compact, &verbose] {
            let section = output.split("Saved limit resets").nth(1).expect("section");
            assert!(section.contains("unknown"));
            assert!(!section.contains("0 banked"));
            assert!(!section.contains("0 usable now"));
        }
    }

    #[tokio::test]
    async fn live_only_status_does_not_load_local_credentials_without_a_daemon() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");
        let (_, pool) = resolve_pool(Some(&config_path), "default").expect("pool");
        let (port, daemon) = fake_daemon(http_response("404 Not Found", "{}")).await;
        let error = status_snapshot(
            "default",
            pool,
            port,
            true,
            SubscriptionRuntimeOptions::default(),
        )
        .await
        .expect_err("live-only requires a running daemon");
        assert!(error.contains("has no running daemon"));
        assert!(
            daemon
                .await
                .expect("daemon request")
                .starts_with("GET /v1/subscriptions/status ")
        );
        assert!(!temp.path().join("source/.credentials.json").exists());
        assert!(!temp.path().join("control/.credentials.json").exists());
    }

    #[test]
    fn unknown_and_stale_capacity_are_not_presented_as_fresh_measurements() {
        let mut account = status_account("a", 0.25, 0.5, 0.75);
        account.usage_snapshot_fresh = false;
        let stale = limit_status_cell(account.claims.first(), "?", false);
        assert_eq!(stale.text, "75% stale");
        account.usage_snapshot_fresh = true;
        account.claims[0].utilization = None;
        let row = compact_status_row(&account, None, OffsetDateTime::now_utc());
        assert_eq!(row.five_hour.text, "?");
        assert_eq!(row.availability.text, "usage unknown");
        account.claims.clear();
        let row = compact_status_row(&account, None, OffsetDateTime::now_utc());
        assert_eq!(row.five_hour.text, "?");
        assert_eq!(row.seven_day.text, "?");
        assert_eq!(row.availability.text, "usage unknown");
    }

    /// A registry with one pool, pointing at directories that hold no
    /// credential document. `reload` must never read them: the running daemon
    /// owns the credentials.
    fn registry_with_pool(temp: &Path, pool_id: &str) -> PathBuf {
        let control = temp.join("control");
        let source = temp.join("source");
        fs::create_dir_all(&control).expect("control dir");
        fs::create_dir_all(&source).expect("source dir");
        let config_path = temp.join("subscriptions.json");
        add("af", pool_id, Some(&config_path), &source, Some(&control)).expect("add account");
        config_path
    }

    /// A loopback daemon that answers one request with `response_body`, and
    /// hands back the request line it saw.
    async fn fake_daemon(response: String) -> (u16, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind daemon fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let served = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept request");
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).await.expect("read request");
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            String::from_utf8_lossy(&request[..read]).into_owned()
        });
        (port, served)
    }

    /// A well-formed HTTP/1.1 response with `body` as its JSON payload.
    fn http_response(status: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Long enough that a loopback fixture never races it, short enough that a
    /// timeout test stays fast.
    const TEST_TIMEOUT: Duration = Duration::from_millis(500);

    #[tokio::test]
    async fn reload_posts_to_the_daemon_and_renders_every_transition() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");
        let body = serde_json::json!({
            "pool_id": "default",
            "accounts": [
                {
                    "id": "af",
                    "previous_health": "quarantined",
                    "health": "healthy",
                    "detail": "reloaded a new credential from the credential source"
                },
                {
                    "id": "ws",
                    "previous_health": "healthy",
                    "health": "healthy",
                    "detail": "unchanged"
                }
            ]
        })
        .to_string();
        let (port, fixture) = fake_daemon(http_response("200 OK", &body)).await;

        let output = reload_at("default", Some(&config_path), port, TEST_TIMEOUT)
            .await
            .expect("reload output");
        let request = fixture.await.expect("fixture request");

        assert!(
            request.starts_with("POST /v1/subscriptions/reload "),
            "reload must POST the daemon reload path: {request}"
        );
        // The daemon rejects a reload without this header, because a browser
        // cannot send it cross-origin without a preflight the daemon never
        // answers. Relying on a reqwest default would break silently.
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/json"),
            "reload must send the content type the daemon requires: {request}"
        );
        assert!(
            output.contains("af  Quarantined → Healthy  reloaded a new credential"),
            "reload should render the healed account transition: {output}"
        );
        assert!(
            output.contains("ws  Healthy → Healthy  unchanged"),
            "reload should render every account: {output}"
        );
    }

    #[tokio::test]
    async fn reload_without_a_daemon_explains_the_next_launch_reads_credentials() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");

        // Claim a loopback port and release it, so nothing is listening.
        let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind closed port");
        let port = closed.local_addr().expect("closed address").port();
        drop(closed);

        let output = reload_at("default", Some(&config_path), port, TEST_TIMEOUT)
            .await
            .expect("reload output");

        assert_eq!(
            output,
            "no running subscription daemon for pool \"default\"; credentials are read fresh at the next launch\n"
        );
    }

    #[tokio::test]
    async fn reload_rejects_a_pool_that_is_not_registered() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");

        let error = reload_at("missing", Some(&config_path), 1, TEST_TIMEOUT)
            .await
            .expect_err("unknown pool");

        assert!(
            error.contains("subscription pool \"missing\" does not exist"),
            "reload should reject an unknown pool: {error}"
        );
    }

    /// A daemon that is running but cannot reload must never be reported as an
    /// absent daemon: it still holds the credential the user asked to replace.
    #[tokio::test]
    async fn reload_reports_a_daemon_that_answers_with_an_error_status() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");
        let (port, _fixture) = fake_daemon(http_response(
            "503 Service Unavailable",
            "{\"ok\":false,\"error\":\"subscription pool unavailable\"}",
        ))
        .await;

        let error = reload_at("default", Some(&config_path), port, TEST_TIMEOUT)
            .await
            .expect_err("an error status must not read as success");

        assert!(
            error.contains("503"),
            "the message should name the answer the daemon gave: {error}"
        );
        assert!(
            !error.contains("no running subscription daemon"),
            "a running daemon must not be reported as absent: {error}"
        );
    }

    #[tokio::test]
    async fn reload_reports_an_answer_it_cannot_read() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");
        let (port, _fixture) = fake_daemon(http_response("200 OK", "not json at all")).await;

        let error = reload_at("default", Some(&config_path), port, TEST_TIMEOUT)
            .await
            .expect_err("an unreadable answer must not read as success");

        assert!(
            error.contains("could not read the reload answer"),
            "the message should say the answer was unreadable: {error}"
        );
        assert!(
            !error.contains("no running subscription daemon"),
            "a running daemon must not be reported as absent: {error}"
        );
    }

    /// The daemon endpoint takes no pool selector, so a daemon serving another
    /// pool reloads *that* pool. The user must be told which pool was reloaded.
    #[tokio::test]
    async fn reload_reports_a_daemon_serving_another_pool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "work");
        let body = serde_json::json!({
            "pool_id": "default",
            "accounts": [{
                "id": "af",
                "previous_health": "healthy",
                "health": "healthy",
                "detail": "unchanged"
            }]
        })
        .to_string();
        let (port, _fixture) = fake_daemon(http_response("200 OK", &body)).await;

        let error = reload_at("work", Some(&config_path), port, TEST_TIMEOUT)
            .await
            .expect_err("a mismatched pool must not read as success");

        assert!(
            error.contains("\"default\"") && error.contains("\"work\""),
            "the message should name both the served and the requested pool: {error}"
        );
        assert!(
            !error.contains("no running subscription daemon"),
            "a running daemon must not be reported as absent: {error}"
        );
    }

    /// A slow reload across several accounts can outlast the timeout. The
    /// daemon is still there and still holds the old credential.
    #[tokio::test]
    async fn reload_reports_a_timeout_rather_than_an_absent_daemon() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = registry_with_pool(temp.path(), "default");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind silent fixture");
        let port = listener.local_addr().expect("fixture address").port();
        // Accept the connection and never answer, so the client times out.
        let _silent = tokio::spawn(async move {
            let held = listener.accept().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
            drop(held);
        });

        let error = reload_at(
            "default",
            Some(&config_path),
            port,
            Duration::from_millis(120),
        )
        .await
        .expect_err("a timeout must not read as success");

        assert!(
            error.contains("timed out after 120ms"),
            "the message should name the budget it exceeded: {error}"
        );
        assert!(
            !error.contains("no running subscription daemon"),
            "a running daemon must not be reported as absent: {error}"
        );
    }
}
