use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn run_cli(home: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_rayline"))
        .env_clear()
        .env("HOME", home)
        .args(arguments)
        .output()
        .expect("run isolated CLI")
}

#[test]
fn incomplete_add_reports_required_arguments_without_changing_registry() {
    let home = tempfile::tempdir().expect("home");
    let registry_dir = home.path().join(".config/rayline");
    fs::create_dir_all(&registry_dir).expect("registry directory");
    let registry = registry_dir.join("subscriptions.json");
    let before = b"do not read or replace this registry during argument parsing";
    fs::write(&registry, before).expect("registry sentinel");

    let cases: &[(&[&str], &str)] = &[
        (&["subscriptions", "add"], "<account-id>"),
        (&["subscriptions", "add", "work"], "--claude-config-dir"),
        (
            &["subscriptions", "add", "work", "--claude-config-dir"],
            "--claude-config-dir",
        ),
        (&["subscriptions", "add", "work", "--pool"], "--pool"),
    ];
    for &(arguments, missing_argument) in cases {
        let output = run_cli(home.path(), arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        let error = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
        let diagnostic = error.lines().next().expect("argument diagnostic");
        assert!(diagnostic.contains(missing_argument), "{error}");
        assert!(
            error.contains("Usage: rayline subscriptions add "),
            "{error}"
        );
        assert_eq!(fs::read(&registry).expect("unchanged registry"), before);
        assert!(!home.path().join(".claude").exists());
    }
}

#[test]
fn subscription_usage_errors_are_distinct_from_unavailable_commands() {
    let home = tempfile::tempdir().expect("home");
    let cases: &[&[&str]] = &[
        &["subscriptions", "unknown"],
        &["subscriptions", "list", "--unknown-option"],
        &["subscriptions", "remove"],
        &["subscriptions", "reload", "--json"],
    ];
    for arguments in cases {
        let output = run_cli(home.path(), arguments);
        assert_eq!(output.status.code(), Some(2), "{arguments:?}: {output:?}");
        assert!(output.stdout.is_empty(), "{arguments:?}: {output:?}");
        let error = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
        assert!(error.contains("Usage: rayline subscriptions "), "{error}");
    }
    let unsupported = run_cli(home.path(), &["unsupported-command"]);
    assert_eq!(unsupported.status.code(), Some(127));
    assert!(!home.path().join(".config").exists());
}

#[test]
fn explicit_add_help_succeeds_without_required_arguments_or_registry() {
    let home = tempfile::tempdir().expect("home");
    let output = run_cli(home.path(), &["subscriptions", "add", "--help"]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(help.contains("<account-id> --claude-config-dir <path>"));
    assert!(!home.path().join(".config").exists());
}
