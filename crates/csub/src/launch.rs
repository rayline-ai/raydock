//! `csub claude`: run Claude Code against the running pool's gateway.

use std::ffi::OsString;
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};

use crate::app::Frontend;
use crate::gateway::LAUNCH_PREFIX;

/// Env vars that would take Claude Code off its subscription OAuth login (so
/// its requests would no longer be pool-placed), and Rayline router keys that
/// Claude Code must never see.
const WITHHELD_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "CLAUDE_CODE_USE_BEDROCK",
    "CLAUDE_CODE_USE_FOUNDRY",
    "CLAUDE_CODE_USE_VERTEX",
    "RAYLINE_ROUTER_API_KEY",
    "RAYLINE_KEY",
];

/// Checks that a csub daemon for `pool` answers on `port`, then replaces this
/// process with `claude` (spawns and waits where exec is unavailable).
pub async fn claude(
    frontend: &Frontend,
    pool: &str,
    port: u16,
    args: Vec<OsString>,
) -> anyhow::Result<ExitCode> {
    ensure_daemon(frontend, pool, port).await?;
    let base_url = format!("http://127.0.0.1:{port}{LAUNCH_PREFIX}{}", new_launch_id());
    let binary = std::env::var_os("CSUB_CLAUDE_BIN").unwrap_or_else(|| OsString::from("claude"));
    let mut command = Command::new(&binary);
    command.args(&args).env("ANTHROPIC_BASE_URL", &base_url);
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
