//! Release packaging checks run these commands against the built binary and
//! expect the not-available message on stderr and a failing exit. Keep the
//! contract explicit here.

use std::fs;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const MESSAGE: &str = "not available in this public Raydock build";

#[test]
fn private_commands_report_not_available() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let home = std::env::temp_dir().join(format!("raydock-private-{}-{stamp}", std::process::id()));
    fs::create_dir_all(&home).expect("scratch home");
    for command in [
        "chat",
        "api",
        "menubar",
        "install-cli",
        "hook-send",
        "agent",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_raydock"))
            .arg(command)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .output()
            .expect("run raydock");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success(), "{command} succeeded");
        assert!(stderr.contains(MESSAGE), "{command}: {stderr}");
    }
    let _ = fs::remove_dir_all(&home);
}
