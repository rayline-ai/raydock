use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use rayline_subscriptions::{
    ClaimScope, CredentialSourceConfig, SUBSCRIPTION_CONFIG_SCHEMA, SubscriptionAccountConfig,
    SubscriptionPoolConfig, SubscriptionPoolRuntime, SubscriptionPoolsConfig,
    SubscriptionRuntimeOptions,
};

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
        } => status(pool_id, config_path.as_deref(), *json).await,
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
    let control = match control_config_dir {
        Some(path) => canonical_directory(path, "control config directory")?,
        None => {
            let default = dirs::home_dir()
                .map(|home| home.join(".claude"))
                .ok_or_else(|| "home directory not found".to_owned())?;
            canonical_directory(&default, "control config directory")?
        }
    };
    let mut config = if path.exists() {
        load_config(Some(&path))?.1
    } else {
        SubscriptionPoolsConfig {
            schema: SUBSCRIPTION_CONFIG_SCHEMA,
            pools: Default::default(),
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

async fn status(pool_id: &str, config_path: Option<&Path>, json: bool) -> Result<String, String> {
    let (_, pool) = resolve_pool(config_path, pool_id)?;
    let runtime =
        SubscriptionPoolRuntime::start(pool_id, pool, SubscriptionRuntimeOptions::default())
            .await
            .map_err(|error| error.to_string())?;
    let status = runtime.status();
    if json {
        return serde_json::to_string_pretty(&status)
            .map(|value| format!("{value}\n"))
            .map_err(|error| format!("encode subscription status: {error}"));
    }
    let mut output = format!("Subscription pool: {}\n", status.pool_id);
    for account in status.accounts {
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
        if let Some(subscription_type) = account.subscription_type {
            output.push_str(&format!("  plan: {subscription_type}\n"));
        }
        for claim in account.claims {
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
        if let Some(error) = account.last_error {
            output.push_str(&format!("  warning: {error}\n"));
        }
    }
    Ok(output)
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
}
