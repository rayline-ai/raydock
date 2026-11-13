//! The installed binary refuses a bad command line or config before it runs
//! anything (docs/spec.md sections 3 and 5.8). Unix only: on Windows the home
//! directory does not come from `HOME`.
#![cfg(unix)]

use std::fs;
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
        let path =
            std::env::temp_dir().join(format!("raydock-startup-{}-{stamp}", std::process::id()));
        fs::create_dir_all(&path).expect("temporary home");
        Self(path)
    }

    fn settings(&self, json: &str) {
        let dir = self.0.join(".config").join("rayline");
        fs::create_dir_all(&dir).expect("settings dir");
        fs::write(dir.join("settings.json"), json).expect("settings file");
    }

    /// Runs `raydock` in the home directory with `PATH` empty, so a run that got as far as starting
    /// a harness would fail differently.
    fn run(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_raydock"));
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("RAYLINE_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("PATH", "")
            .output()
            .expect("run raydock")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

const ROUTING: &str = r#"{"routing": {"decisions_model": "arc-swift-1.0",
    "classifier_model": "anthropic/claude-haiku-5.5",
    "policy": {"preset": "arc-policy.hybrid-default.v1"}}}"#;

/// A `--config` file with `routing`, inside the temporary home.
const ROUTING_FILE: &str = "routing.json";

fn assert_fails(output: &Output, code: i32, stderr: &str) {
    let text = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(code), "{text}");
    assert!(text.contains(stderr), "{text}");
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn a_usage_error_exits_2() {
    let home = Home::new();
    assert_fails(
        &home.run(&["run", "nope"]),
        2,
        "rayline: unknown harness `nope`",
    );
    assert_fails(
        &home.run(&["run", "pi", "--local", "--config", "x.json"]),
        2,
        "`--local` and `--config` can't be used together",
    );
}

#[test]
fn local_mode_without_routing_fails_before_anything_runs() {
    let home = Home::new();
    assert_fails(
        &home.run(&["run", "pi", "--local"]),
        1,
        "`routing` is required in local mode",
    );
}

#[test]
fn a_missing_config_file_fails_before_anything_runs() {
    let home = Home::new();
    let missing = home.0.join("missing.json");
    assert_fails(
        &home.run(&[
            "run",
            "claude",
            "--config",
            missing.to_str().expect("utf-8"),
        ]),
        1,
        "missing.json",
    );
}

#[test]
fn an_unknown_env_fails_before_anything_runs() {
    let home = Home::new();
    for args in [
        &["--env", "nope", "run", "pi"][..],
        &["--env", "nope", "auth", "status"],
    ] {
        assert_fails(
            &home.run(args),
            1,
            "`environments.nope` is not defined, and `--env nope` selects it",
        );
    }
}

#[test]
fn a_bad_update_channel_fails_before_update_runs() {
    let home = Home::new();
    home.settings(r#"{"update": {"channel": "nightly"}}"#);
    assert_fails(&home.run(&["update", "--check"]), 1, "update.channel");
}

#[test]
fn local_mode_refuses_the_kept_options_it_does_not_run_yet() {
    let home = Home::new();
    home.settings(ROUTING);
    fs::write(home.0.join(ROUTING_FILE), ROUTING).expect("routing file");
    for (args, option) in [
        (
            &["run", "claude", "--local", "--isolated"][..],
            "`--isolated`",
        ),
        (
            &["run", "claude-desktop", "--local"],
            "`run claude-desktop`",
        ),
        (&["run", "codex", "app", "--local"], "`run codex app`"),
        (
            &["run", "codex", "configure", "--local"],
            "`run codex configure`",
        ),
        (&["run", "codex", "--local", "--model", "m"], "`--model`"),
        (&["run", "pi", "--config", ROUTING_FILE], "`--config`"),
    ] {
        assert_fails(
            &home.run(args),
            1,
            &format!("Error: {option} is not available in local mode yet"),
        );
    }
}
