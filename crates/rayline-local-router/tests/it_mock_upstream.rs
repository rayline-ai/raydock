//! Hermetic end-to-end tests proving the documented provider-endpoint configs
//! work through the full HTTP path, using an in-process mock upstream instead of
//! a real provider. These run in CI with no API keys and loopback-only traffic,
//! and — because we control the upstream byte-for-byte — they assert the
//! streaming behavior deterministically (which the live provider tests cannot,
//! since real providers control their own chunk granularity).

use std::net::TcpListener as StdTcpListener;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use rayline_local_router::{LocalRouterOptions, serve};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn free_port() -> u16 {
    StdTcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Accept one connection, drain the request, and reply with a fixed raw HTTP/1.1
/// response. `connection: close` lets the client read a body of unknown length
/// (SSE) until EOF. The caller passes a pre-bound listener so the port is known
/// before the router is configured.
async fn serve_once(listener: TcpListener, response: String) {
    if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 16384];
        // A single read drains the (small) request line, headers, and body for a
        // loopback request; we don't need the contents, only to free the socket.
        let _ = sock.read(&mut buf).await;
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

async fn serve_once_capture(listener: TcpListener, response: String, tx: oneshot::Sender<String>) {
    if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 16384];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

fn http_sse(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n{body}"
    )
}

fn http_json(body: &Value) -> String {
    let body = body.to_string();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

fn write_config(name: &str, config: &Value) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "rayline-mock-{}-{}-{}.json",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
    path
}

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
            .map(|r| r.status().is_success())
            .unwrap_or(false)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("router on port {port} did not become healthy");
}

async fn collect_sse(resp: reqwest::Response) -> Vec<Value> {
    let mut buffer = String::new();
    let mut events = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
        while let Some(idx) = buffer.find('\n') {
            let mut line = buffer[..idx].to_owned();
            buffer.drain(..idx + 1);
            if line.ends_with('\r') {
                line.pop();
            }
            if let Some(p) = line.strip_prefix("data:") {
                let p = p.trim();
                if !p.is_empty() && p != "[DONE]" {
                    if let Ok(v) = serde_json::from_str::<Value>(p) {
                        events.push(v);
                    }
                }
            }
        }
    }
    events
}

/// Documented config: an arbitrary local OpenAI-compatible endpoint (e.g. a
/// llama.cpp server) with NO `api_key_env`. Proves the openai_chat → Anthropic
/// SSE translation end-to-end over real HTTP, against an arbitrary base_url,
/// with the no-auth path, deterministically (exactly one text_delta per
/// upstream content fragment).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_chat_streaming_translation_through_mock_no_auth() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "data: {\"id\":\"chatcmpl-mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2}}\n\n",
        "data: [DONE]\n\n",
    );
    tokio::spawn(serve_once(upstream, http_sse(sse)));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "local-openai",
            "protocol": "openai_chat",
            "base_url": format!("http://127.0.0.1:{up_port}/v1"),
            "models": ["local-model-x"]
            // No api_key_env: the documented "omit it if the server needs no auth" path.
        }],
        "routes": {
            "main": {"endpoint": "local-openai", "model": "local-model-x"},
            "default": {"endpoint": "local-openai", "model": "local-model-x"}
        }
    });
    let path = write_config("openai-stream", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .json(&json!({
            "model": "rayline-router",
            "stream": true,
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    assert!(ct.starts_with("text/event-stream"), "content-type={ct:?}");

    let events = collect_sse(resp).await;
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    let text_deltas = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .count();
    let text: String = events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    assert_eq!(text_deltas, 2, "one text_delta per upstream fragment");
    assert_eq!(text, "Hello world");
    assert_eq!(types.first(), Some(&"message_start"));
    assert_eq!(types.last(), Some(&"message_stop"));
    let message_delta = events
        .iter()
        .find(|e| e["type"] == "message_delta")
        .expect("message_delta");
    assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(message_delta["usage"]["output_tokens"], 2);

    let _ = std::fs::remove_file(path);
    println!("PASS mock openai_chat streaming (no auth): deltas={text_deltas} text={text:?}");
}

/// Documented config: an `anthropic_messages` endpoint. Proves the router
/// forwards native Anthropic SSE through VERBATIM — preserving every upstream
/// content_block_delta (no coalescing). This is the deterministic counterpart to
/// the live OpenRouter test, whose delta count is provider-controlled.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_messages_passthrough_preserves_deltas_through_mock() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_mock\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" there\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    tokio::spawn(serve_once(upstream, http_sse(sse)));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "anthropic-compatible",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["claude-mock"]
        }],
        "routes": {
            "main": {"endpoint": "anthropic-compatible", "model": "claude-mock"},
            "default": {"endpoint": "anthropic-compatible", "model": "claude-mock"}
        }
    });
    let path = write_config("anthropic-passthrough", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-router",
            "stream": true,
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("router request");
    if resp.status() != 200 {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        panic!("unexpected status {status}: {body}");
    }

    let events = collect_sse(resp).await;
    let text_deltas = events
        .iter()
        .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
        .count();
    let text: String = events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    // Passthrough must preserve BOTH upstream deltas (not coalesce them).
    assert_eq!(
        text_deltas, 2,
        "passthrough must preserve every upstream delta"
    );
    assert_eq!(text, "Hi there");
    assert!(events.iter().any(|e| e["type"] == "message_stop"));

    let _ = std::fs::remove_file(path);
    println!("PASS mock anthropic_messages passthrough: deltas={text_deltas} text={text:?}");
}

