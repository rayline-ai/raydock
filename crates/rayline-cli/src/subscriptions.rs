mod forecast;

use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal as _, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rayline_subscriptions::{
    AccountRuntimeStatus, ClaimScope, CredentialHealth, CredentialReloadSummary,
    CredentialSourceConfig, LimitClaim, ModelFamily, PoolRuntimeStatus, SUBSCRIPTION_CONFIG_SCHEMA,
    SubscriptionAccountConfig, SubscriptionPoolConfig, SubscriptionPoolRuntime,
    SubscriptionPoolsConfig, SubscriptionRuntimeOptions,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use self::forecast::{DepletionForecast, depletion_forecast};

const DEFAULT_SUBSCRIPTION_METRICS_PORT: u16 = 20816;

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
        } => add(
            account_id,
            pool_id,
            config_path.as_deref(),
            claude_config_dir,
            control_config_dir.as_deref(),
        ),
        SubscriptionCommand::Remove {
            account_id,
            pool_id,
            config_path,
        } => remove(account_id, pool_id, config_path.as_deref()),
        SubscriptionCommand::List { config_path, json } => list(config_path.as_deref(), *json),
        SubscriptionCommand::Status {
            pool_id,
            config_path,
            json,
            verbose,
            live_only,
        } => status(pool_id, config_path.as_deref(), *json, *verbose, *live_only).await,
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
) -> Result<String, String> {
    let (_, pool) = resolve_pool(config_path, pool_id)?;
    let status = match live_pool_status(pool_id).await {
        Some(status) => status,
        None if live_only => {
            return Err(format!(
                "subscription pool {pool_id:?} has no running daemon; start Claude through Rayline or omit --live-only to read a standalone snapshot"
            ));
        }
        None => {
            let runtime = SubscriptionPoolRuntime::start(
                pool_id,
                pool,
                SubscriptionRuntimeOptions::default(),
            )
            .await
            .map_err(|error| error.to_string())?;
            runtime.status_without_live_placement()
        }
    };
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

fn render_verbose_status(status: &PoolRuntimeStatus) -> String {
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
            "Live placement: unavailable (no running pool daemon was found); allowance is a standalone snapshot.\n",
        );
    }
    for account in &status.accounts {
        output.push_str(&format!(
            "\n{}  credential={:?}  usage={}\n",
            account.id,
            account.credential_health,
            if account.usage_snapshot_fresh {
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
        None => output.push_str("Snapshot: standalone allowance · live placement unavailable\n\n"),
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
            text: "exhausted".to_owned(),
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
        text: format!("{:.0}%", remaining * 100.0),
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

fn subscription_metrics_port() -> u16 {
    std::env::var("RAYLINE_SUBSCRIPTION_METRICS_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_SUBSCRIPTION_METRICS_PORT)
}

async fn live_pool_status(pool_id: &str) -> Option<PoolRuntimeStatus> {
    live_pool_status_at(pool_id, subscription_metrics_port()).await
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
    reload_at(
        pool_id,
        config_path,
        subscription_metrics_port(),
        RELOAD_TIMEOUT,
    )
    .await
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
        .send()
        .await
    {
        Ok(response) => response,
        // Nothing is listening, so no daemon holds a credential to correct.
        Err(error) if error.is_connect() => return ReloadOutcome::NoDaemon,
        Err(error) if error.is_timeout() => {
            return ReloadOutcome::Failed(format!(
                "the reload timed out after {timeout:?}; the subscription daemon on 127.0.0.1:{port} may still be reloading, so check `rayline subscriptions status`"
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
            "the subscription daemon on 127.0.0.1:{port} serves pool {served:?}, not {pool_id:?}, and reloaded {served:?} instead; point RAYLINE_SUBSCRIPTION_METRICS_PORT at the daemon for {pool_id:?}"
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
            last_error: None,
        }
    }

    #[test]
    fn compact_status_prioritizes_remaining_capacity_and_availability() {
        let status = PoolRuntimeStatus {
            pool_id: "default".to_owned(),
            accounts: vec![
                status_account("af", 0.0, 1.0, 1.0),
                status_account("mx", 0.81, 0.75, 1.0),
                status_account("ws", 0.03, 0.77, 0.70),
            ],
            placement: None,
        };

        let output = render_compact_status(&status, false);
        assert!(output.contains("Snapshot: standalone allowance"));
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

        let status = live_pool_status_at("default", port)
            .await
            .expect("live pool status");
        let placement = status.placement.expect("live placement");
        assert_eq!(placement.accounts[0].active_launch_leases, 3);
        assert_eq!(placement.accounts[0].active_model_leases["fable"], 1);
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
