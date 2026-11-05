//! Live integration tests for the local router's true streaming + image paths.
//!
//! These hit real providers, so they require an EXPLICIT opt-in — `RAYLINE_LIVE_TESTS=1`
//! AND the relevant provider key. A provider key alone is not enough: that keeps
//! the default `cargo test` run (and CI) hermetic even when a developer shell or
//! CI environment happens to have `OPENAI_API_KEY`/`OPENROUTER_API_KEY` set.
//! Run locally with:
//!
//! ```
//! RAYLINE_LIVE_TESTS=1 OPENAI_API_KEY=... OPENROUTER_API_KEY=... \
//!   cargo +1.88.0 test -p rayline-local-router --test it_live_streaming \
//!     -- --nocapture --test-threads=1
//! ```
//!
//! Every test starts the real `serve()` on a free port with a temp router
//! config and POSTs Anthropic `/v1/messages` to it.

use std::io::Write;
use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use rayline_local_router::{LocalRouterOptions, serve};
use serde_json::{Value, json};

/// Returns true (and prints why) when a live test should be skipped. Requires an
/// explicit `RAYLINE_LIVE_TESTS` opt-in in addition to the provider key, so keys
/// lingering in a shell or CI never trigger external calls by default.
fn skip_live(test: &str, key_env: &str) -> bool {
    if std::env::var("RAYLINE_LIVE_TESTS").is_err() {
        eprintln!("SKIP {test}: set RAYLINE_LIVE_TESTS=1 to run live provider tests");
        return true;
    }
    if std::env::var(key_env).is_err() {
        eprintln!("SKIP {test}: {key_env} not set");
        return true;
    }
    false
}

/// Pick a free TCP port by binding to :0 and immediately dropping the listener.
fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

/// Write a temp router config and return its path (kept alive by the caller).
fn write_config(name: &str, config: &Value) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "rayline-live-{}-{}-{}.json",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut file = std::fs::File::create(&path).expect("create temp config");
    file.write_all(serde_json::to_string_pretty(config).unwrap().as_bytes())
        .expect("write temp config");
    path
}

/// Start `serve()` on `port` with the given config and wait until it answers.
async fn start_router(port: u16, config_path: PathBuf) {
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config_path),
        ..LocalRouterOptions::default()
    };
    tokio::spawn(async move {
        let _ = serve(opts).await;
    });
    let client = reqwest::Client::new();
    for _ in 0..100 {
        if client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .map(|resp| resp.status().is_success())
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router on port {port} did not become healthy");
}

/// Assert HTTP 200, surfacing the upstream/router error body on failure so live
/// runs are debuggable. Returns the (still-streamable) response.
async fn expect_ok(resp: reqwest::Response) -> reqwest::Response {
    if resp.status() != 200 {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        panic!("router returned {status}: {body}");
    }
    resp
}

/// Collect all SSE `data:` JSON payloads from a streamed response body, asserting
/// the content type is `text/event-stream`.
async fn collect_sse_events(resp: reqwest::Response) -> Vec<Value> {
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(
        content_type.starts_with("text/event-stream"),
        "expected SSE content type, got {content_type:?}"
    );
    let mut buffer = String::new();
    let mut events = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk.expect("stream chunk");
        buffer.push_str(&String::from_utf8_lossy(&bytes));
        // Drain complete lines; SSE data lines carry the JSON payloads we want.
        while let Some(idx) = buffer.find('\n') {
            let mut line = buffer[..idx].to_owned();
            buffer.drain(..idx + 1);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(payload) = line.strip_prefix("data:") {
                let payload = payload.trim();
                if payload.is_empty() || payload == "[DONE]" {
                    continue;
                }
                if let Ok(value) = serde_json::from_str::<Value>(payload) {
                    events.push(value);
                }
            }
        }
    }
    events
}

fn event_types(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_owned())
        .collect()
}

fn concatenated_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect()
}