/// Codex/OpenAI Responses mode: a request to the router's `/v1/responses` should
/// route to an `openai_responses` endpoint, rewrite the upstream model, preserve
/// SSE events, and keep Codex-facing model identity on the response headers.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_responses_passthrough_through_mock_no_auth() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_mock\",\"status\":\"in_progress\",\"model\":\"mock-model\"}}\n\n",
        "event: response.output_item.added\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"phase\":\"final_answer\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello Codex\"}\n\n",
        "event: response.output_item.done\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"Hello Codex\"}],\"phase\":\"final_answer\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_mock\",\"status\":\"completed\",\"model\":\"mock-model\",\"usage\":{\"input_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":0},\"output_tokens\":2,\"output_tokens_details\":{\"reasoning_tokens\":0},\"total_tokens\":5}}}\n\n",
    );
    tokio::spawn(serve_once(upstream, http_sse(sse)));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "mock-openai",
            "protocol": "openai_responses",
            "base_url": format!("http://127.0.0.1:{up_port}/v1"),
            "models": ["mock-model"]
        }],
        "routes": {
            "main": {"endpoint": "mock-openai", "model": "mock-model"},
            "default": {"endpoint": "mock-openai", "model": "mock-model"},
            "model_routes": {
                "rayline-local": {"endpoint": "mock-openai", "model": "mock-model"}
            }
        }
    });
    let path = write_config("openai-responses", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("x-client-request-id", "req_codex_mock")
        .json(&json!({
            "model": "rayline-local",
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "hi"}
            ]}],
            "tools": []
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-rayline-selected-model"),
        Some("mock-model")
    );
    assert_eq!(header(&resp, "openai-model"), Some("rayline-local"));

    let events = collect_sse(resp).await;
    let types: Vec<&str> = events.iter().filter_map(|e| e["type"].as_str()).collect();
    assert!(types.contains(&"response.created"));
    assert!(types.contains(&"response.output_item.added"));
    assert!(types.contains(&"response.output_text.delta"));
    assert!(types.contains(&"response.output_item.done"));
    assert!(types.contains(&"response.completed"));
    let text: String = events
        .iter()
        .filter(|e| e["type"] == "response.output_text.delta")
        .filter_map(|e| e["delta"].as_str())
        .collect();
    assert_eq!(text, "Hello Codex");

    let _ = std::fs::remove_file(path);
    println!("PASS mock openai_responses passthrough: text={text:?}");
}

/// Codex subscription mode: Codex owns ChatGPT auth and sends it to Rayline's
/// custom Responses provider. Rayline must forward those client auth headers only
/// when the selected endpoint explicitly opts into `auth: client_bearer`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_responses_client_bearer_forwards_codex_subscription_headers() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_mock\",\"status\":\"in_progress\",\"model\":\"gpt-5.4\"}}\n\n",
        "event: response.output_text.delta\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_mock\",\"status\":\"completed\",\"model\":\"gpt-5.4\"}}\n\n",
    );
    tokio::spawn(serve_once_capture(upstream, http_sse(sse), tx));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "codex-subscription",
            "protocol": "openai_responses",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "auth": "client_bearer",
            "models": ["gpt-5.4"]
        }],
        "routes": {
            "main": {"endpoint": "codex-subscription", "model": "gpt-5.4"},
            "default": {"endpoint": "codex-subscription", "model": "gpt-5.4"},
            "model_routes": {
                "rayline-codex": {"endpoint": "codex-subscription", "model": "gpt-5.4"}
            }
        }
    });
    let path = write_config("openai-client-bearer", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("authorization", "Bearer codex-token")
        .header("chatgpt-account-id", "workspace-123")
        .header("x-openai-fedramp", "true")
        .header("version", "0.142.0")
        .json(&json!({
            "model": "rayline-codex",
            "stream": true,
            "input": "hi"
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    let events = collect_sse(resp).await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.completed")
    );

    let captured = rx.await.expect("captured upstream request");
    assert!(captured.starts_with("POST /responses "));
    assert!(captured.contains("authorization: Bearer codex-token\r\n"));
    assert!(captured.contains("chatgpt-account-id: workspace-123\r\n"));
    assert!(captured.contains("x-openai-fedramp: true\r\n"));
    assert!(captured.contains("version: 0.142.0\r\n"));
    assert!(captured.contains("\"model\":\"gpt-5.4\""));
    assert!(!captured.contains("\"model\":\"rayline-codex\""));

    let _ = std::fs::remove_file(path);
    println!("PASS mock openai_responses client bearer forwards subscription headers");
}

/// In client-bearer subscription mode `/v1/models` should be authoritative from
/// the Codex backend, while appending only the single Codex-default sentinel
/// `rayline-local` (what the CLI stamps as Codex's default `model`, so it must be
/// reselectable). The other sentinels (`rayline-router`/`rayline-subagent`
/// /`rayline-codex`) stay valid wire values but must not pollute the picker.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_endpoint_client_bearer_proxies_and_merges_rayline_models() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "models": [{
                "slug": "gpt-5.4",
                "display_name": "GPT-5.4",
                "supported_in_api": true
            }]
        })),
        tx,
    ));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "codex-subscription",
            "protocol": "openai_responses",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "auth": "client_bearer",
            "models": ["gpt-5.4"]
        }],
        "routes": {
            "main": {"endpoint": "codex-subscription", "model": "gpt-5.4"},
            "default": {"endpoint": "codex-subscription", "model": "gpt-5.4"}
        }
    });
    let path = write_config("models-client-bearer", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .get(format!(
            "http://127.0.0.1:{port}/v1/models?client_version=9.9.9"
        ))
        .header("authorization", "Bearer codex-token")
        .header("chatgpt-account-id", "workspace-123")
        .header("version", "0.142.0")
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    let body = resp.json::<Value>().await.unwrap();
    let slugs = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|model| model["slug"].as_str())
        .collect::<Vec<_>>();
    assert!(slugs.contains(&"gpt-5.4"));
    assert!(slugs.contains(&"rayline-local"));
    assert!(!slugs.contains(&"rayline-router"));
    assert!(!slugs.contains(&"rayline-codex"));
    assert!(!slugs.contains(&"rayline-subagent"));

    let captured = rx.await.expect("captured upstream request");
    assert!(captured.starts_with("GET /models?client_version=9.9.9 "));
    assert!(captured.contains("authorization: Bearer codex-token\r\n"));
    assert!(captured.contains("chatgpt-account-id: workspace-123\r\n"));
    assert!(captured.contains("version: 0.142.0\r\n"));

    let _ = std::fs::remove_file(path);
    println!("PASS mock models client bearer proxies and merges Rayline models");
}

