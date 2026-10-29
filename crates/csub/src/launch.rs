//! `csub claude`: run Claude Code against the running pool's gateway.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};

use crate::app::Frontend;
use crate::gateway::LAUNCH_PREFIX;

/// Env vars that would take Claude Code off the gateway or its subscription
/// OAuth login (so its requests would no longer be pool-placed), provider
/// credentials it has no use for on the pool, and Rayline router keys it must
/// never see. Names checked against Claude Code 2.1.312.
const WITHHELD_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_UNIX_SOCKET",
    "CLAUDE_CODE_USE_ANTHROPIC_AWS",
    "CLAUDE_CODE_USE_ANTHROPIC_GOOGLE_CLOUD",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_MANTLE",
    "CLAUDE_CODE_USE_VERTEX",
    "ANTHROPIC_AWS_API_KEY",
    "ANTHROPIC_FOUNDRY_API_KEY",
    "ANTHROPIC_FOUNDRY_AUTH_TOKEN",
    "AWS_BEARER_TOKEN_BEDROCK",
    "RAYLINE_ROUTER_API_KEY",
    "RAYLINE_KEY",
];

/// Settings files whose `env` block Claude Code applies over the process
/// environment. The gateway URL is also passed with `--settings`, which wins
/// over user and project settings but not over managed policy.
fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|home| home.join(".claude")))
}

fn settings_files(cwd: &Path) -> Vec<(PathBuf, bool)> {
    let mut files = vec![(
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json"),
        true,
    )];
    if let Some(config_dir) = claude_config_dir() {
        files.push((config_dir.join("settings.json"), false));
    }
    for directory in cwd.ancestors() {
        files.push((directory.join(".claude/settings.json"), false));
        files.push((directory.join(".claude/settings.local.json"), false));
    }
    files
}

