//! `csub claude`: run Claude Code against the running pool's gateway.

use std::ffi::OsString;
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};

use crate::gateway::LAUNCH_PREFIX;

/// Env vars that would take Claude Code off its subscription OAuth login, so
/// its requests would no longer be pool-placed.
const CONFLICTING_ENV: &[&str] = &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"];

/// Checks that a csub daemon for `pool` answers on `port`, then replaces this
/// process with `claude` (spawns and waits where exec is unavailable).
pub async fn claude(pool: &str, port: u16, args: Vec<OsString>) -> anyhow::Result<ExitCode> {
    ensure_daemon(pool, port).await?;
    let base_url = format!("http://127.0.0.1:{port}{LAUNCH_PREFIX}{}", new_launch_id());
    let binary = std::env::var_os("CSUB_CLAUDE_BIN").unwrap_or_else(|| OsString::from("claude"));
    let mut command = Command::new(&binary);
    command.args(&args).env("ANTHROPIC_BASE_URL", &base_url);
    for name in CONFLICTING_ENV {
        command.env_remove(name);
    }
    for name in ["NO_PROXY", "no_proxy"] {
        let mut value = std::env::var_os(name).unwrap_or_default();
        if !value.is_empty() {
            value.push(",");
        }
        value.push("127.0.0.1,localhost");
        command.env(name, value);
    }
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

async fn ensure_daemon(pool: &str, port: u16) -> anyhow::Result<()> {
    let hint = "start it with `csub service install` (or run `csub serve`)";
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
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
            "the csub daemon on 127.0.0.1:{port} serves pool {served:?}, not {pool:?}; pass --port for the daemon of {pool:?}"
        );
    }
    Ok(())
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

    #[tokio::test]
    async fn refuses_to_launch_without_a_csub_daemon() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let error = claude("default", port, Vec::new())
            .await
            .expect_err("no daemon");
        assert!(
            error.to_string().contains("csub service install"),
            "{error}"
        );
    }
}