/// Codex auxiliary Responses endpoint: native `openai_responses` routes should
/// pass `/v1/responses/compact` through to the upstream and rewrite the model to
/// the selected upstream model. This keeps native Codex backends authoritative
/// for non-turn endpoints while synthetic handling only applies to non-native
/// routes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn openai_responses_compact_passthrough_rewrites_model() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({"output": [{"type": "compaction", "encrypted_content": "summary"}]})),
        tx,
    ));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "mock-openai",
            "protocol": "openai_responses",
            "base_url": format!("http://127.0.0.1:{up_port}/v1"),
            "models": ["mock-model"]
        }],
        "routes": {
            "main": {"endpoint": "mock-openai", "model": "mock-model"},
            "default": {"endpoint": "mock-openai", "model": "mock-model"},
            "model_routes": {
                "rayline-local": {"endpoint": "mock-openai", "model": "mock-model"}
            }
        }
    });
    let path = write_config("openai-compact", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses/compact"))
        .json(&json!({
            "model": "rayline-local",
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "compact"}
            ]}]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Value>().await.unwrap()["output"][0]["encrypted_content"],
        "summary"
    );

    let captured = rx.await.expect("captured upstream request");
    assert!(captured.starts_with("POST /v1/responses/compact "));
    assert!(captured.contains("\"model\":\"mock-model\""));
    assert!(!captured.contains("\"model\":\"rayline-local\""));

    let _ = std::fs::remove_file(path);
    println!("PASS mock openai_responses compact passthrough rewrites model");
}

/// Latest Codex remote compaction uses the regular `/v1/responses` stream with a
/// `compaction_trigger` input item and expects exactly one `compaction` output
/// item. Non-native routes synthesize that Responses item from the upstream text.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_route_responses_compaction_trigger_emits_compaction_item() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    tokio::spawn(serve_once(upstream, anthropic_sse("compact summary")));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "anthropic-compatible",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["claude-mock"]
        }],
        "routes": {
            "main": {"endpoint": "anthropic-compatible", "model": "claude-mock"},
            "default": {"endpoint": "anthropic-compatible", "model": "claude-mock"},
            "model_routes": {
                "rayline-local": {"endpoint": "anthropic-compatible", "model": "claude-mock"}
            }
        }
    });
    let path = write_config("anthropic-responses-compact", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .json(&json!({
            "model": "rayline-local",
            "stream": true,
            "input": [
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "compact this"}
                ]},
                {"type": "compaction_trigger"}
            ]
        }))
        .send()
        .await
        .expect("router request");
    if resp.status() != 200 {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        panic!("unexpected status {status}: {body}");
    }

    let events = collect_sse(resp).await;
    let compaction = events
        .iter()
        .find(|event| event["type"] == "response.output_item.done")
        .expect("output item done");
    assert_eq!(compaction["item"]["type"], "compaction");
    assert_eq!(compaction["item"]["encrypted_content"], "compact summary");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.completed")
    );

    let _ = std::fs::remove_file(path);
    println!("PASS anthropic responses compaction trigger emits compaction item");
}

