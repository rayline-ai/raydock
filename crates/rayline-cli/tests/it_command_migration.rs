use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Installation(PathBuf);

impl Installation {
    fn new() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("raydock-command-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).expect("temporary installation");
        Self(path)
    }

    fn binary(&self, name: &str) -> PathBuf {
        self.0
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    }

    fn run(&self, name: &str, args: &[&str]) -> Output {
        Command::new(self.binary(name))
            .args(args)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .output()
            .expect("run installed command")
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn place_payload(path: &Path) {
    fs::copy(env!("CARGO_BIN_EXE_raydock"), path).expect("stage verified payload");
}

#[test]
fn old_updater_landing_migrates_and_alias_preserves_output_arguments_and_status() {
    let install = Installation::new();
    // 0.2.14's updater writes the signed rayline-<platform> payload here.
    place_payload(&install.binary("rayline"));
    let first = install.run("rayline", &["--version"]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).starts_with("raydock "));
    assert!(String::from_utf8_lossy(&first.stderr).contains("remains an alias"));
    assert!(install.binary("raydock").is_file());

    for args in [
        vec!["--version"],
        vec!["--help"],
        vec!["unknown-command", "two words", "λ"],
        vec!["local", "--help"],
    ] {
        let direct = install.run("raydock", &args);
        let alias = install.run("rayline", &args);
        assert_eq!(direct.status.code(), alias.status.code());
        assert_eq!(direct.stdout, alias.stdout);
        assert_eq!(direct.stderr, alias.stderr);
    }
    let help = install.run("rayline", &["--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("Usage: raydock"));
    fs::write(install.binary("rayline-core"), "installed execution core").expect("core marker");
    for name in ["rayline", "raydock"] {
        let check = install.run(
            name,
            &[
                "update",
                "--check",
                "--channel",
                "prod",
                "--version",
                rayline_cli::RAYLINE_VERSION,
            ],
        );
        assert!(
            check.status.success(),
            "{}{}",
            String::from_utf8_lossy(&check.stdout),
            String::from_utf8_lossy(&check.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_named_payload_in_app_bundle_requires_managed_upgrade() {
    let install = Installation::new();
    let bin = install.0.join("Rayline.app/Contents/Resources/bin");
    fs::create_dir_all(&bin).expect("bundle layout");
    let legacy = bin.join(format!("rayline{}", std::env::consts::EXE_SUFFIX));
    place_payload(&legacy);
    let result = Command::new(&legacy)
        .arg("--version")
        .output()
        .expect("run bundle payload");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("managed installation"));
    assert!(
        !bin.join(format!("raydock{}", std::env::consts::EXE_SUFFIX))
            .exists()
    );
}
