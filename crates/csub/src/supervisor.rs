//! Background sessions (`--bg`, `claude agents`) run under Claude Code's
//! supervisor daemon, which keeps the environment it started with. A pool
//! launch may only hand work to one that already talks to a csub gateway.

use std::ffi::OsString;
use std::path::Path;

/// `--bg`/`--background` or `claude agents` hand work to the supervisor; a
/// foreground session never touches it.
pub fn starts_background_session(args: &[OsString]) -> bool {
    args.first().and_then(|arg| arg.to_str()) == Some("agents")
        || args
            .iter()
            .filter_map(|arg| arg.to_str())
            .any(|arg| arg == "--bg" || arg == "--background")
}

/// Refuses when a live supervisor for `config_dir` is not on the csub gateway
/// on `port` (or its environment cannot be inspected).
pub fn check(config_dir: &Path, port: u16) -> Result<(), String> {
    if agent_view_disabled() {
        return Ok(());
    }
    let Some(pid) = running_supervisor(config_dir) else {
        return Ok(());
    };
    verdict(pid, read_base_url(pid), port)
}

fn verdict(pid: u32, base_url: Option<Option<String>>, port: u16) -> Result<(), String> {
    let gateway = format!("http://127.0.0.1:{port}/l/");
    let why = match base_url {
        Some(Some(url)) if url.starts_with(&gateway) => return Ok(()),
        Some(_) => "that is not on the subscription pool",
        None => "whose environment cannot be inspected",
    };
    Err(format!(
        "a Claude Code background supervisor (pid {pid}) {why} is running; background sessions from this launch would reuse it. Stop it with `claude daemon stop --any` and launch again."
    ))
}

fn agent_view_disabled() -> bool {
    std::env::var("CLAUDE_CODE_DISABLE_AGENT_VIEW").is_ok_and(|value| {
        !matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no"
        )
    })
}

fn running_supervisor(config_dir: &Path) -> Option<u32> {
    let raw = std::fs::read_to_string(config_dir.join("daemon.lock")).ok()?;
    let lock = serde_json::from_str::<serde_json::Value>(&raw).ok()?;
    let pid = u32::try_from(lock.get("pid")?.as_u64()?)
        .ok()
        .filter(|pid| *pid > 0)?;
    pid_is_alive(pid).then_some(pid)
}

fn pid_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return false;
        };
        // SAFETY: kill(pid, 0) only probes for existence/permission; it
        // delivers no signal and touches no memory.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// `None` when the environment cannot be read; `Some(None)` when it has no
/// `ANTHROPIC_BASE_URL`.
fn read_base_url(pid: u32) -> Option<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        let raw = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        Some(
            raw.split(|byte| *byte == 0)
                .filter_map(|entry| entry.strip_prefix(b"ANTHROPIC_BASE_URL="))
                .map(|value| String::from_utf8_lossy(value).into_owned())
                .next_back(),
        )
    }
    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/bin/ps")
            .args(["eww", "-p", &pid.to_string(), "-o", "command="])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        let line = String::from_utf8_lossy(&output.stdout);
        if line.trim().is_empty() {
            return None;
        }
        Some(
            line.split(' ')
                .filter_map(|token| token.strip_prefix("ANTHROPIC_BASE_URL="))
                .map(|value| value.trim().to_owned())
                .next_back(),
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_background_launches_are_checked() {
        assert!(starts_background_session(&args(&["--bg", "fix it"])));
        assert!(starts_background_session(&args(&[
            "-p",
            "x",
            "--background"
        ])));
        assert!(starts_background_session(&args(&["agents"])));
        assert!(!starts_background_session(&args(&["-p", "agents"])));
        assert!(!starts_background_session(&args(&[])));
    }

    #[test]
    fn reuses_only_a_supervisor_on_the_csub_gateway() {
        let url = |value: &str| Some(Some(value.to_owned()));
        assert!(verdict(7, url("http://127.0.0.1:20816/l/abc"), 20816).is_ok());
        assert!(verdict(7, url("http://127.0.0.1:20817/l/abc"), 20816).is_err());
        assert!(verdict(7, url("https://gateway.example"), 20816).is_err());
        let error = verdict(7, Some(None), 20816).expect_err("plain supervisor");
        assert!(error.contains("claude daemon stop --any"), "{error}");
        assert!(verdict(7, None, 20816).is_err());
    }

    #[test]
    fn no_lock_or_a_dead_pid_means_no_supervisor() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(running_supervisor(temp.path()), None);
        std::fs::write(temp.path().join("daemon.lock"), r#"{"pid": 0}"#).expect("lock");
        assert_eq!(running_supervisor(temp.path()), None);
        std::fs::write(
            temp.path().join("daemon.lock"),
            format!(r#"{{"pid": {}}}"#, std::process::id()),
        )
        .expect("lock");
        assert_eq!(running_supervisor(temp.path()), Some(std::process::id()));
    }
}