/// A minimal Anthropic SSE stream whose single text delta is `text`, so a test can
/// tell which upstream answered by reading the response text.
fn anthropic_sse(text: &str) -> String {
    let body = format!(
        "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"x\",\"content\":[],\"stop_reason\":null,\"usage\":{{\"input_tokens\":1,\"output_tokens\":0}}}}}}\n\n\
event: content_block_delta\ndata: {{\"type\":\"content_block_delta\",\"index\":0,\"delta\":{{\"type\":\"text_delta\",\"text\":\"{text}\"}}}}\n\n\
event: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"end_turn\"}},\"usage\":{{\"output_tokens\":1}}}}\n\n\
event: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
    );
    http_sse(&body)
}

fn sse_text(events: &[Value]) -> String {
    events
        .iter()
        .filter(|e| e["delta"]["type"] == "text_delta")
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect()
}

fn header<'a>(resp: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    resp.headers().get(name).and_then(|v| v.to_str().ok())
}

/// End-to-end (headless / agent path): a single `--config` with distinct `main`
/// and `subagent` routes must send the MAIN request to one upstream and a SUBAGENT
/// request to ANOTHER, over real HTTP. This is the SDK-through-the-router proof for
/// `rayline router start --config` — main vs subagent is classified per request by
/// the agent headers, exactly as the proxy sets them at runtime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_routes_main_and_subagent_to_distinct_endpoints() {
    let main_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let main_port = main_up.local_addr().unwrap().port();
    let sub_up = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let sub_port = sub_up.local_addr().unwrap().port();
    tokio::spawn(serve_once(main_up, anthropic_sse("MAIN-OK")));
    tokio::spawn(serve_once(sub_up, anthropic_sse("SUB-OK")));

    let port = free_port();
    let config = json!({
        "endpoints": [
            {"id": "main-ep", "protocol": "anthropic_messages",
             "base_url": format!("http://127.0.0.1:{main_port}"), "models": ["model-main"]},
            {"id": "sub-ep", "protocol": "anthropic_messages",
             "base_url": format!("http://127.0.0.1:{sub_port}"), "models": ["model-sub"]}
        ],
        "routes": {
            "main": {"endpoint": "main-ep", "model": "model-main"},
            "subagent": {"endpoint": "sub-ep", "model": "model-sub"}
        }
    });
    let path = write_config("config-both", &config);
    start_router(port, path.clone()).await;
    let client = reqwest::Client::new();

    // Main turn: no agent headers → routes.main → main-ep.
    let main_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-router", "stream": true, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("main request");
    assert_eq!(main_resp.status(), 200);
    assert_eq!(header(&main_resp, "x-rayline-task-class"), Some("main"));
    assert_eq!(
        header(&main_resp, "x-rayline-selected-model"),
        Some("model-main")
    );
    assert_eq!(sse_text(&collect_sse(main_resp).await), "MAIN-OK");

    // Subagent turn: agent-id + agent-type headers → routes.subagent → sub-ep.
    let sub_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-agent-id", "abc123")
        .header("x-rayline-claude-code-agent-type", "reviewer")
        .json(&json!({
            "model": "rayline-router", "stream": true, "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("subagent request");
    assert_eq!(sub_resp.status(), 200);
    assert_eq!(header(&sub_resp, "x-rayline-task-class"), Some("subagent"));
    assert_eq!(
        header(&sub_resp, "x-rayline-selected-model"),
        Some("model-sub")
    );
    assert_eq!(sse_text(&collect_sse(sub_resp).await), "SUB-OK");

    let _ = std::fs::remove_file(path);
    println!("PASS config drives main→main-ep, subagent→sub-ep over HTTP");
}

/// Single-model config (`routes.main` only, no `routes.subagent`): BOTH the main
/// turn and a subagent turn must reach the one configured endpoint/model over
/// real HTTP. Without the main-inheritance fallback, subagent turns would be
/// redirected to the on-device adapter the config never mentions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn main_only_config_routes_subagents_to_main_over_http() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    tokio::spawn(async move {
        // Two sequential requests (main, then subagent) on the same upstream.
        for _ in 0..2 {
            if let Ok((mut sock, _)) = upstream.accept().await {
                let mut buf = [0u8; 16384];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(anthropic_sse("ONE-MODEL-OK").as_bytes())
                    .await;
                let _ = sock.flush().await;
                let _ = sock.shutdown().await;
            }
        }
    });

    let port = free_port();
    let config = json!({
        "endpoints": [
            {"id": "solo", "protocol": "anthropic_messages",
             "base_url": format!("http://127.0.0.1:{upstream_port}"), "models": ["model-solo"]}
        ],
        "routes": {"main": {"endpoint": "solo", "model": "model-solo"}}
    });
    let path = write_config("config-main-only", &config);
    start_router(port, path.clone()).await;
    let client = reqwest::Client::new();

    let body = json!({
        "model": "rayline-router", "stream": true, "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });

    let main_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .expect("main request");
    assert_eq!(main_resp.status(), 200);
    assert_eq!(header(&main_resp, "x-rayline-task-class"), Some("main"));
    assert_eq!(
        header(&main_resp, "x-rayline-selected-model"),
        Some("model-solo")
    );
    assert_eq!(sse_text(&collect_sse(main_resp).await), "ONE-MODEL-OK");

    let sub_resp = client
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("x-claude-code-agent-id", "abc123")
        .header("x-rayline-claude-code-agent-type", "reviewer")
        .json(&body)
        .send()
        .await
        .expect("subagent request");
    assert_eq!(sub_resp.status(), 200);
    assert_eq!(header(&sub_resp, "x-rayline-task-class"), Some("subagent"));
    assert_eq!(
        header(&sub_resp, "x-rayline-selected-model"),
        Some("model-solo")
    );
    assert_eq!(sse_text(&collect_sse(sub_resp).await), "ONE-MODEL-OK");

    let _ = std::fs::remove_file(path);
    println!("PASS main-only config drives main AND subagents to the single endpoint");
}

/// Codex namespaces MCP tools with characters (`.`, `/`) that Anthropic's
/// Messages API and the providers behind it reject: forwarding them verbatim
/// fails the whole turn with a 400 the client shows as "Provider returned
/// error". The router must rewrite such names to the provider-safe charset on
/// the way upstream and restore the client's original name on the way back, so
/// Codex can still match the call to the MCP tool it declared.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_style_tool_names_are_provider_safe_upstream_and_restored_downstream() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    // The upstream answers with the name it was given (the rewritten one), the
    // way a real provider echoes the tool it picked.
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"codex_list_resource_templates\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"cursor\\\":null}\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(upstream, http_sse(sse), tx));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "mock-anthropic",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["model-solo"]
        }],
        "routes": {
            "main": {"endpoint": "mock-anthropic", "model": "model-solo"},
            "model_routes": {
                "rayline-local": {"endpoint": "mock-anthropic", "model": "model-solo"}
            }
        }
    });
    let path = write_config("mcp-tool-names", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .json(&json!({
            "model": "rayline-local",
            "stream": true,
            "input": [{"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "list the templates"}
            ]}],
            "tools": [{
                "type": "function",
                "name": "codex.list_resource_templates",
                "description": "List MCP resource templates",
                "parameters": {"type": "object", "properties": {}}
            }]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);

    let events = collect_sse(resp).await;
    let call = events
        .iter()
        .find(|e| e["type"] == "response.output_item.done" && e["item"]["type"] == "function_call")
        .expect("a function_call item");
    assert_eq!(call["item"]["name"], "codex.list_resource_templates");
    assert_eq!(call["item"]["call_id"], "toolu_1");
    assert_eq!(call["item"]["arguments"], "{\"cursor\":null}");

    let captured = rx.await.expect("captured upstream request");
    let body = captured
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    let sent = serde_json::from_str::<Value>(body).expect("upstream request body");
    let sent_name = sent["tools"][0]["name"]
        .as_str()
        .expect("upstream tool name");
    assert_eq!(sent_name, "codex_list_resource_templates");
    assert!(
        !captured.contains("codex.list_resource_templates"),
        "the provider-rejected name must not reach upstream"
    );

    let _ = std::fs::remove_file(path);
    println!("PASS MCP tool name rewritten upstream as {sent_name:?} and restored downstream");
}

/// Claude Code shapes its body for the sentinel it was told about; when the
/// selected model is a Claude 5 family the router must translate the legacy
/// thinking block and drop the deprecated sampling params BEFORE the bytes go
/// upstream. Streaming path, with the provider-prefixed OpenRouter id.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_messages_adapts_body_for_claude_5_upstream() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(upstream, anthropic_sse("ok"), tx));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "openrouter-mock",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["anthropic/claude-sonnet-5"]
        }],
        "routes": {
            "main": {"endpoint": "openrouter-mock", "model": "anthropic/claude-sonnet-5"},
            "default": {"endpoint": "openrouter-mock", "model": "anthropic/claude-sonnet-5"}
        }
    });
    let path = write_config("anthropic-adapt-claude-5", &config);
    start_router(port, path.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": "rayline-router",
            "stream": true,
            "max_tokens": 2048,
            "temperature": 1,
            "top_p": 0.95,
            "top_k": 5,
            "thinking": {"type": "enabled", "budget_tokens": 5000},
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    let events = collect_sse(resp).await;
    assert_eq!(sse_text(&events), "ok");

    let captured = rx.await.expect("captured upstream request");
    let body = captured
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    let sent = serde_json::from_str::<Value>(body).expect("upstream request body");
    assert_eq!(sent["model"], "anthropic/claude-sonnet-5");
    assert_eq!(sent["thinking"], json!({"type": "adaptive"}));
    assert_eq!(sent["output_config"]["effort"], "high");
    for key in ["temperature", "top_p", "top_k"] {
        assert!(
            sent.get(key).is_none(),
            "{key} must not reach a Claude 5 upstream"
        );
    }

    let _ = std::fs::remove_file(path);
    println!("PASS Claude 5 body adapted upstream: {sent}");
}

