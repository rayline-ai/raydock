#![cfg(unix)]

use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir()
            .canonicalize()
            .expect("temp directory")
            .join(format!(
                "rayline-desktop-lifecycle-{}-{stamp}",
                std::process::id()
            ));
        fs::create_dir_all(path.join(".rayline/claude-desktop")).expect("home");
        Self(path)
    }
    fn run(&self, action: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_raydock"))
            .env_clear()
            .env("HOME", &self.0)
            .env("PATH", "/usr/bin:/bin")
            .env("RAYLINE_CLAUDE_DESKTOP_NO_LAUNCHD", "1")
            .args(["claude-desktop", action])
            .output()
            .expect("run CLI")
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn mutating_commands_observe_the_same_cross_process_lease() {
    let home = Home::new();
    let lock = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(home.0.join(".rayline/claude-desktop.lock"))
        .expect("lock");
    // SAFETY: flock receives a valid open descriptor and no borrowed pointers.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    for action in [
        "start",
        "stop",
        "off",
        "uninstall",
        "install-service",
        "uninstall-service",
    ] {
        let output = home.run(action);
        assert!(
            !output.status.success(),
            "{action} ignored another operation's lease"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("Another Rayline command"),
            "{action} failed for the wrong reason: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(home.0.join(".rayline/claude-desktop").is_dir());
    }
    drop(lock);
    assert!(
        home.run("off").status.success(),
        "cleanup must work after the lease is released"
    );
}

#[cfg(target_os = "macos")]
#[test]
fn manual_cleanup_ignores_unrelated_desktop_symlinks_without_a_journal() {
    use std::os::unix::fs::symlink;
    let home = Home::new();
    let target = home.0.join("desktop-config-target");
    fs::create_dir_all(&target).expect("target");
    let parent = home.0.join("Library/Application Support");
    fs::create_dir_all(&parent).expect("parent");
    symlink(&target, parent.join("Claude-3p")).expect("Desktop symlink");
    for action in ["off", "uninstall"] {
        let output = home.run(action);
        assert!(
            output.status.success(),
            "{action} should not inspect Desktop without recovery: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert_eq!(fs::read_dir(target).expect("untouched Desktop").count(), 0);
}
