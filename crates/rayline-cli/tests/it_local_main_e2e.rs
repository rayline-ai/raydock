//! Interactive end-to-end for the `agent = local` routing modes (`L-Rc`/`L-Rl`/`L-K`/`L-L`,
//! marked ‡ in examples/routing-modes/README.md).
//!
//! This is the harness for "drive a local **main** agent through `--config` and
//! confirm it can actually spawn a subagent." It is `#[ignore]`d and gated on
//! `RAYLINE_LOCAL_MAIN_E2E=1` because it requires a real Claude Code binary, a
//! running local model, and network — and, crucially, it is **expected to fail**
//! with the small local models available today (qwen 7B/9B and similar emit tool
//! calls as plain text instead of invoking the `Task` tool, so the main agent
//! never spawns a subagent). The hermetic routing tests in `rayline-local-router`
//! already prove the *routing* for these configs is correct; this test guards the
//! local-main *capability*, which becomes meaningful once a tool-capable local
//! main is available.
//!
//! Run with:
//!   CLAUDE_BIN=/path/to/claude RAYLINE_LOCAL_MAIN_E2E=1 \
//!     cargo test -p rayline-cli --test it_local_main_e2e -- --ignored --nocapture

use std::process::Command;

#[test]
#[ignore = "live: set RAYLINE_LOCAL_MAIN_E2E=1 (+ CLAUDE_BIN). Expected-fail with current small local mains — they can't drive Claude Code tool calls, so the local main never spawns a subagent. See examples/routing-modes/README.md (‡)."]
fn local_main_spawns_subagent_end_to_end() {
    if std::env::var_os("RAYLINE_LOCAL_MAIN_E2E").is_none() {
        eprintln!(
            "skipping local-main e2e: set RAYLINE_LOCAL_MAIN_E2E=1 (and CLAUDE_BIN, plus a \
             tool-capable local model) to run it"
        );
        return;
    }

    let rayline = env!("CARGO_BIN_EXE_raydock");
    let config = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../examples/routing-modes/L-L.json"
    );
    // A blunt prompt that forces an immediate `Task` call. A capable main spawns
    // an Explore subagent; today's small local mains emit fake tool-call text.
    let prompt = "Use the Task tool to launch an Explore subagent that replies with the single \
                  word PONG. Do not use any other tool first. After it returns, output: DONE";

    let run = Command::new(rayline)
        .args([
            "claude", "--config", config, "--via", "proxy", "--", "-p", prompt,
        ])
        .output()
        .expect("spawn `raydock claude --config`");

    // The subagent leg is observable in router metrics as a request carrying an
    // `agent_type`. With a tool-capable local main this is present; today it is
    // not — which is exactly the expected-fail this test documents.
    let top = Command::new(rayline)
        .args(["top", "--json", "--all"])
        .output()
        .expect("spawn `raydock top --json`");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&top.stdout).expect("parse `raydock top --json` output");

    let spawned_subagent = ["active", "recent"]
        .iter()
        .filter_map(|section| snapshot.get(*section))
        .filter_map(|section| section.as_array())
        .flatten()
        .any(|req| {
            req.get("agent_type")
                .map(|value| !value.is_null())
                .unwrap_or(false)
        });

    assert!(
        spawned_subagent,
        "local main did not spawn a subagent — expected-fail with current small local models.\n\
         stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
    );
}