/// The same body routed to a legacy family is forwarded byte-for-byte: the
/// adaptation is gated on the selected model, not applied blindly. Non-streaming
/// path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn anthropic_messages_leaves_legacy_family_body_untouched() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    let reply = json!({
        "id": "msg_mock", "type": "message", "role": "assistant", "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": "ok"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    });
    tokio::spawn(serve_once_capture(upstream, http_json(&reply), tx));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "anthropic-mock",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["claude-haiku-4-5"]
        }],
        "routes": {
            "main": {"endpoint": "anthropic-mock", "model": "claude-haiku-4-5"},
            "default": {"endpoint": "anthropic-mock", "model": "claude-haiku-4-5"}
        }
    });
    let path = write_config("anthropic-legacy-untouched", &config);
    start_router(port, path.clone()).await;

    let request = json!({
        "model": "rayline-router",
        "max_tokens": 2048,
        "temperature": 1,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [{"role": "user", "content": "hi"}]
    });
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .json(&request)
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    let reply_seen = resp.json::<Value>().await.expect("json reply");
    assert_eq!(reply_seen["content"][0]["text"], "ok");

    let captured = rx.await.expect("captured upstream request");
    let body = captured
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or_default();
    let sent = serde_json::from_str::<Value>(body).expect("upstream request body");
    let mut expected = request.clone();
    expected["model"] = json!("claude-haiku-4-5");
    assert_eq!(sent, expected);

    let _ = std::fs::remove_file(path);
    println!("PASS legacy family body forwarded untouched: {sent}");
}

