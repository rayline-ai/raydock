//! Compares the counter with a live server's `usage.prompt_tokens`. Ignored by
//! default; run it against a model snapshot directory and its server:
//!
//! ```sh
//! RAYLINE_TOKENCOUNT_MODEL_DIR=/path/to/snapshot \
//! RAYLINE_TOKENCOUNT_CHAT_URL=https://host/v1/chat/completions \
//! RAYLINE_TOKENCOUNT_MODEL=GLM-5.3-Flash-EXL3 \
//! cargo test -p rayline-tokencount --test live_server -- --ignored --nocapture
//! ```

use std::path::{Path, PathBuf};

use rayline_tokencount::PromptCounter;
use serde_json::{Value, json};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name}"))
}

/// Claude Code-shaped Chat requests: tools, tool calls and tool results
/// carrying this workspace's sources, growing with `files`.
fn request(model: &str, files: &[PathBuf]) -> Value {
    let tools: Vec<Value> = (0..12)
        .map(|index| {
            json!({"type": "function", "function": {"name": format!("Tool{index}"), "description": "Run a command. ".repeat(20),
                "parameters": {"type": "object", "properties": {"command": {"type": "string", "description": "The command to run <with> & \"quotes\""},
                "timeout": {"type": "number"}}, "required": ["command"]}}})
        })
        .collect();
    let mut messages = vec![
        json!({"role": "system", "content": "You are a coding agent. ✓ café 日本語 😀\n".repeat(40)}),
        json!({"role": "user", "content": "Review these files."}),
    ];
    for (index, file) in files.iter().enumerate() {
        let source = std::fs::read_to_string(file).unwrap();
        let arguments = json!({"command": format!("cat {}", file.display())}).to_string();
        messages.push(json!({"role": "assistant", "content": "", "tool_calls": [{"id": format!("c{index}"), "type": "function", "function": {"name": "Tool0", "arguments": arguments}}]}));
        messages
            .push(json!({"role": "tool", "tool_call_id": format!("c{index}"), "content": source}));
    }
    messages.push(json!({"role": "user", "content": "Summarise."}));
    json!({"model": model, "messages": messages, "tools": tools, "max_tokens": 1})
}

fn sources() -> Vec<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../rayline-cli/src");
    let mut files: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .collect();
    files.sort();
    files
}

#[test]
#[ignore = "needs a model snapshot and its live server"]
fn counts_what_the_server_counts() {
    let counter = PromptCounter::from_dir(Path::new(&env("RAYLINE_TOKENCOUNT_MODEL_DIR"))).unwrap();
    let url = env("RAYLINE_TOKENCOUNT_CHAT_URL");
    let model = env("RAYLINE_TOKENCOUNT_MODEL");
    let files = sources();
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(900))
        .build()
        .unwrap();
    for size in [1, 4, 12, 30] {
        let body = request(&model, &files[..size.min(files.len())]);
        let started = std::time::Instant::now();
        let ours = counter.count_chat(&body).unwrap().prompt_tokens as i64;
        let elapsed = started.elapsed();
        let usage: Value = client
            .post(&url)
            .json(&body)
            .send()
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .unwrap();
        let server = usage["usage"]["prompt_tokens"].as_i64().unwrap();
        let error = (ours - server) as f64 / server as f64;
        println!(
            "files {size:2}  server {server:7}  counter {ours:7}  error {:+.3}%  {elapsed:?}",
            100.0 * error
        );
        assert!(
            error.abs() < 0.005 || (ours - server).abs() <= 8,
            "counter {ours} vs server {server}"
        );
    }
}
