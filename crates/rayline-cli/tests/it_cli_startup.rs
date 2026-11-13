//! The installed binary refuses a bad command line or config before it runs
//! anything (docs/spec.md sections 3 and 5.8).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
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
        self.run_exe(Path::new(env!("CARGO_BIN_EXE_raydock")), args)
    }

    /// Like [`Home::run`], with the binary at `exe`. The Hugging Face cache is
    /// `<home>/hf`.
    fn run_exe(&self, exe: &Path, args: &[&str]) -> Output {
        let mut command = Command::new(exe);
        for (name, _) in std::env::vars_os() {
            if name.to_string_lossy().starts_with("RAYLINE_") {
                command.env_remove(name);
            }
        }
        command
            .args(args)
            .current_dir(&self.0)
            .env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("PATH", "")
            .env("HF_HOME", self.0.join("hf"))
            .env_remove("HF_HUB_CACHE")
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
    for args in [&["run", "pi", "--local"][..], &["local", "start"]] {
        assert_fails(&home.run(args), 1, "`routing` is required in local mode");
    }
}

#[test]
fn a_missing_config_file_fails_before_anything_runs() {
    let home = Home::new();
    let missing = home.0.join("missing.json");
    let missing = missing.to_str().expect("utf-8");
    for args in [
        &["run", "claude", "--config", missing][..],
        &["local", "start", "--config", missing],
    ] {
        assert_fails(&home.run(args), 1, "missing.json");
    }
}

#[test]
fn claude_is_found_on_path_only() {
    let home = Home::new();
    // Where Claude Code's installers put it; neither is looked at.
    for file in [".local/bin/claude", ".claude/local/claude"] {
        let file = home.0.join(file);
        fs::create_dir_all(file.parent().expect("parent")).expect("install dir");
        fs::write(file, "").expect("claude");
    }
    assert_fails(
        &home.run(&["run", "claude"]),
        1,
        "`claude` not found on PATH",
    );
}

#[test]
fn a_config_file_starts_the_local_process_from_it() {
    let home = Home::new();
    fs::write(home.0.join(ROUTING_FILE), ROUTING).expect("routing file");
    for args in [
        &["run", "pi", "--config", ROUTING_FILE][..],
        &["local", "start", "--config", ROUTING_FILE],
        &["run", "codex", "--config", ROUTING_FILE],
    ] {
        assert_fails(&home.run(args), 1, "installed rayline-core is missing at");
    }
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
fn local_mode_refuses_harness_args_before_starting_the_local_process() {
    let home = Home::new();
    home.settings(ROUTING);
    for (args, message) in [
        (
            &["run", "pi", "--local", "--", "--resume"][..],
            "local mode requires a fresh harness session",
        ),
        (
            &["run", "openclaw", "--local", "--", "--container", "c"],
            "OpenClaw container mode cannot use the hybrid gateway configuration",
        ),
    ] {
        assert_fails(&home.run(args), 1, message);
    }
}

#[test]
fn local_mode_refuses_the_kept_options_it_does_not_run() {
    let home = Home::new();
    home.settings(ROUTING);
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
    ] {
        assert_fails(
            &home.run(args),
            1,
            &format!("Error: {option} is not available in local mode"),
        );
    }
}

/// A config whose model names a tokenizer, so a start that got past the
/// router key would download it.
const LOCAL_WITH_TOKENIZER: &str = r#"{
    "endpoints": {"ollama": {"base_url": "http://127.0.0.1:9",
        "protocols": ["anthropic_messages", "openai_chat"]}},
    "models": {"z-ai/glm-5.3-flash": {"endpoint": "ollama", "model": "glm",
        "context_window": 1000000, "max_output_tokens": 32000,
        "tokenizer": "zai-org/GLM-5.3-Flash",
        "price": {"input": 0, "output": 0, "cache_read": 0, "cache_write": 0}}},
    "routing": {"decisions_model": "arc-swift-1.0",
        "classifier_model": "anthropic/claude-haiku-5.5",
        "policy": {"preset": "arc-policy.hybrid-default.v1"}}}"#;

#[test]
fn a_signed_out_local_start_fails_before_it_clears_state_or_downloads() {
    let home = Home::new();
    home.settings(LOCAL_WITH_TOKENIZER);
    // An installed release: `raydock` with `rayline-core` beside it.
    let bin = home.0.join("bin");
    fs::create_dir_all(&bin).expect("bin dir");
    let exe = bin.join(format!("raydock{}", std::env::consts::EXE_SUFFIX));
    fs::copy(env!("CARGO_BIN_EXE_raydock"), &exe).expect("copy raydock");
    fs::write(
        bin.join(format!("rayline-core{}", std::env::consts::EXE_SUFFIX)),
        "",
    )
    .expect("core");
    // What a local process that has exited left behind.
    let mut exited = Command::new(&exe)
        .arg("--version")
        .stdout(Stdio::null())
        .spawn()
        .expect("run raydock");
    exited.wait().expect("raydock exits");
    let state = home.0.join(".rayline").join("local");
    fs::create_dir_all(&state).expect("state dir");
    let supervisor = format!(
        r#"{{"pid": {}, "config": "/c.json", "env": null}}"#,
        exited.id()
    );
    fs::write(state.join("supervisor.json"), &supervisor).expect("supervisor");
    fs::write(state.join("ready.json"), "{}").expect("ready");

    for args in [&["local", "start"][..], &["run", "pi", "--local"]] {
        assert_fails(&home.run_exe(&exe, args), 1, "Not logged in to prod");
        assert_eq!(
            fs::read_to_string(state.join("supervisor.json")).expect("supervisor kept"),
            supervisor
        );
        assert!(state.join("ready.json").exists());
        assert!(!home.0.join("hf").exists());
    }
}