/// OpenAI-compatible-only harnesses speak Chat Completions. On an Anthropic
/// route the router translates the request and answers in Chat Completions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_completions_translate_to_anthropic_and_back() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "tool_use", "id": "call_1", "name": "shell", "input": {"cmd": "ls"}}],
            "stop_reason": "tool_use", "usage": {"input_tokens": 9, "output_tokens": 2}
        })),
        tx,
    ));
    let config = write_config(
        "chat-inbound-anthropic",
        &json!({
            "endpoints": [{"id": "up", "protocol": "anthropic_messages",
                           "base_url": format!("http://127.0.0.1:{upstream_port}"),
                           "models": ["m"]}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    );
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"model": "m", "messages": [
                          {"role": "system", "content": "Be brief."},
                          {"role": "user", "content": "list files"}],
                      "tools": [{"type": "function", "function": {"name": "shell",
                                 "parameters": {"type": "object", "properties": {"cmd": {"type": "string"}}}}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let completion: Value = response.json().await.unwrap();
    assert_eq!(completion["object"], "chat.completion");
    let choice = &completion["choices"][0];
    assert_eq!(choice["finish_reason"], "tool_calls");
    assert_eq!(
        choice["message"]["tool_calls"][0]["function"]["name"],
        "shell"
    );
    assert_eq!(
        choice["message"]["tool_calls"][0]["function"]["arguments"],
        "{\"cmd\":\"ls\"}"
    );
    let forwarded = rx.await.expect("upstream saw the request");
    assert!(forwarded.starts_with("POST /v1/messages"), "{forwarded}");
    let sent: Value =
        serde_json::from_str(forwarded.split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(sent["system"], "Be brief.");
    assert_eq!(sent["messages"][0]["content"][0]["text"], "list files");
    assert_eq!(sent["tools"][0]["name"], "shell");
    let _ = std::fs::remove_file(config);
}

/// On an OpenAI Chat route the request passes through with the routed model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_completions_pass_through_to_openai_chat() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "chatcmpl_1", "object": "chat.completion", "created": 1, "model": "routed",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "PONG"},
                         "finish_reason": "stop"}]
        })),
        tx,
    ));
    let config = write_config(
        "chat-inbound-openai",
        &json!({
            "endpoints": [{"id": "up", "protocol": "openai_chat",
                           "base_url": format!("http://127.0.0.1:{upstream_port}/v1"),
                           "models": ["routed"]}],
            "routes": {"main": {"endpoint": "up", "model": "routed"},
                       "default": {"endpoint": "up", "model": "routed"}}
        }),
    );
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"model": "anything", "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let completion: Value = response.json().await.unwrap();
    assert_eq!(completion["choices"][0]["message"]["content"], "PONG");
    let forwarded = rx.await.expect("upstream saw the request");
    assert!(
        forwarded.starts_with("POST /v1/chat/completions"),
        "{forwarded}"
    );
    let sent: Value =
        serde_json::from_str(forwarded.split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(sent["model"], "routed");
    let _ = std::fs::remove_file(config);
}