/// Refuses a launch whose settings would re-introduce a withheld variable or,
/// from managed policy, override the gateway URL. Names only, never values.
fn check_settings_env(files: &[(PathBuf, bool)]) -> anyhow::Result<()> {
    for (path, managed) in files {
        let managed = *managed;
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(settings) = serde_json::from_str::<serde_json::Value>(&text) else {
            continue;
        };
        let Some(env) = settings.get("env").and_then(|env| env.as_object()) else {
            continue;
        };
        for name in env.keys() {
            let overrides_url = managed && name == "ANTHROPIC_BASE_URL";
            if overrides_url || WITHHELD_ENV.contains(&name.as_str()) {
                bail!(
                    "{} sets {name} in its env block, which would take Claude Code off the subscription pool; remove it or launch without the pool",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn refuse_own_settings_flag(args: &[OsString]) -> anyhow::Result<()> {
    for arg in args {
        let Some(arg) = arg.to_str() else { continue };
        if arg == "--" {
            break;
        }
        if arg == "--settings" || arg.starts_with("--settings=") {
            bail!(
                "--settings cannot be combined with the subscription pool, which passes its own --settings to pin the gateway URL"
            );
        }
    }
    Ok(())
}

/// Checks that a csub daemon for `pool` answers on `port`, then replaces this
/// process with `claude` (spawns and waits where exec is unavailable).
pub async fn claude(
    frontend: &Frontend,
    pool: &str,
    port: u16,
    args: Vec<OsString>,
) -> anyhow::Result<ExitCode> {
    refuse_own_settings_flag(&args)?;
    let cwd = std::env::current_dir().context("read the current directory")?;
    check_settings_env(&settings_files(&cwd))?;
    if crate::supervisor::starts_background_session(&args) {
        if let Some(config_dir) = claude_config_dir() {
            crate::supervisor::check(&config_dir, port).map_err(|message| anyhow!(message))?;
        }
    }
    ensure_daemon(frontend, pool, port).await?;
    let base_url = format!("http://127.0.0.1:{port}{LAUNCH_PREFIX}{}", new_launch_id());
    let settings = serde_json::json!({"env": {"ANTHROPIC_BASE_URL": base_url}}).to_string();
    let binary = std::env::var_os("CSUB_CLAUDE_BIN").unwrap_or_else(|| OsString::from("claude"));
    let mut command = Command::new(&binary);
    command
        .arg("--settings")
        .arg(&settings)
        .args(&args)
        .env("ANTHROPIC_BASE_URL", &base_url);
    for name in WITHHELD_ENV {
        command.env_remove(name);
    }
    let no_proxy = merged_no_proxy(
        std::env::var("NO_PROXY").ok().as_deref(),
        std::env::var("no_proxy").ok().as_deref(),
    );
    command
        .env("NO_PROXY", &no_proxy)
        .env("no_proxy", &no_proxy);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        let error = command.exec();
        Err(anyhow!(error).context(format!("run {}", binary.to_string_lossy())))
    }
    #[cfg(not(unix))]
    {
        let status = command
            .status()
            .with_context(|| format!("run {}", binary.to_string_lossy()))?;
        Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(0, 255) as u8
        ))
    }
}

async fn ensure_daemon(frontend: &Frontend, pool: &str, port: u16) -> anyhow::Result<()> {
    let name = frontend.name;
    let hint = format!("start it with `{name} service install` (or run `{name} serve`)");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        // Loopback only: an inherited HTTP(S)_PROXY must not carry this.
        .no_proxy()
        .build()
        .context("build HTTP client")?
        .get(format!("http://127.0.0.1:{port}/healthz"))
        .send()
        .await
        .map_err(|_| anyhow!("no subscription pool daemon answers on 127.0.0.1:{port}; {hint}"))?;
    let health: serde_json::Value = response
        .json()
        .await
        .map_err(|_| anyhow!("127.0.0.1:{port} is not a csub daemon; {hint}"))?;
    if health.get("service").and_then(|value| value.as_str()) != Some("csub") {
        bail!(
            "127.0.0.1:{port} is not a csub daemon (a legacy rld pool daemon has no gateway); {hint}"
        );
    }
    let served = health
        .get("pool")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    if served != pool {
        bail!(
            "the csub daemon on 127.0.0.1:{port} serves pool {served:?}, not {pool:?}; set CSUB_PORT to the port of the daemon for {pool:?}"
        );
    }
    Ok(())
}

/// Both spellings carry the union of existing exceptions plus loopback, since
/// tools disagree on which one wins.
fn merged_no_proxy(upper: Option<&str>, lower: Option<&str>) -> String {
    let mut entries: Vec<&str> = Vec::new();
    for entry in [upper, lower]
        .into_iter()
        .flatten()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .chain(["127.0.0.1", "localhost"])
    {
        if !entry.is_empty() && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(",")
}

fn new_launch_id() -> String {
    format!(
        "{:016x}{:016x}",
        rand::random::<u64>(),
        rand::random::<u64>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_ids_are_valid_gateway_path_segments() {
        let id = new_launch_id();
        assert_eq!(id.len(), 32);
        let path = format!("{LAUNCH_PREFIX}{id}/v1/messages");
        assert_eq!(
            crate::gateway::split_launch(&path),
            Some((Some(id.as_str()), "/v1/messages"))
        );
        assert_ne!(id, new_launch_id());
    }

    #[test]
    fn no_proxy_keeps_both_spellings_and_adds_loopback() {
        assert_eq!(
            merged_no_proxy(Some("corp.internal"), None),
            "corp.internal,127.0.0.1,localhost"
        );
        assert_eq!(
            merged_no_proxy(Some("a, b"), Some("b,c,localhost")),
            "a,b,c,localhost,127.0.0.1"
        );
        assert_eq!(merged_no_proxy(None, None), "127.0.0.1,localhost");
    }

    #[test]
    fn settings_that_reintroduce_a_withheld_variable_are_refused() {
        let temp = tempfile::tempdir().expect("tempdir");
        let user = temp.path().join("settings.json");
        let managed = temp.path().join("managed-settings.json");
        let files = [(managed.clone(), true), (user.clone(), false)];
        check_settings_env(&files).expect("no settings files");
        // A user/project gateway URL is overridden by our --settings.
        std::fs::write(
            &user,
            r#"{"env":{"ANTHROPIC_BASE_URL":"https://gateway.example","OTHER":"1"}}"#,
        )
        .expect("settings");
        check_settings_env(&files).expect("base URL is pinned by --settings");
        // Managed policy outranks --settings.
        std::fs::write(&managed, r#"{"env":{"ANTHROPIC_BASE_URL":"https://gw"}}"#)
            .expect("managed");
        assert!(check_settings_env(&files).is_err());
        std::fs::remove_file(&managed).expect("rm");
        std::fs::write(&user, r#"{"env":{"ANTHROPIC_API_KEY":"sk-test"}}"#).expect("settings");
        let error = check_settings_env(&files).expect_err("api key");
        assert!(error.to_string().contains("ANTHROPIC_API_KEY"), "{error}");
        assert!(!error.to_string().contains("sk-test"), "{error}");
    }

    #[test]
    fn project_settings_are_found_up_the_tree() {
        let files = settings_files(Path::new("/a/b"));
        assert!(files.contains(&(PathBuf::from("/a/b/.claude/settings.json"), false)));
        assert!(files.contains(&(PathBuf::from("/a/.claude/settings.local.json"), false)));
    }

    #[test]
    fn a_user_settings_flag_is_refused() {
        assert!(refuse_own_settings_flag(&[OsString::from("--settings=x.json")]).is_err());
        assert!(refuse_own_settings_flag(&[OsString::from("--settings")]).is_err());
        refuse_own_settings_flag(&[OsString::from("--model")]).expect("other flags");
    }

    #[tokio::test]
    async fn refuses_to_launch_without_a_csub_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let error = claude(&crate::app::CSUB, "default", port, Vec::new())
            .await
            .expect_err("no daemon");
        assert!(
            error.to_string().contains("csub service install"),
            "{error}"
        );
    }
}