/// Base64 PNG of three distinct colored shapes — a red circle, a blue square,
/// and a green triangle on white — generated offline and checked in under
/// `fixtures/`. A real multi-shape image (not a solid pixel, which providers
/// reject as "unsupported") lets the image tests assert the model actually
/// perceived the picture by naming the shapes and colors back.
fn shapes_png_b64() -> &'static str {
    include_str!("fixtures/shapes_png_base64.txt").trim()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_true_streaming_text() {
    if skip_live("openai_chat_true_streaming_text", "OPENAI_API_KEY") {
        return;
    }
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "openai",
            "protocol": "openai_chat",
            "base_url": "https://api.openai.com/v1",
            "api_key_env": "OPENAI_API_KEY",
            "models": ["gpt-5.5"]
        }],
        "routes": {
            "main": {"endpoint": "openai", "model": "gpt-5.5"},
            "default": {"endpoint": "openai", "model": "gpt-5.5"}
        }
    });
    let path = write_config("openai-stream", &config);
    start_router(port, path.clone()).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({
            "model": "rayline-auto",
            "stream": true,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "Count from 1 to 20, space separated."}]
        }))
        .send()
        .await
        .expect("send streaming request");
    let resp = expect_ok(resp).await;

    let events = collect_sse_events(resp).await;
    let types = event_types(&events);
    let text_deltas = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .count();
    let text = concatenated_text(&events);
    assert!(
        text_deltas > 1,
        "expected MANY text_delta events (true streaming), got {text_deltas}; text={text:?}"
    );
    assert!(!text.trim().is_empty(), "expected non-empty text");
    assert!(types.contains(&"message_start".to_owned()));
    assert!(types.contains(&"message_stop".to_owned()));
    let _ = std::fs::remove_file(path);
    println!(
        "PASS openai_chat_true_streaming_text: text_delta_events={text_deltas} reply={:?}",
        text.trim()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_tool_use_streaming() {
    if skip_live("openai_chat_tool_use_streaming", "OPENAI_API_KEY") {
        return;
    }
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "openai",
            "protocol": "openai_chat",
            "base_url": "https://api.openai.com/v1",
            "api_key_env": "OPENAI_API_KEY",
            "models": ["gpt-5.5"]
        }],
        "routes": {
            "main": {"endpoint": "openai", "model": "gpt-5.5"},
            "default": {"endpoint": "openai", "model": "gpt-5.5"}
        }
    });
    let path = write_config("openai-tool", &config);
    start_router(port, path.clone()).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({
            "model": "rayline-auto",
            "stream": true,
            "max_tokens": 256,
            "tool_choice": {"type": "tool", "name": "get_weather"},
            "tools": [{
                "name": "get_weather",
                "description": "Get the current weather for a city",
                "input_schema": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }],
            "messages": [{"role": "user", "content": "What is the weather in Paris? Use the tool."}]
        }))
        .send()
        .await
        .expect("send tool streaming request");
    let resp = expect_ok(resp).await;

    let events = collect_sse_events(resp).await;
    let tool_start = events
        .iter()
        .find(|e| e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use")
        .expect("a streamed tool_use content_block_start");
    assert_eq!(tool_start["content_block"]["name"], "get_weather");
    let args: String = events
        .iter()
        .filter(|e| e["delta"]["type"] == "input_json_delta")
        .filter_map(|e| e["delta"]["partial_json"].as_str())
        .collect();
    let parsed: Value =
        serde_json::from_str(&args).unwrap_or_else(|_| panic!("reconstructed tool args: {args:?}"));
    assert!(
        parsed["city"].as_str().is_some(),
        "expected a city argument, got {parsed:?}"
    );
    let _ = std::fs::remove_file(path);
    println!("PASS openai_chat_tool_use_streaming: reconstructed_args={parsed}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_image_shape_detection() {
    if skip_live("openai_chat_image_shape_detection", "OPENAI_API_KEY") {
        return;
    }
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "openai",
            "protocol": "openai_chat",
            "base_url": "https://api.openai.com/v1",
            "api_key_env": "OPENAI_API_KEY",
            "models": ["gpt-5.5"]
        }],
        "routes": {
            "main": {"endpoint": "openai", "model": "gpt-5.5"},
            "default": {"endpoint": "openai", "model": "gpt-5.5"}
        }
    });
    let path = write_config("openai-image", &config);
    start_router(port, path.clone()).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({
            "model": "rayline-auto",
            "stream": false,
            "max_tokens": 64,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "List each colored shape in this image, one per line as '<color> <shape>'."},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": shapes_png_b64()
                    }}
                ]
            }]
        }))
        .send()
        .await
        .expect("send image request");
    let resp = expect_ok(resp).await;
    let body: Value = resp.json().await.expect("json body");
    let answer = body["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .unwrap_or("")
        .to_owned();
    // The model must name the shapes and colors back, proving it perceived the
    // image (and that the router delivered it intact through the conversion).
    let lower = answer.to_lowercase();
    for needle in ["red", "blue", "green", "circle", "triangle"] {
        assert!(
            lower.contains(needle),
            "model did not report {needle:?} for the shapes image; answer={answer:?} ({body})"
        );
    }
    assert!(
        lower.contains("square") || lower.contains("rectangle"),
        "model did not report the blue square; answer={answer:?} ({body})"
    );
    let _ = std::fs::remove_file(path);
    println!(
        "PASS openai_chat_image_shape_detection: answer={:?}",
        answer.trim()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openrouter_anthropic_streaming() {
    if skip_live("openrouter_anthropic_streaming", "OPENROUTER_API_KEY") {
        return;
    }
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "openrouter",
            "protocol": "anthropic_messages",
            "base_url": "https://openrouter.ai/api",
            "api_key_env": "OPENROUTER_API_KEY",
            "auth": "bearer",
            "models": ["anthropic/claude-sonnet-5"]
        }],
        "routes": {
            "main": {"endpoint": "openrouter", "model": "anthropic/claude-sonnet-5"},
            "default": {"endpoint": "openrouter", "model": "anthropic/claude-sonnet-5"}
        }
    });
    let path = write_config("openrouter-stream", &config);
    start_router(port, path.clone()).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-auto",
            "stream": true,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "Count from 1 to 20, space separated."}]
        }))
        .send()
        .await
        .expect("send openrouter streaming request");
    let resp = expect_ok(resp).await;

    let events = collect_sse_events(resp).await;
    let types = event_types(&events);
    let text_deltas = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .count();
    let text = concatenated_text(&events);
    // The router passes OpenRouter's native Anthropic SSE through verbatim, so we
    // assert the event structure arrived intact. We deliberately do NOT require a
    // minimum delta count: OpenRouter's chunk granularity is provider-controlled
    // and sometimes coalesces the whole reply into a single content_block_delta.
    assert!(
        text_deltas >= 1,
        "expected at least one native Anthropic text_delta; text={text:?}"
    );
    assert!(
        types.contains(&"message_start".to_owned()),
        "missing message_start"
    );
    assert!(
        types.contains(&"message_stop".to_owned()),
        "missing message_stop"
    );
    assert!(
        text.contains("20"),
        "streamed reply should contain the counted sequence; got {text:?}"
    );
    let _ = std::fs::remove_file(path);
    println!(
        "PASS openrouter_anthropic_streaming: text_delta_events={text_deltas} reply={:?}",
        text.trim()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openrouter_anthropic_image_shape_detection() {
    if skip_live(
        "openrouter_anthropic_image_shape_detection",
        "OPENROUTER_API_KEY",
    ) {
        return;
    }
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "openrouter",
            "protocol": "anthropic_messages",
            "base_url": "https://openrouter.ai/api",
            "api_key_env": "OPENROUTER_API_KEY",
            "auth": "bearer",
            "models": ["anthropic/claude-sonnet-5"]
        }],
        "routes": {
            "main": {"endpoint": "openrouter", "model": "anthropic/claude-sonnet-5"},
            "default": {"endpoint": "openrouter", "model": "anthropic/claude-sonnet-5"}
        }
    });
    let path = write_config("openrouter-image", &config);
    start_router(port, path.clone()).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-auto",
            "stream": false,
            "max_tokens": 64,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "List each colored shape in this image, one per line as '<color> <shape>'."},
                    {"type": "image", "source": {
                        "type": "base64",
                        "media_type": "image/png",
                        "data": shapes_png_b64()
                    }}
                ]
            }]
        }))
        .send()
        .await
        .expect("send openrouter image request");
    let resp = expect_ok(resp).await;
    let body: Value = resp.json().await.expect("json body");
    let answer = body["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .unwrap_or("")
        .to_owned();
    // The model must name the shapes and colors back, proving it perceived the
    // image (and that the router delivered it intact through the conversion).
    let lower = answer.to_lowercase();
    for needle in ["red", "blue", "green", "circle", "triangle"] {
        assert!(
            lower.contains(needle),
            "model did not report {needle:?} for the shapes image; answer={answer:?} ({body})"
        );
    }
    assert!(
        lower.contains("square") || lower.contains("rectangle"),
        "model did not report the blue square; answer={answer:?} ({body})"
    );
    let _ = std::fs::remove_file(path);
    println!(
        "PASS openrouter_anthropic_image_shape_detection: answer={:?}",
        answer.trim()
    );
}