/// Requests a web page could send to the loopback router (cross-site Origin,
/// DNS-rebound Host, CORS "simple" POST) are refused before they reach the
/// provider endpoint, so they cannot spend its stored credentials.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn browser_reachable_requests_never_reach_the_provider() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "local-openai",
            "protocol": "openai_chat",
            "base_url": format!("http://127.0.0.1:{up_port}/v1"),
            "models": ["local-model-x"]
        }],
        "routes": {
            "main": {"endpoint": "local-openai", "model": "local-model-x"},
            "default": {"endpoint": "local-openai", "model": "local-model-x"}
        }
    });
    let path = write_config("request-guard", &config);
    start_router(port, path.clone()).await;

    let url = format!("http://127.0.0.1:{port}/v1/messages");
    let body = json!({"model": "rayline-router", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    let client = reqwest::Client::new();
    let cross_origin = client
        .post(&url)
        .header("origin", "https://evil.example")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(cross_origin.status(), 403);
    let rebound = client
        .post(&url)
        .header("host", format!("evil.example:{port}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(rebound.status(), 421);
    let simple_post = client
        .post(&url)
        .header("content-type", "text/plain")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(simple_post.status(), 415);

    let reached = tokio::time::timeout(Duration::from_millis(300), upstream.accept()).await;
    assert!(reached.is_err(), "a guarded request reached the provider");
    let _ = std::fs::remove_file(path);
}

/// Accept one connection, drain the request, then write `pieces` one at a time
/// with a pause between them, so the router receives them as separate chunks.
/// The connection is dropped after the last piece.
async fn serve_pieces(listener: TcpListener, pieces: Vec<Vec<u8>>) {
    if let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 16384];
        let _ = sock.read(&mut buf).await;
        for piece in pieces {
            let _ = sock.write_all(&piece).await;
            let _ = sock.flush().await;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// A one-route config whose only endpoint is `endpoint` (id `up`, model `m`).
fn chat_route_config(name: &str, endpoint: Value) -> PathBuf {
    write_config(
        name,
        &json!({
            "endpoints": [endpoint],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    )
}

fn anthropic_endpoint(upstream_port: u16) -> Value {
    json!({"id": "up", "protocol": "anthropic_messages",
           "base_url": format!("http://127.0.0.1:{upstream_port}"), "models": ["m"]})
}

/// POST a streaming Chat Completions request and read the raw body, returning
/// it with whether the body ended in a transport error.
async fn stream_chat(port: u16) -> (String, bool) {
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"model": "m", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut body = Vec::new();
    let mut aborted = false;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => body.extend_from_slice(&bytes),
            Err(_) => {
                aborted = true;
                break;
            }
        }
    }
    (String::from_utf8(body).expect("utf-8 body"), aborted)
}

/// The text a Chat chunk stream carries.
fn chat_stream_text(body: &str) -> String {
    body.split("\n\n")
        .filter_map(|event| event.strip_prefix("data: "))
        .filter_map(|data| serde_json::from_str::<Value>(data).ok())
        .filter_map(|chunk| {
            chunk["choices"][0]["delta"]["content"]
                .as_str()
                .map(str::to_owned)
        })
        .collect()
}

const SSE_HEAD: &str =
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
const SSE_START: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1}}}\n\n";
const SSE_STOP: &str = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

fn sse_text_delta(text: &str) -> String {
    format!(
        "event: content_block_delta\ndata: {}\n\n",
        json!({"type": "content_block_delta", "index": 0,
               "delta": {"type": "text_delta", "text": text}})
    )
}

/// An upstream that drops the connection mid-answer must not look like a
/// finished completion: no finish chunk, no `[DONE]`, and the client sees the
/// failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_stream_reports_upstream_disconnect() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    // A chunked body cut off before its terminating chunk is a transport error.
    let event = format!("{SSE_START}{}", sse_text_delta("partial"));
    tokio::spawn(serve_pieces(
        upstream,
        vec![
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n"
                .to_vec(),
            format!("{:x}\r\n{event}\r\n", event.len()).into_bytes(),
        ],
    ));
    let config = chat_route_config("chat-disconnect", anthropic_endpoint(upstream_port));
    let port = free_port();
    start_router(port, config.clone()).await;
    let (body, aborted) = stream_chat(port).await;
    assert_eq!(chat_stream_text(&body), "partial");
    assert!(!body.contains("[DONE]"), "{body}");
    assert!(!body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(aborted || body.contains("\"error\""), "{body}");
    let _ = std::fs::remove_file(config);
}

/// A connection closed cleanly before `message_stop`, and an SSE `error`
/// event, both end the Chat stream with an error instead of `[DONE]`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_stream_reports_upstream_error_event_and_early_eof() {
    for (name, tail, expected) in [
        (
            "error-event",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
            "overloaded_error",
        ),
        ("early-eof", "", "ended before the response completed"),
    ] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        tokio::spawn(serve_once(
            upstream,
            format!("{SSE_HEAD}{SSE_START}{}{tail}", sse_text_delta("partial")),
        ));
        let config = chat_route_config(name, anthropic_endpoint(upstream_port));
        let port = free_port();
        start_router(port, config.clone()).await;
        let (body, _) = stream_chat(port).await;
        assert_eq!(chat_stream_text(&body), "partial", "{name}");
        assert!(!body.contains("[DONE]"), "{name}: {body}");
        assert!(body.contains(expected), "{name}: {body}");
        let _ = std::fs::remove_file(config);
    }
}

