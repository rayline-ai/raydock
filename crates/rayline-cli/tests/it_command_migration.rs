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
        vec![r#"two words "quoted" λ \tail\"#],
        vec![""],
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

#[cfg(unix)]
#[test]
fn app_bundle_alias_keeps_existing_command_and_login_service_paths_working() {
    let install = Installation::new();
    let bin = install.0.join("Rayline.app/Contents/Resources/bin");
    fs::create_dir_all(&bin).expect("bundle layout");
    place_payload(&bin.join("raydock"));
    std::os::unix::fs::symlink("raydock", bin.join("rayline")).expect("bundle alias");
    // An existing installation or login service may still invoke either link.
    std::os::unix::fs::symlink(bin.join("rayline"), install.binary("rayline"))
        .expect("existing command link");
    for executable in [bin.join("rayline"), install.binary("rayline")] {
        let result = Command::new(executable)
            .arg("--version")
            .output()
            .expect("run bundled alias");
        assert!(result.status.success());
        assert!(String::from_utf8_lossy(&result.stdout).starts_with("raydock "));
        assert!(result.stderr.is_empty(), "managed aliases must not migrate");
    }
    assert_eq!(
        fs::read_link(bin.join("rayline")).unwrap(),
        Path::new("raydock")
    );
}

#[cfg(unix)]
#[test]
fn failed_migration_preserves_legacy_payload_and_explains_repair() {
    let install = Installation::new();
    let legacy = install.binary("rayline");
    place_payload(&legacy);
    // A conflicting destination must fail visibly rather than run the command
    // under an installation layout the updater cannot maintain.
    fs::create_dir(install.binary("raydock")).expect("conflicting directory");
    let result = install.run("rayline", &["--version"]);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains("Raydock command migration failed"),
        "{error}"
    );
    assert!(error.contains("Re-run the Raydock installer"), "{error}");
    assert_eq!(
        fs::read(&legacy).unwrap(),
        fs::read(env!("CARGO_BIN_EXE_raydock")).unwrap()
    );
    assert!(install.binary("raydock").is_dir());
}

#[test]
fn installer_hard_link_migrates_to_a_forwarding_alias() {
    let install = Installation::new();
    place_payload(&install.binary("raydock"));
    fs::hard_link(install.binary("raydock"), install.binary("rayline"))
        .expect("installer hard link");
    let result = install.run("rayline", &["--version"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(!String::from_utf8_lossy(&result.stderr).contains("Rayline Local is now"));
    let direct = install.run("raydock", &["--version"]);
    let alias = install.run("rayline", &["--version"]);
    assert_eq!(direct.stdout, alias.stdout);
    assert_eq!(direct.stderr, alias.stderr);
    assert_eq!(direct.status.code(), alias.status.code());
}

#[test]
fn installed_alias_inherits_stdin_and_propagates_exit_status() {
    use std::io::Write;
    use std::process::Stdio;

    let install = Installation::new();
    place_payload(&install.binary("rayline"));
    assert!(install.run("rayline", &["--version"]).status.success());
    // Exercise the installed alias against a real child that consumes stdin.
    let source = install.0.join("stdin_probe.rs");
    fs::write(
        &source,
        r#"
        use std::io::{Read, Write};
        fn main() {
            let mut bytes = Vec::new();
            std::io::stdin().read_to_end(&mut bytes).unwrap();
            std::io::stdout().write_all(&bytes).unwrap();
            eprintln!("child stderr");
            std::process::exit(37);
        }
    "#,
    )
    .expect("stdin probe source");
    let build = Command::new("rustc")
        .args(["--edition=2021", "--crate-name", "stdin_probe"])
        .arg(&source)
        .arg("-o")
        .arg(install.binary("raydock"))
        .output()
        .expect("compile stdin probe");
    assert!(
        build.status.success(),
        "{}",
        String::from_utf8_lossy(&build.stderr)
    );
    let mut child = Command::new(install.binary("rayline"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run alias");
    let input = "stdin with spaces, λ, and a second line\nnext line\n";
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .expect("write stdin");
    let result = child.wait_with_output().expect("wait for alias");
    assert_eq!(result.stdout, input.as_bytes());
    assert_eq!(
        String::from_utf8_lossy(&result.stderr).trim(),
        "child stderr"
    );
    assert_eq!(result.status.code(), Some(37));
}

#[cfg(unix)]
#[test]
fn concurrent_first_invocations_converge_on_the_same_command() {
    use std::process::Stdio;

    let install = Installation::new();
    place_payload(&install.binary("rayline"));
    let children: Vec<_> = (0..16)
        .map(|_| {
            Command::new(install.binary("rayline"))
                .arg("--version")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .expect("start concurrent command")
        })
        .collect();
    let results: Vec<_> = children
        .into_iter()
        .map(|child| child.wait_with_output().expect("wait for command"))
        .collect();
    let expected = install.run("raydock", &["--version"]);
    for result in results {
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout, expected.stdout);
    }
    assert_eq!(
        fs::read_link(install.binary("rayline")).unwrap(),
        Path::new("raydock")
    );
}