/// Documented default auth: an `anthropic_messages` endpoint with `api_key_env`
/// and NO `auth` override sends the key as `x-api-key` (the historical default).
/// Proven against OpenRouter's Anthropic endpoint, which accepts x-api-key.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openrouter_anthropic_default_xapikey_auth() {
    if skip_live(
        "openrouter_anthropic_default_xapikey_auth",
        "OPENROUTER_API_KEY",
    ) {
        return;
    }
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "anthropic-compatible",
            "protocol": "anthropic_messages",
            "base_url": "https://openrouter.ai/api",
            "api_key_env": "OPENROUTER_API_KEY",
            "models": ["anthropic/claude-sonnet-5"]
            // No "auth" field => default x-api-key for anthropic_messages.
        }],
        "routes": {
            "main": {"endpoint": "anthropic-compatible", "model": "anthropic/claude-sonnet-5"},
            "default": {"endpoint": "anthropic-compatible", "model": "anthropic/claude-sonnet-5"}
        }
    });
    let path = write_config("openrouter-xapikey", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-auto",
            "stream": false,
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "Reply with the single word PONG."}]
        }))
        .send()
        .await
        .expect("send x-api-key request");
    let resp = expect_ok(resp).await;
    let body: Value = resp.json().await.expect("json body");
    let text = body["content"]
        .as_array()
        .and_then(|blocks| blocks.iter().find(|b| b["type"] == "text"))
        .and_then(|b| b["text"].as_str())
        .unwrap_or("")
        .to_owned();
    assert!(
        text.to_uppercase().contains("PONG"),
        "default x-api-key auth round-trip failed; body={body}"
    );
    let _ = std::fs::remove_file(path);
    println!(
        "PASS openrouter_anthropic_default_xapikey_auth: reply={:?}",
        text.trim()
    );
}