/// A multi-byte character split across upstream chunks reaches the client
/// intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_stream_keeps_utf8_split_across_chunks() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let bytes = format!("{SSE_START}{}{SSE_STOP}", sse_text_delta("café")).into_bytes();
    let split = bytes
        .windows(2)
        .position(|pair| pair == "é".as_bytes())
        .expect("é in body")
        + 1;
    tokio::spawn(serve_pieces(
        upstream,
        vec![
            SSE_HEAD.as_bytes().to_vec(),
            bytes[..split].to_vec(),
            bytes[split..].to_vec(),
        ],
    ));
    let config = chat_route_config("chat-utf8", anthropic_endpoint(upstream_port));
    let port = free_port();
    start_router(port, config.clone()).await;
    let (body, aborted) = stream_chat(port).await;
    assert!(!aborted);
    assert_eq!(chat_stream_text(&body), "café");
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    let _ = std::fs::remove_file(config);
}

/// CRLF-delimited SSE (valid per the SSE spec) is translated like LF.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_stream_accepts_crlf_sse() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let body = format!("{SSE_START}{}{SSE_STOP}", sse_text_delta("hello")).replace('\n', "\r\n");
    tokio::spawn(serve_once(upstream, format!("{SSE_HEAD}{body}")));
    let config = chat_route_config("chat-crlf", anthropic_endpoint(upstream_port));
    let port = free_port();
    start_router(port, config.clone()).await;
    let (body, aborted) = stream_chat(port).await;
    assert!(!aborted);
    assert_eq!(chat_stream_text(&body), "hello");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(body.ends_with("data: [DONE]\n\n"), "{body}");
    let _ = std::fs::remove_file(config);
}

/// An `auth: client_bearer` OpenAI Chat endpoint receives the caller's own
/// Authorization and account headers, like the Responses passthrough.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_client_bearer_forwards_caller_auth() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "chatcmpl_1", "object": "chat.completion", "created": 1, "model": "m",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                         "finish_reason": "stop"}]
        })),
        tx,
    ));
    let config = chat_route_config(
        "chat-client-bearer",
        json!({"id": "up", "protocol": "openai_chat", "auth": "client_bearer",
               "base_url": format!("http://127.0.0.1:{upstream_port}/v1"), "models": ["m"]}),
    );
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("authorization", "Bearer caller-token")
        .header("chatgpt-account-id", "workspace-123")
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let captured = rx.await.expect("upstream saw the request");
    assert!(
        captured.starts_with("POST /v1/chat/completions "),
        "{captured}"
    );
    assert!(
        captured.contains("authorization: Bearer caller-token\r\n"),
        "{captured}"
    );
    assert!(
        captured.contains("chatgpt-account-id: workspace-123\r\n"),
        "{captured}"
    );
    assert_eq!(captured.matches("content-type:").count(), 1, "{captured}");
    let _ = std::fs::remove_file(config);
}

/// An OpenAI Chat upstream's error status and end-to-end headers (here a
/// 429's Retry-After) reach the Chat client unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_passthrough_keeps_upstream_headers() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let body = json!({"error": {"message": "slow down", "type": "rate_limit_error"}}).to_string();
    tokio::spawn(serve_once(
        upstream,
        format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 120\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    ));
    let config = chat_route_config(
        "chat-headers",
        json!({"id": "up", "protocol": "openai_chat",
               "base_url": format!("http://127.0.0.1:{upstream_port}/v1"), "models": ["m"]}),
    );
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 429);
    assert_eq!(header(&response, "retry-after"), Some("120"));
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["message"], "slow down");
    let _ = std::fs::remove_file(config);
}

/// A route to an `openai_responses` endpoint is refused with an OpenAI-shaped
/// 400 instead of POSTing Chat JSON to a Responses-only backend.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_refuses_openai_responses_endpoint() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let config = chat_route_config(
        "chat-responses",
        json!({"id": "up", "protocol": "openai_responses",
               "base_url": format!("http://127.0.0.1:{upstream_port}/v1"), "models": ["m"]}),
    );
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .timeout(Duration::from_secs(5))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .expect("refused without waiting on the endpoint");
    assert_eq!(response.status(), 400);
    let error: Value = response.json().await.unwrap();
    assert_eq!(error["error"]["code"], "unsupported_route");
    let reached = tokio::time::timeout(Duration::from_millis(300), upstream.accept()).await;
    assert!(reached.is_err(), "the Responses endpoint was called");
    let _ = std::fs::remove_file(config);
}

/// The #100 browser guard covers the Chat listener: a CORS "simple" text/plain
/// POST is refused before it reaches the provider.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_refuses_text_plain_post() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let config = chat_route_config("chat-text-plain", anthropic_endpoint(upstream_port));
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("content-type", "text/plain")
        .body(json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 415);
    let reached = tokio::time::timeout(Duration::from_millis(300), upstream.accept()).await;
    assert!(reached.is_err(), "a text/plain POST reached the provider");
    let _ = std::fs::remove_file(config);
}
