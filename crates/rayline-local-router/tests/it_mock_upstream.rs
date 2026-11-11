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
            "model": "rayline-auto",
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
            "model": "rayline-auto",
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
/// reselectable). The other sentinels (`rayline-auto`/`rayline-subagent`
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
                "supported_in_api": true,
                "context_window": 272000,
                "max_context_window": 1000000
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
            "default": {"endpoint": "codex-subscription", "model": "gpt-5.4"},
            // As `raydock codex` writes it: the sentinel pinned to main.
            "model_routes": {"rayline-local": {"endpoint": "codex-subscription", "model": "gpt-5.4"}}
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
    assert!(!slugs.contains(&"rayline-auto"));
    assert!(!slugs.contains(&"rayline-codex"));
    assert!(!slugs.contains(&"rayline-subagent"));
    // The sentinel routes to gpt-5.4, so it takes the window the backend lists.
    let sentinel = body["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|model| model["slug"] == "rayline-local")
        .unwrap();
    assert_eq!(sentinel["context_window"], 272000);
    // The subscription model's override cap is kept, not lowered to its default.
    assert_eq!(sentinel["max_context_window"], 1000000);

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

/// `response` (a raw HTTP/1.1 reply) with one more header after the status line.
fn with_header(response: &str, header: &str) -> String {
    response.replacen("\r\n", &format!("\r\n{header}\r\n"), 1)
}

/// Start a router that reports into `metrics`, and wait for it to be healthy.
async fn start_router_with_metrics(
    port: u16,
    config_path: PathBuf,
    metrics: std::sync::Arc<rayline_metrics::RouterMetrics>,
) {
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config_path),
        metrics: Some(metrics),
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
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("router on port {port} did not become healthy");
}

/// The `upstream_request_id` the metrics hold for `request_id`, whether the
/// request is still streaming or already finished.
fn metrics_upstream_request_id(
    metrics: &rayline_metrics::RouterMetrics,
    request_id: &str,
) -> Option<String> {
    let snapshot = metrics.snapshot();
    snapshot
        .active
        .iter()
        .chain(snapshot.recent.iter())
        .find(|row| row.request_id == request_id)
        .and_then(|row| row.upstream_request_id.clone())
}

/// The finished (`recent`) metrics row for `request_id`, if the request is done.
fn recent_metrics_row(
    metrics: &rayline_metrics::RouterMetrics,
    request_id: &str,
) -> Option<rayline_metrics::RequestSnapshot> {
    metrics
        .snapshot()
        .recent
        .into_iter()
        .find(|row| row.request_id == request_id)
}

fn anthropic_endpoint_config(name: &str, up_port: u16) -> PathBuf {
    write_config(
        name,
        &json!({
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
        }),
    )
}

/// The cloud router names each forwarded request in `x-rayline-route-id`. A
/// passthrough forward keeps that id on the response it returns and on the
/// request's metrics row, so `raydock top` and the log can name the cloud route a
/// local request became; the id is on the headers, so a streaming response needs
/// no buffering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forwarded_anthropic_request_keeps_the_upstream_request_id() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    tokio::spawn(serve_once(
        upstream,
        with_header(&anthropic_sse("Hi"), "x-rayline-route-id: rt_passthrough"),
    ));
    let path = anthropic_endpoint_config("upstream-id-passthrough", up_port);
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    start_router_with_metrics(port, path.clone(), metrics.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/messages"))
        .header("anthropic-version", "2023-06-01")
        .header("x-rayline-request-id", "req_passthrough")
        .json(&json!({
            "model": "rayline-auto",
            "stream": true,
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-rayline-upstream-request-id"),
        Some("rt_passthrough")
    );
    assert_eq!(
        metrics_upstream_request_id(&metrics, "req_passthrough").as_deref(),
        Some("rt_passthrough"),
        "the metrics row carries the upstream id as soon as the headers are in"
    );
    let events = collect_sse(resp).await;
    assert_eq!(sse_text(&events), "Hi");
    let _ = std::fs::remove_file(path);
}

/// A translated forward (Codex Responses in, OpenAI Chat out) builds a fresh
/// response, so the id has to be carried over explicitly. This
/// translation completes the metrics row before the handler returns, so the id
/// must be on the finished row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translated_responses_request_keeps_the_upstream_request_id() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let completion = json!({
        "id": "chatcmpl-mock", "object": "chat.completion", "model": "m",
        "choices": [{"index": 0, "finish_reason": "stop",
                     "message": {"role": "assistant", "content": "translated"}}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    });
    tokio::spawn(serve_once(
        upstream,
        with_header(&http_json(&completion), "x-rayline-route-id: rt_legacy"),
    ));
    let path = write_config(
        "upstream-id-translated",
        &json!({
            "endpoints": [{"id": "up", "protocol": "openai_chat",
                           "base_url": format!("http://127.0.0.1:{up_port}/v1"), "models": ["m"]}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    );
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    start_router_with_metrics(port, path.clone(), metrics.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("x-rayline-request-id", "req_translated")
        .json(&json!({"model": "rayline-auto", "stream": false, "input": "hi"}))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-rayline-upstream-request-id"),
        Some("rt_legacy")
    );
    let row = recent_metrics_row(&metrics, "req_translated").expect("the row is finished");
    assert_eq!(row.state, "completed");
    assert_eq!(row.upstream_request_id.as_deref(), Some("rt_legacy"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["output"][0]["content"][0]["text"], "translated");
    let _ = std::fs::remove_file(path);
}

/// Codex's compaction side call is forwarded as a one-shot Anthropic request and
/// answered with a synthetic Responses body; the cloud's id rides along on the
/// response and the metrics row all the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_compaction_call_keeps_the_upstream_request_id() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let message = json!({
        "id": "msg_mock", "type": "message", "role": "assistant", "model": "claude-mock",
        "content": [{"type": "text", "text": "compact summary"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1}
    });
    tokio::spawn(serve_once(
        upstream,
        with_header(&http_json(&message), "x-rayline-route-id: rt_compact"),
    ));
    let path = anthropic_endpoint_config("upstream-id-compact", up_port);
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    start_router_with_metrics(port, path.clone(), metrics.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses/compact"))
        .header("x-rayline-request-id", "req_compact")
        .json(&json!({"model": "rayline-auto", "input": "compact this"}))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    assert_eq!(
        header(&resp, "x-rayline-upstream-request-id"),
        Some("rt_compact")
    );
    let row = recent_metrics_row(&metrics, "req_compact").expect("the row is finished");
    assert_eq!(row.upstream_request_id.as_deref(), Some("rt_compact"));
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["output"][0]["encrypted_content"], "compact summary");
    let _ = std::fs::remove_file(path);
}

/// The id is recorded when the upstream headers arrive, so an answer whose body
/// then fails to translate still leaves it on the errored metrics row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upstream_request_id_survives_a_body_that_fails_to_translate() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let broken = "HTTP/1.1 200 OK\r\nx-rayline-route-id: rt_broken\r\ncontent-type: application/json\r\ncontent-length: 9\r\nconnection: close\r\n\r\nnot json!";
    tokio::spawn(serve_once(upstream, broken.to_owned()));
    let path = write_config(
        "upstream-id-broken",
        &json!({
            "endpoints": [{"id": "up", "protocol": "openai_chat",
                           "base_url": format!("http://127.0.0.1:{up_port}/v1"), "models": ["m"]}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    );
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    start_router_with_metrics(port, path.clone(), metrics.clone()).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .header("x-rayline-request-id", "req_broken")
        .json(&json!({"model": "rayline-auto", "stream": false, "input": "hi"}))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 502);
    let row = recent_metrics_row(&metrics, "req_broken").expect("the row is finished");
    assert_eq!(row.state, "error");
    assert_eq!(row.upstream_request_id.as_deref(), Some("rt_broken"));
    let _ = std::fs::remove_file(path);
}

/// Chat Completions translated to Anthropic Messages carries the id too, and a
/// local-only answer (an upstream that sends no id) adds no header at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn translated_chat_request_keeps_the_upstream_request_id_when_sent() {
    let message = json!({
        "id": "msg_mock", "type": "message", "role": "assistant", "model": "claude-mock",
        "content": [{"type": "text", "text": "chat"}],
        "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1}
    });
    for upstream_header in [Some("x-rayline-route-id: rt_chat"), None] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        let reply = http_json(&message);
        tokio::spawn(serve_once(
            upstream,
            upstream_header.map_or_else(|| reply.clone(), |h| with_header(&reply, h)),
        ));
        let path = anthropic_endpoint_config("upstream-id-chat", up_port);
        let port = free_port();
        start_router(port, path.clone()).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&json!({"model": "rayline-auto",
                          "messages": [{"role": "user", "content": "hi"}]}))
            .send()
            .await
            .expect("router request");
        assert_eq!(resp.status(), 200);
        assert_eq!(
            header(&resp, "x-rayline-upstream-request-id"),
            upstream_header.map(|_| "rt_chat")
        );
        let body: Value = resp.json().await.unwrap();
        assert_eq!(body["choices"][0]["message"]["content"], "chat");
        let _ = std::fs::remove_file(path);
    }
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
            "model": "rayline-auto", "stream": true, "max_tokens": 16,
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
            "model": "rayline-auto", "stream": true, "max_tokens": 16,
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
        "model": "rayline-auto", "stream": true, "max_tokens": 16,
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
            "model": "rayline-auto",
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
        "model": "rayline-auto",
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

/// Codex `web_search = "live"` through a Responses → Anthropic route: the
/// hosted `web_search` tool reaches the upstream as Anthropic's server tool
/// (never silently dropped), and a hosted tool with no Anthropic equivalent is
/// refused with an actionable `unsupported_tool` before any upstream
/// call.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_hosted_tools_reach_anthropic_or_are_refused() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "Searched."}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
        tx,
    ));
    let config = write_config(
        "hosted-tools",
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
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/responses");

    let refused = client
        .post(&url)
        .json(&json!({"model": "m", "input": "hi", "stream": false,
                      "tools": [{"type": "web_search"}, {"type": "code_interpreter"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unsupported_tool");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("code_interpreter")
    );

    // Any tool type outside the translation's allowlist is refused, under
    // automatic and forced tool choice alike, and so is a function the model
    // may only call from a program.
    let mut refusals = Vec::new();
    for kind in ["computer", "programmatic_tool_calling", "made_up_tool_2031"] {
        for tool_choice in [json!("auto"), json!({"type": kind})] {
            refusals.push(json!({"model": "m", "input": "hi", "stream": false,
                "tool_choice": tool_choice,
                "tools": [{"type": "function", "name": "shell"}, {"type": kind}]}));
        }
    }
    refusals.push(json!({"model": "m", "input": "hi", "stream": false,
        "tools": [{"type": "function", "name": "delete_all",
                   "allowed_callers": ["programmatic"]}]}));
    for request in refusals {
        let refused = client.post(&url).json(&request).send().await.unwrap();
        assert_eq!(refused.status(), 400, "{request}");
        let body: Value = refused.json().await.unwrap();
        assert_eq!(
            body["error"]["code"], "unsupported_tool",
            "{request} {body}"
        );
    }

    // Settings Anthropic's search cannot honour, and a required call with no
    // tool left to make it, are refused before any upstream call.
    for (request, code) in [
        (
            json!({"model": "m", "input": "hi", "stream": false,
                   "tools": [{"type": "web_search", "filters": {
                       "allowed_domains": ["a.example"], "blocked_domains": ["b.a.example"]}}]}),
            "unsupported_web_search",
        ),
        (
            json!({"model": "m", "input": "hi", "stream": false, "tool_choice": "required",
                   "tools": [{"type": "web_search", "external_web_access": false}]}),
            "required_tool_unavailable",
        ),
    ] {
        let refused = client.post(&url).json(&request).send().await.unwrap();
        assert_eq!(refused.status(), 400);
        let body: Value = refused.json().await.unwrap();
        assert_eq!(body["error"]["code"], code, "{body}");
    }

    let ok = client
        .post(&url)
        .json(
            &json!({"model": "m", "input": "What changed in Rust 1.88?", "stream": false,
                      "tools": [{"type": "web_search",
                                 "filters": {"allowed_domains": ["blog.rust-lang.org"]}}]}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let forwarded = rx.await.expect("upstream saw the request");
    let body = forwarded.split("\r\n\r\n").nth(1).expect("request body");
    let sent: Value = serde_json::from_str(body).expect("json body");
    assert_eq!(
        sent["tools"],
        json!([{"type": "web_search_20250305", "name": "web_search",
                "allowed_domains": ["blog.rust-lang.org"]}])
    );
    let _ = std::fs::remove_file(config);
}

/// Codex declares its sub-agent functions inside a Responses `namespace`
/// tool. Through a Responses → Anthropic route they must pass the tool
/// allowlist, reach the upstream by qualified name (a choice forcing one by
/// namespace and name included), and come back under the client's namespace
/// and name. A Chat Completions route cannot carry them and refuses them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_namespace_tools_reach_anthropic_or_are_refused() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "tool_use", "id": "toolu_1",
                         "name": "multi_agent_v1__spawn_agent",
                         "input": {"message": "hi"}}],
            "stop_reason": "tool_use", "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
        tx,
    ));
    let config = write_config(
        "namespace-tools",
        &json!({
            "endpoints": [{"id": "up", "protocol": "anthropic_messages",
                           "base_url": format!("http://127.0.0.1:{upstream_port}"),
                           "models": ["m"]}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    );
    let chat_config = write_config(
        "namespace-tools-chat",
        &json!({
            "endpoints": [{"id": "chat", "protocol": "openai_chat",
                           "base_url": "http://127.0.0.1:9", "models": ["m"]}],
            "routes": {"main": {"endpoint": "chat", "model": "m"},
                       "default": {"endpoint": "chat", "model": "m"}}
        }),
    );
    let port = free_port();
    start_router(port, config.clone()).await;
    let chat_port = free_port();
    start_router(chat_port, chat_config.clone()).await;
    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/v1/responses");
    let chat_url = format!("http://127.0.0.1:{chat_port}/v1/responses");
    let request = || {
        json!({"model": "m", "input": "spawn a helper", "stream": false,
        "tool_choice": {"type": "function", "namespace": "multi_agent_v1",
                        "name": "spawn_agent"},
        "tools": [
            {"type": "function", "name": "shell"},
            {"type": "namespace", "name": "multi_agent_v1",
             "description": "Sub-agents.",
             "tools": [{"type": "function", "name": "spawn_agent",
                        "parameters": {"type": "object", "properties": {
                            "message": {"type": "string"}}}}]}
        ]})
    };

    let refused = client
        .post(&chat_url)
        .json(&request())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let body: Value = refused.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unsupported_tool", "{body}");
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("namespace"),
        "{body}"
    );

    let ok = client.post(&url).json(&request()).send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let response: Value = ok.json().await.unwrap();
    let forwarded = rx.await.expect("upstream saw the request");
    let body = forwarded.split("\r\n\r\n").nth(1).expect("request body");
    let sent: Value = serde_json::from_str(body).expect("json body");
    let names: Vec<&str> = sent["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert_eq!(names, ["shell", "multi_agent_v1__spawn_agent"]);
    assert_eq!(sent["tool_choice"]["type"], "tool");
    assert_eq!(sent["tool_choice"]["name"], "multi_agent_v1__spawn_agent");

    let call = response["output"]
        .as_array()
        .expect("output")
        .iter()
        .find(|item| item["type"] == "function_call")
        .unwrap_or_else(|| panic!("function_call in {response}"));
    assert_eq!(call["name"], "spawn_agent");
    assert_eq!(call["namespace"], "multi_agent_v1");
    let _ = std::fs::remove_file(config);
    let _ = std::fs::remove_file(chat_config);
}

/// A request refused for a hosted tool the route cannot run is announced to
/// metrics like any other; it must also be closed out there, or `raydock top`
/// shows it as active forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refused_hosted_tool_request_is_not_left_active_in_metrics() {
    let config = write_config(
        "hosted-tools-metrics",
        &json!({
            "endpoints": [{"id": "up", "protocol": "anthropic_messages",
                           "base_url": "http://127.0.0.1:9", "models": ["m"]}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    );
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        metrics: Some(metrics.clone()),
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
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let refused = client
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .json(&json!({"model": "m", "input": "hi", "stream": false,
                      "tools": [{"type": "code_interpreter", "container": {"type": "auto"}}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 400);
    let totals = metrics.snapshot().totals;
    assert_eq!(totals.active_requests, 0);
    assert_eq!(totals.errored_requests, 1);
    let _ = std::fs::remove_file(config);
}

/// A complete (non-streaming) Anthropic message with a single text block and
/// authoritative token counts, as an `anthropic_messages` upstream returns for
/// `stream: false`.
fn anthropic_json_message(text: &str) -> Value {
    json!({
        "id": "msg_mock", "type": "message", "role": "assistant", "model": "x",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 42, "output_tokens": 17, "cache_read_input_tokens": 5}
    })
}

/// Send one `/v1/responses` turn with the given `stream` flag through a router
/// that owns its metrics sink and routes everything to an `anthropic_messages`
/// upstream answering `upstream` (one raw HTTP response). Returns the metrics
/// snapshot taken after the client has read the whole answer.
async fn responses_turn_metrics(
    name: &str,
    upstream: String,
    want_stream: bool,
) -> rayline_metrics::MetricsSnapshot {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = listener.local_addr().unwrap().port();
    tokio::spawn(serve_once(listener, upstream));

    let config = write_config(
        name,
        &json!({
            "endpoints": [{"id": "up", "protocol": "anthropic_messages",
                           "base_url": format!("http://127.0.0.1:{up_port}"),
                           "models": ["claude-mock"]}],
            "routes": {"main": {"endpoint": "up", "model": "claude-mock"},
                       "default": {"endpoint": "up", "model": "claude-mock"},
                       "model_routes": {
                           "rayline-local": {"endpoint": "up", "model": "claude-mock"}
                       }}
        }),
    );
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        metrics: Some(metrics.clone()),
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
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let resp = client
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .json(&json!({"model": "rayline-local", "input": "hi", "stream": want_stream}))
        .send()
        .await
        .expect("router request");
    let status = resp.status();
    // Drain the answer so a streamed body cannot be mistaken for an open row.
    let body = resp.text().await.expect("response body");
    assert_eq!(status, 200, "unexpected status {status}: {body}");
    assert!(
        body.contains("\"usage\""),
        "translated answer should carry usage: {body}"
    );
    let _ = std::fs::remove_file(config);
    metrics.snapshot()
}

/// The row a `/v1/responses` turn leaves behind once the Anthropic upstream has
/// answered and the translation has been handed to the client: in `recent`, not
/// `active`, with the upstream's token counts (whole prompt, output, cached
/// part) and the selected model.
fn assert_row_closed(
    snapshot: &rayline_metrics::MetricsSnapshot,
    input_tokens: u64,
    output_tokens: u64,
    prompt_cache_tokens: Option<u64>,
) {
    assert!(
        snapshot.active.is_empty(),
        "row still active: {:?}",
        snapshot.active
    );
    assert_eq!(snapshot.totals.active_requests, 0);
    assert_eq!(snapshot.totals.completed_requests, 1);
    assert_eq!(snapshot.totals.errored_requests, 0);
    assert_eq!(snapshot.recent.len(), 1, "exactly one closed row expected");
    let row = &snapshot.recent[0];
    assert_eq!(row.state, "completed");
    assert_eq!(row.status_code, Some(200));
    assert_eq!(row.endpoint_id.as_deref(), Some("up"));
    assert_eq!(row.selected_model.as_deref(), Some("claude-mock"));
    assert_eq!(row.input_tokens, Some(input_tokens));
    assert_eq!(row.output_tokens, Some(output_tokens));
    assert_eq!(row.prompt_cache_tokens, prompt_cache_tokens);
    // The cache column is a rate, so the row needs the prompt total the cached
    // part is measured against, not just the cached count.
    match prompt_cache_tokens {
        Some(cached) => {
            assert_eq!(row.prompt_tokens, Some(input_tokens));
            assert_eq!(
                row.cache_hit_ratio,
                Some(cached as f64 / input_tokens as f64),
                "cache hit rate should be reported, not blank"
            );
        }
        None => assert_eq!(row.cache_hit_ratio, None),
    }
    assert_eq!(
        (snapshot.totals.input_tokens, snapshot.totals.output_tokens),
        (input_tokens, output_tokens)
    );
    assert!(
        row.first_token_at_unix_ms.is_some(),
        "FirstToken not recorded"
    );
}

/// Codex compaction and some tool turns send `stream: false`; an Anthropic
/// upstream answers with a JSON message. The translation is synchronous, so the
/// metrics row must be closed before the answer is returned, or `raydock top`
/// shows the turn as active forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_anthropic_json_answer_non_streaming_closes_metrics_row() {
    let snapshot = responses_turn_metrics(
        "responses-json-nonstream-metrics",
        http_json(&anthropic_json_message("done")),
        false,
    )
    .await;
    assert_row_closed(&snapshot, 47, 17, Some(5));
}

/// `stream: true` from the client with a JSON (non-streaming) upstream answer
/// is replayed as one synthetic SSE body; that row must close too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_anthropic_json_answer_streaming_client_closes_metrics_row() {
    let snapshot = responses_turn_metrics(
        "responses-json-stream-metrics",
        http_json(&anthropic_json_message("done")),
        true,
    )
    .await;
    assert_row_closed(&snapshot, 47, 17, Some(5));
}

/// `stream: false` from the client with an SSE upstream answer is buffered into
/// one JSON body; the counts come from `message_start` / `message_delta`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_anthropic_sse_answer_non_streaming_client_closes_metrics_row() {
    let snapshot = responses_turn_metrics(
        "responses-sse-nonstream-metrics",
        anthropic_sse("done"),
        false,
    )
    .await;
    assert_row_closed(&snapshot, 1, 1, None);
}

/// Streaming on both sides is the normal interactive Codex turn: the upstream
/// SSE is translated event by event and the stream task closes the row. That
/// row must carry the cached prompt count too, or `raydock top`'s cache column
/// reads as a miss for the shape most turns take. The upstream here sends the
/// Rayline router's placeholder `message_start` (zero input, `null` cache) with
/// the real counts in the final `message_delta`, as a translating gateway does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_anthropic_sse_answer_streaming_closes_metrics_row_with_cache_tokens() {
    let upstream = http_sse(concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"x\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":0,\"cache_read_input_tokens\":null,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":42,\"cache_read_input_tokens\":5,\"output_tokens\":17}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));
    let snapshot = responses_turn_metrics("responses-sse-stream-metrics", upstream, true).await;
    assert_row_closed(&snapshot, 47, 17, Some(5));
}

/// A streamed turn whose upstream never reports cache tokens leaves the cache
/// count unset rather than claiming a zero-token hit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_anthropic_sse_answer_streaming_without_cache_tokens_leaves_cache_unset() {
    let snapshot = responses_turn_metrics(
        "responses-sse-stream-nocache-metrics",
        anthropic_sse("done"),
        true,
    )
    .await;
    assert_row_closed(&snapshot, 1, 1, None);
}

/// A buffered SSE answer that reports an `error` event, or stops before
/// `message_stop`, is not a completion: the client still gets the text that
/// arrived (unchanged), but the row is closed as an error, so the completed
/// totals do not count a failed turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_anthropic_failed_sse_answer_non_streaming_client_closes_row_as_error() {
    let error_event = http_sse(concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
    ));
    let cut_off = http_sse(concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\n",
    ));
    for (name, upstream, error) in [
        ("responses-sse-error-metrics", error_event, "Overloaded"),
        (
            "responses-sse-cutoff-metrics",
            cut_off,
            "ended before the response completed",
        ),
    ] {
        let snapshot = responses_turn_metrics(name, upstream, false).await;
        assert!(snapshot.active.is_empty(), "{name}: {:?}", snapshot.active);
        assert_eq!(snapshot.totals.errored_requests, 1, "{name}");
        assert_eq!(snapshot.recent.len(), 1, "{name}");
        let row = &snapshot.recent[0];
        assert_eq!(row.state, "error", "{name}: {row:?}");
        assert_eq!(row.status_code, Some(502), "{name}");
        assert!(
            row.error.as_deref().is_some_and(|e| e.contains(error)),
            "{name}: {row:?}"
        );
    }
}

/// Codex declares `{type: "web_search", external_web_access: false}` on every
/// turn and sets it to true only for live search. The default must keep
/// working through a Responses → Anthropic route: the offline declaration is
/// omitted, never translated into a server tool the upstream would refuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn responses_codex_default_web_search_is_omitted_not_translated() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "20.50"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
        tx,
    ));
    let config = write_config(
        "codex-default-web-search",
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
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .json(&json!({"model": "m", "input": "total?", "stream": false,
        "tools": [
            {"type": "function", "name": "shell",
             "parameters": {"type": "object", "properties": {}}},
            {"type": "web_search", "external_web_access": false}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let forwarded = rx.await.expect("upstream saw the request");
    let body = forwarded.split("\r\n\r\n").nth(1).expect("request body");
    let sent: Value = serde_json::from_str(body).expect("json body");
    let tools = sent["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), 1, "{tools:?}");
    assert_eq!(tools[0]["name"], "shell");
    let _ = std::fs::remove_file(config);
}

/// `rayline claude-desktop`: with an inbound guard the router refuses every
/// request that lacks the per-instance token, names a non-loopback Host
/// (DNS rebinding) or Origin, or sends a non-JSON body — before anything is
/// forwarded — and forwards an authorized request without leaking the token
/// upstream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_guard_refuses_unauthorized_rebinding_and_non_json_requests() {
    const TOKEN: &str = "rlt-it-guard-token-0123456789abcdef";
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "OK"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
        tx,
    ));
    let config = write_config(
        "inbound-guard",
        &json!({
            "endpoints": [{"id": "up", "protocol": "anthropic_messages",
                           "base_url": format!("http://127.0.0.1:{upstream_port}"),
                           "models": ["m"]}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }),
    );
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        inbound_guard: rayline_local_router::InboundGuard::new(TOKEN),
        ..LocalRouterOptions::default()
    };
    tokio::spawn(async move {
        let _ = serve(opts).await;
    });
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let body = r#"{"model":"m","max_tokens":8,"messages":[{"role":"user","content":"hi"}]}"#;
    let post = |token: Option<&str>, content_type: &str| {
        let mut request = client
            .post(format!("{base}/v1/messages"))
            .header("content-type", content_type)
            .body(body);
        if let Some(token) = token {
            request = request.header("x-api-key", token);
        }
        request
    };
    assert_eq!(
        post(None, "application/json")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        post(Some("rayline-local-placeholder"), "application/json")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        post(Some(TOKEN), "text/plain")
            .send()
            .await
            .unwrap()
            .status(),
        415
    );
    assert_eq!(
        post(Some(TOKEN), "application/json")
            .header("host", format!("rebind.example:{port}"))
            .send()
            .await
            .unwrap()
            .status(),
        421
    );
    assert_eq!(
        post(Some(TOKEN), "application/json")
            .header("origin", "https://evil.example")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    let ok = post(Some(TOKEN), "application/json").send().await.unwrap();
    assert_eq!(ok.status(), 200);
    let forwarded = rx.await.expect("upstream saw the authorized request");
    assert!(
        !forwarded.contains(TOKEN),
        "the inbound token reached the upstream"
    );
    let _ = std::fs::remove_file(config);
}

/// Write `request` to the router over a raw TCP socket and return the response
/// status code, so a test controls header order and repetition byte-for-byte.
async fn raw_status(port: u16, request: &str) -> u16 {
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .unwrap();
    sock.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    // A request that slipped past the guard would hang on the silent upstream.
    let _ = tokio::time::timeout(Duration::from_secs(5), sock.read_to_end(&mut response)).await;
    let response = String::from_utf8_lossy(&response);
    response
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unexpected response: {response}"))
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

/// Same upstream shape as the Chat Completions case below, through
/// `/v1/responses`: `response.completed` must report the whole prompt and the
/// cache-read count under `input_tokens_details.cached_tokens`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_responses_stream_reports_prompt_and_cached_tokens() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":null}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"PONG\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":8686,\"output_tokens\":17,\"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":5508}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    tokio::spawn(serve_once(upstream, http_sse(sse)));
    let config = write_config(
        "responses-inbound-usage",
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
        .post(format!("http://127.0.0.1:{port}/v1/responses"))
        .json(&json!({"model": "m", "stream": true,
                      "input": [{"type": "message", "role": "user", "content": [
                          {"type": "input_text", "text": "ping"}]}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let events = collect_sse(response).await;
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "response.output_text.delta" && event["delta"] == "PONG"),
        "{events:?}"
    );
    let completed = events
        .iter()
        .find(|event| event["type"] == "response.completed")
        .expect("response.completed");
    assert_eq!(
        completed["response"]["usage"],
        json!({"input_tokens": 14194, "input_tokens_details": {"cached_tokens": 5508},
               "output_tokens": 17, "output_tokens_details": {"reasoning_tokens": 0},
               "total_tokens": 14211})
    );
    let _ = std::fs::remove_file(config);
}

/// The Rayline router streams a placeholder `message_start` (`input_tokens`
/// 0, cache fields `null`) and reports the real counts only in the final
/// `message_delta`. A Chat client reading the usage chunk must see the whole
/// prompt and the cache-read count, not zeros (rayline-ai/rayline#134).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_completions_stream_reports_prompt_and_cached_tokens() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let sse = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"m\",\"content\":[],\"stop_reason\":null,\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":null}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"PONG\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"input_tokens\":8686,\"output_tokens\":17,\"cache_creation_input_tokens\":null,\"cache_read_input_tokens\":5508}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    let (tx, _rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(upstream, http_sse(sse), tx));
    let config = write_config(
        "chat-inbound-usage",
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
        .json(&json!({"model": "m", "stream": true,
                      "stream_options": {"include_usage": true},
                      "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let chunks = collect_sse(response).await;
    assert!(
        chunks
            .iter()
            .any(|chunk| chunk["choices"][0]["delta"]["content"] == "PONG"),
        "{chunks:?}"
    );
    let usage = chunks
        .iter()
        .find_map(|chunk| chunk.get("usage"))
        .expect("usage chunk");
    assert_eq!(
        usage,
        &json!({"prompt_tokens": 14194, "completion_tokens": 17, "total_tokens": 14211,
                "prompt_tokens_details": {"cached_tokens": 5508}})
    );
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
    let body = json!({"model": "rayline-auto", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
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

    // Raw HTTP/1.1 requests that smuggle a second guarded header past a check
    // that reads only the first value, or an absolute-form target whose
    // authority disagrees with Host.
    let payload = body.to_string();
    let absolute = format!("http://127.0.0.1:{port}/v1/messages");
    let raw_cases = [
        (
            "/v1/messages",
            format!(
                "host: 127.0.0.1:{port}\r\nhost: evil.example:{port}\r\ncontent-type: application/json\r\n"
            ),
            421,
        ),
        (
            absolute.as_str(),
            format!("host: evil.example:{port}\r\ncontent-type: application/json\r\n"),
            421,
        ),
        (
            "/v1/messages",
            format!(
                "host: 127.0.0.1:{port}\r\norigin: http://127.0.0.1:{port}\r\norigin: https://evil.example\r\ncontent-type: application/json\r\n"
            ),
            403,
        ),
        (
            "/v1/messages",
            format!(
                "host: 127.0.0.1:{port}\r\norigin: http://127.0.0.1:{port}\r\norigin: null\r\ncontent-type: application/json\r\n"
            ),
            403,
        ),
        (
            "/v1/messages",
            format!(
                "host: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-type: text/plain\r\n"
            ),
            415,
        ),
    ];
    for (target, headers, expected) in raw_cases {
        let request = format!(
            "POST {target} HTTP/1.1\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{payload}",
            payload.len()
        );
        assert_eq!(
            raw_status(port, &request).await,
            expected,
            "{target} {headers:?}"
        );
    }

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

/// With an inbound guard, a caller that authenticates to the router with its
/// token as `Authorization: Bearer` does not have that token forwarded to a
/// `client_bearer` endpoint as its provider credential.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_client_bearer_never_forwards_the_inbound_token() {
    const TOKEN: &str = "rlt-it-chat-bearer-token-0123456789abcdef";
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
        "chat-client-bearer-guard",
        json!({"id": "up", "protocol": "openai_chat", "auth": "client_bearer",
               "base_url": format!("http://127.0.0.1:{upstream_port}/v1"), "models": ["m"]}),
    );
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        inbound_guard: rayline_local_router::InboundGuard::new(TOKEN),
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
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let captured = rx.await.expect("upstream saw the request");
    assert!(
        !captured.contains(TOKEN),
        "the inbound token reached the upstream: {captured}"
    );
    assert!(
        !captured.to_ascii_lowercase().contains("authorization:"),
        "{captured}"
    );
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

/// The Chat Completions listener sits behind the same inbound checks as every
/// other POST route: with an inbound guard, both its paths refuse a missing or
/// wrong token, a DNS-rebound Host, a cross-site Origin, a non-JSON body,
/// repeated Host/Origin/Content-Type headers and an absolute-form target whose
/// authority disagrees with Host, before anything is forwarded. Only the
/// authorized request reaches the provider, without the token.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_completions_go_through_the_inbound_guard() {
    const TOKEN: &str = "rlt-it-chat-guard-token-0123456789abcdef";
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_json(&json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
            "content": [{"type": "text", "text": "OK"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 1, "output_tokens": 1}
        })),
        tx,
    ));
    let config = chat_route_config("chat-inbound-guard", anthropic_endpoint(upstream_port));
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        inbound_guard: rayline_local_router::InboundGuard::new(TOKEN),
        ..LocalRouterOptions::default()
    };
    tokio::spawn(async move {
        let _ = serve(opts).await;
    });
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if client
            .get(format!("{base}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // The upstream captures only its first request, so a refused request that
    // reached it would be captured instead of the authorized one sent last.
    let refused =
        json!({"model": "m", "messages": [{"role": "user", "content": "refused"}]}).to_string();
    for path in ["/v1/chat/completions", "/chat/completions"] {
        let post = |token: Option<&str>, content_type: &str| {
            let mut request = client
                .post(format!("{base}{path}"))
                .header("content-type", content_type)
                .body(refused.clone());
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            request
        };
        let cases = [
            (post(None, "application/json"), 401),
            (post(Some("sk-wrong"), "application/json"), 401),
            (post(Some(TOKEN), "text/plain"), 415),
            (
                post(Some(TOKEN), "application/json")
                    .header("host", format!("rebind.example:{port}")),
                421,
            ),
            (
                post(Some(TOKEN), "application/json").header("origin", "https://evil.example"),
                403,
            ),
        ];
        for (request, expected) in cases {
            assert_eq!(request.send().await.unwrap().status(), expected, "{path}");
        }

        let absolute = format!("http://127.0.0.1:{port}{path}");
        let auth = format!("authorization: Bearer {TOKEN}\r\n");
        let raw_cases = [
            (
                path,
                format!(
                    "host: 127.0.0.1:{port}\r\nhost: evil.example:{port}\r\ncontent-type: application/json\r\n"
                ),
                421,
            ),
            (
                absolute.as_str(),
                format!("host: evil.example:{port}\r\ncontent-type: application/json\r\n"),
                421,
            ),
            (
                path,
                format!(
                    "host: 127.0.0.1:{port}\r\norigin: http://127.0.0.1:{port}\r\norigin: https://evil.example\r\ncontent-type: application/json\r\n"
                ),
                403,
            ),
            (
                path,
                format!(
                    "host: 127.0.0.1:{port}\r\ncontent-type: application/json\r\ncontent-type: text/plain\r\n"
                ),
                415,
            ),
        ];
        for (target, headers, expected) in raw_cases {
            let request = format!(
                "POST {target} HTTP/1.1\r\n{auth}{headers}content-length: {}\r\nconnection: close\r\n\r\n{refused}",
                refused.len()
            );
            assert_eq!(
                raw_status(port, &request).await,
                expected,
                "{target} {headers:?}"
            );
        }
    }

    let ok = client
        .post(format!("{base}/v1/chat/completions"))
        .header("authorization", format!("Bearer {TOKEN}"))
        .json(&json!({"model": "m", "messages": [{"role": "user", "content": "authorized"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 200);
    let completion: Value = ok.json().await.unwrap();
    assert_eq!(completion["choices"][0]["message"]["content"], "OK");
    let forwarded = rx.await.expect("upstream saw the authorized request");
    assert!(
        forwarded.contains("authorized") && !forwarded.contains("refused"),
        "a refused request reached the provider: {forwarded}"
    );
    assert!(
        !forwarded.contains(TOKEN),
        "the inbound token reached the upstream"
    );
    let _ = std::fs::remove_file(config);
}

// --- Chat Completions metrics lifecycle --------------------------------------

/// Route one Chat Completions `request` to the mock endpoint `endpoint(port)`
/// answering `upstream` (a raw HTTP response), drain the answer, and return
/// the request's settled metrics row with the router totals.
async fn chat_metrics_row(
    name: &str,
    endpoint: impl FnOnce(u16) -> Value,
    upstream: Option<String>,
    request: Value,
) -> (
    rayline_metrics::RequestSnapshot,
    rayline_metrics::RouterTotals,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = listener.local_addr().unwrap().port();
    if let Some(upstream) = upstream {
        tokio::spawn(serve_once(listener, upstream));
    }
    let config = chat_route_config(name, endpoint(upstream_port));
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        metrics: Some(metrics.clone()),
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
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .timeout(Duration::from_secs(5))
        .json(&request)
        .send()
        .await
        .unwrap();
    // A cut-off stream is a body error; the row is closed either way.
    let _ = response.bytes().await;
    let _ = std::fs::remove_file(config);
    // The forwarder closes the row after the last body byte went out.
    for _ in 0..100 {
        let snapshot = metrics.snapshot();
        if snapshot.totals.active_requests == 0 && snapshot.recent.len() == 1 {
            let mut recent = snapshot.recent;
            return (recent.remove(0), snapshot.totals);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!(
        "the request was not closed in metrics: {:?}",
        metrics.snapshot()
    );
}

fn openai_chat_endpoint(upstream_port: u16) -> Value {
    json!({"id": "up", "protocol": "openai_chat",
           "base_url": format!("http://127.0.0.1:{upstream_port}/v1"), "models": ["m"]})
}

fn chat_request(stream: bool) -> Value {
    json!({"model": "m", "stream": stream,
           "messages": [{"role": "user", "content": "ping"}]})
}

/// A remote Chat Completions request has the same row as a Messages request:
/// announced with the route, closed as completed with the Chat `usage`
/// counts (whole prompt, completion, cached), so `raydock top` lists it and
/// the totals count it. Both translated answer shapes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_translated_requests_complete_their_metrics_row() {
    let message = http_json(&json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "PONG"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 9, "output_tokens": 2, "cache_read_input_tokens": 5}
    }));
    let sse = http_sse(concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":0,\"cache_read_input_tokens\":null}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"PONG\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":8686,\"output_tokens\":17,\"cache_read_input_tokens\":5508}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    ));
    for (name, upstream, stream, expected) in [
        (
            "chat-metrics-json",
            message.clone(),
            false,
            (14, 2, Some(5)),
        ),
        ("chat-metrics-replay", message, true, (14, 2, Some(5))),
        ("chat-metrics-sse", sse, true, (14194, 17, Some(5508))),
    ] {
        let (row, totals) = chat_metrics_row(
            name,
            anthropic_endpoint,
            Some(upstream),
            chat_request(stream),
        )
        .await;
        assert_eq!(row.state, "completed", "{name}: {row:?}");
        assert_eq!(row.status_code, Some(200), "{name}");
        assert_eq!(row.target.as_deref(), Some("remote"), "{name}");
        assert_eq!(row.endpoint_id.as_deref(), Some("up"), "{name}");
        assert_eq!(row.requested_model.as_deref(), Some("m"), "{name}");
        assert_eq!(row.selected_model.as_deref(), Some("m"), "{name}");
        assert!(
            row.route_id.is_some() && row.policy.is_some(),
            "{name}: {row:?}"
        );
        assert!(row.first_token_at_unix_ms.is_some(), "{name}: {row:?}");
        assert_eq!(
            (row.input_tokens, row.output_tokens, row.prompt_cache_tokens),
            (Some(expected.0), Some(expected.1), expected.2),
            "{name}"
        );
        assert_eq!(totals.completed_requests, 1, "{name}");
        assert_eq!(totals.remote_requests, 1, "{name}");
        assert_eq!(totals.errored_requests, 0, "{name}");
        assert_eq!(
            (totals.input_tokens, totals.output_tokens),
            (expected.0, expected.1),
            "{name}"
        );
    }
}

/// A Chat Completions request passed through to an OpenAI-compatible
/// endpoint completes its row from the `usage` the upstream sends, in JSON
/// and in the stream's usage chunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_passthrough_requests_complete_their_metrics_row() {
    let completion = http_json(&json!({
        "id": "chatcmpl_1", "object": "chat.completion", "created": 1, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "PONG"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 5, "completion_tokens": 1, "total_tokens": 6}
    }));
    let chunks = http_sse(&format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {"role": "assistant", "content": "PONG"},
                            "finish_reason": null}]}),
        json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"id": "chatcmpl_1", "object": "chat.completion.chunk", "choices": [],
               "usage": {"prompt_tokens": 12, "completion_tokens": 3, "total_tokens": 15,
                         "prompt_tokens_details": {"cached_tokens": 4}}}),
    ));
    for (name, upstream, stream, expected) in [
        ("chat-metrics-pass-json", completion, false, (5, 1, None)),
        ("chat-metrics-pass-sse", chunks, true, (12, 3, Some(4))),
    ] {
        let (row, totals) = chat_metrics_row(
            name,
            openai_chat_endpoint,
            Some(upstream),
            chat_request(stream),
        )
        .await;
        assert_eq!(row.state, "completed", "{name}: {row:?}");
        assert_eq!(row.status_code, Some(200), "{name}");
        assert_eq!(row.target.as_deref(), Some("remote"), "{name}");
        assert_eq!(row.endpoint_id.as_deref(), Some("up"), "{name}");
        assert_eq!(row.selected_model.as_deref(), Some("m"), "{name}");
        assert!(row.first_token_at_unix_ms.is_some(), "{name}: {row:?}");
        assert_eq!(
            (row.input_tokens, row.output_tokens, row.prompt_cache_tokens),
            (Some(expected.0), Some(expected.1), expected.2),
            "{name}"
        );
        assert_eq!(totals.completed_requests, 1, "{name}");
        assert_eq!(totals.remote_requests, 1, "{name}");
    }
}

/// Every way a Chat Completions request can fail after it was announced
/// closes its row as an error, so nothing is left active: an upstream
/// non-2xx on both routes, a stream cut off mid-answer, a stream closed
/// cleanly before its last chunk, an SSE `error` event after a 200, a
/// request the translated route refuses, and a route to a Responses-only
/// endpoint.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_failures_close_their_metrics_row() {
    let too_many = |body: Value| {
        let body = body.to_string();
        format!(
            "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let anthropic_429 = too_many(json!({"type": "error",
        "error": {"type": "rate_limit_error", "message": "slow down"}}));
    let openai_429 =
        too_many(json!({"error": {"message": "slow down", "type": "rate_limit_error"}}));
    // A chunked body that ends before its terminating chunk.
    let cut_off = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n{:x}\r\n{SSE_START}\r\n",
        SSE_START.len()
    );
    let unsearchable = json!({"model": "m", "web_search_options": {},
                              "messages": [{"role": "user", "content": "ping"}]});
    let responses_endpoint = |upstream_port: u16| {
        json!({"id": "up", "protocol": "openai_responses",
               "base_url": format!("http://127.0.0.1:{upstream_port}/v1"), "models": ["m"]})
    };
    struct Failure {
        name: &'static str,
        endpoint: fn(u16) -> Value,
        upstream: Option<String>,
        request: Value,
        status: Option<u16>,
        error: &'static str,
    }
    let cases = [
        Failure {
            name: "chat-metrics-anthropic-429",
            endpoint: anthropic_endpoint,
            upstream: Some(anthropic_429),
            request: chat_request(false),
            status: Some(429),
            error: "upstream returned HTTP 429",
        },
        Failure {
            name: "chat-metrics-openai-429",
            endpoint: openai_chat_endpoint,
            upstream: Some(openai_429),
            request: chat_request(false),
            status: Some(429),
            error: "upstream returned HTTP 429",
        },
        Failure {
            name: "chat-metrics-cut-off",
            endpoint: anthropic_endpoint,
            upstream: Some(cut_off),
            request: chat_request(true),
            status: Some(502),
            error: "upstream stream",
        },
        Failure {
            name: "chat-metrics-clean-eof",
            endpoint: openai_chat_endpoint,
            // A close-delimited 200 that stops before its last chunk: a
            // normal EOF, not a transport error.
            upstream: Some(http_sse(&format!(
                "data: {}\n\n",
                json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                       "choices": [{"index": 0, "delta": {"content": "PO"},
                                    "finish_reason": null}]}),
            ))),
            request: chat_request(true),
            status: Some(200),
            error: "ended before the response completed",
        },
        Failure {
            name: "chat-metrics-sse-error",
            endpoint: openai_chat_endpoint,
            upstream: Some(http_sse(&format!(
                "data: {}\n\ndata: {}\n\n",
                json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                       "choices": [{"index": 0, "delta": {"content": "PO"}, "finish_reason": null}]}),
                json!({"error": {"message": "boom", "type": "server_error"}}),
            ))),
            request: chat_request(true),
            status: Some(200),
            error: "boom",
        },
        Failure {
            name: "chat-metrics-refused",
            endpoint: anthropic_endpoint,
            upstream: None,
            request: unsearchable,
            status: Some(400),
            error: "unsupported_parameter",
        },
        Failure {
            name: "chat-metrics-responses-route",
            endpoint: responses_endpoint,
            upstream: None,
            request: chat_request(false),
            status: Some(400),
            error: "unsupported_route",
        },
    ];
    for Failure {
        name,
        endpoint,
        upstream,
        request,
        status,
        error,
    } in cases
    {
        let (row, totals) = chat_metrics_row(name, endpoint, upstream, request).await;
        assert_eq!(row.state, "error", "{name}: {row:?}");
        assert_eq!(row.status_code, status, "{name}: {row:?}");
        assert!(
            row.error.as_deref().is_some_and(|e| e.contains(error)),
            "{name}: {row:?}"
        );
        assert_eq!(row.target.as_deref(), Some("remote"), "{name}");
        assert_eq!(totals.errored_requests, 1, "{name}");
        assert_eq!(totals.completed_requests, 1, "{name}");
    }
}

/// A Chat client that stops reading mid-stream cancels its generation on
/// both routes: the router stops reading the upstream (so the upstream's
/// next write fails) and closes the row as an error instead of draining the
/// rest of the answer and recording it as completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_client_disconnect_cancels_upstream_and_closes_row() {
    let anthropic_piece = sse_text_delta("more");
    let openai_piece = format!(
        "data: {}\n\n",
        json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {"content": "more"}, "finish_reason": null}]})
    );
    type Endpoint = fn(u16) -> Value;
    for (name, endpoint, piece) in [
        (
            "chat-cancel-anthropic",
            anthropic_endpoint as Endpoint,
            anthropic_piece,
        ),
        (
            "chat-cancel-openai",
            openai_chat_endpoint as Endpoint,
            openai_piece,
        ),
    ] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (cancelled_tx, cancelled_rx) = oneshot::channel::<bool>();
        // An answer that keeps streaming for seconds unless the reader goes
        // away; reports whether a write failed before it ran out of pieces.
        tokio::spawn(async move {
            let Ok((mut sock, _)) = upstream.accept().await else {
                return;
            };
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(SSE_HEAD.as_bytes()).await;
            let _ = sock.write_all(SSE_START.as_bytes()).await;
            let mut cancelled = false;
            for _ in 0..40 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if sock.write_all(piece.as_bytes()).await.is_err() || sock.flush().await.is_err() {
                    cancelled = true;
                    break;
                }
            }
            let _ = cancelled_tx.send(cancelled);
        });
        let config = chat_route_config(name, endpoint(upstream_port));
        let metrics = rayline_metrics::RouterMetrics::new("test");
        let port = free_port();
        let opts = LocalRouterOptions {
            port,
            config_path: Some(config.clone()),
            metrics: Some(metrics.clone()),
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
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&chat_request(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{name}");
        let mut stream = response.bytes_stream();
        let _ = stream.next().await.expect("first chunk");
        drop(stream);
        let _ = std::fs::remove_file(config);
        assert!(
            tokio::time::timeout(Duration::from_secs(3), cancelled_rx)
                .await
                .expect("the upstream kept streaming to a client that left")
                .unwrap_or(false),
            "{name}: the upstream never saw the cancellation"
        );
        let mut row = None;
        for _ in 0..100 {
            let snapshot = metrics.snapshot();
            if snapshot.totals.active_requests == 0 && !snapshot.recent.is_empty() {
                row = snapshot.recent.into_iter().next();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = row.expect("the row was closed");
        assert_eq!(row.state, "error", "{name}: {row:?}");
        assert!(
            row.error
                .as_deref()
                .is_some_and(|e| e.contains("client disconnected")),
            "{name}: {row:?}"
        );
    }
}

/// A streaming client that did not ask for usage still gets a metered row:
/// the router asks the upstream for the usage chunk and keeps it from the
/// client, whose stream is otherwise byte for byte what the upstream sent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_passthrough_requests_usage_the_client_did_not() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let content = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                         "choices": [{"index": 0, "delta": {"content": "PONG"},
                                      "finish_reason": null}]});
    let finish = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
    let usage = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk", "choices": [],
                       "usage": {"prompt_tokens": 31, "completion_tokens": 7, "total_tokens": 38,
                                 "prompt_tokens_details": {"cached_tokens": 11}}});
    let (tx, rx) = oneshot::channel();
    tokio::spawn(serve_once_capture(
        upstream,
        http_sse(&format!(
            "data: {content}\n\ndata: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
        )),
        tx,
    ));
    let config = chat_route_config("chat-usage-injected", openai_chat_endpoint(upstream_port));
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        metrics: Some(metrics.clone()),
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
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // No `stream_options`: this client never asked for usage.
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"model": "m", "stream": true,
                      "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = String::from_utf8(response.bytes().await.unwrap().to_vec()).expect("utf-8");
    let _ = std::fs::remove_file(config);

    // The router asked the upstream for the usage chunk.
    let forwarded = rx.await.expect("upstream saw the request");
    let sent: Value =
        serde_json::from_str(forwarded.split("\r\n\r\n").nth(1).expect("body")).expect("json");
    assert_eq!(
        sent["stream_options"]["include_usage"],
        json!(true),
        "{sent}"
    );

    // The client got the answer and the terminator, but not the usage chunk
    // it never asked for.
    assert!(body.contains("PONG") && body.contains("[DONE]"), "{body}");
    assert!(body.contains("\"finish_reason\":\"stop\""), "{body}");
    assert!(
        !body.contains("prompt_tokens"),
        "the client saw a usage chunk it did not request: {body}"
    );

    // And the row carries the upstream's real counts, not the estimate.
    let mut row = None;
    for _ in 0..100 {
        let snapshot = metrics.snapshot();
        if snapshot.totals.active_requests == 0 && !snapshot.recent.is_empty() {
            row = snapshot.recent.into_iter().next();
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let row = row.expect("the row was closed");
    assert_eq!(row.state, "completed", "{row:?}");
    assert_eq!(
        (row.input_tokens, row.output_tokens, row.prompt_cache_tokens),
        (Some(31), Some(7), Some(11)),
        "{row:?}"
    );
}

/// A client that asked for usage itself keeps seeing the usage chunk: the
/// router only withholds a chunk it requested on the client's behalf.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_passthrough_keeps_usage_the_client_asked_for() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let finish = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": "PONG"},
                                     "finish_reason": "stop"}]});
    let usage = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk", "choices": [],
                       "usage": {"prompt_tokens": 4, "completion_tokens": 2, "total_tokens": 6}});
    tokio::spawn(serve_once(
        upstream,
        http_sse(&format!(
            "data: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
        )),
    ));
    let config = chat_route_config("chat-usage-kept", openai_chat_endpoint(upstream_port));
    let port = free_port();
    start_router(port, config.clone()).await;
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"model": "m", "stream": true,
                      "stream_options": {"include_usage": true},
                      "messages": [{"role": "user", "content": "ping"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = String::from_utf8(response.bytes().await.unwrap().to_vec()).expect("utf-8");
    assert!(body.contains("\"prompt_tokens\":4"), "{body}");
    assert!(body.contains("[DONE]"), "{body}");
    let _ = std::fs::remove_file(config);
}

/// A multi-choice stream (`n > 1`) is only complete once every choice
/// finished: one choice's `finish_reason` must not make a clean EOF that
/// truncated its sibling look like a success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_multi_choice_truncation_is_not_recorded_as_completed() {
    let first_done = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                            "choices": [{"index": 0, "delta": {"content": "A"},
                                         "finish_reason": "stop"}]});
    let second_open = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                             "choices": [{"index": 1, "delta": {"content": "B"},
                                          "finish_reason": null}]});
    let second_done = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                             "choices": [{"index": 1, "delta": {}, "finish_reason": "stop"}]});
    // Choice 0 finished, choice 1 was cut off: a clean close, no [DONE].
    let truncated = http_sse(&format!("data: {first_done}\n\ndata: {second_open}\n\n"));
    // Both choices finished, still no [DONE]: a complete answer.
    let complete = http_sse(&format!(
        "data: {first_done}\n\ndata: {second_open}\n\ndata: {second_done}\n\n"
    ));
    for (name, upstream, expected_state) in [
        ("chat-multi-truncated", truncated, "error"),
        ("chat-multi-complete", complete, "completed"),
    ] {
        let (row, _totals) = chat_metrics_row(
            name,
            openai_chat_endpoint,
            Some(upstream),
            json!({"model": "m", "stream": true, "n": 2,
                   "messages": [{"role": "user", "content": "ping"}]}),
        )
        .await;
        assert_eq!(row.state, expected_state, "{name}: {row:?}");
        if expected_state == "error" {
            assert!(
                row.error
                    .as_deref()
                    .is_some_and(|e| e.contains("ended before the response completed")),
                "{name}: {row:?}"
            );
        }
    }
}

/// Withholding the injected usage chunk shortens the body, so a
/// `Content-Length` the upstream computed for the unfiltered body must not
/// be forwarded: the client would otherwise see fewer bytes than promised
/// and report a truncated response.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_filtered_stream_drops_a_stale_content_length() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_port = upstream.local_addr().unwrap().port();
    let finish = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": "PONG"},
                                     "finish_reason": "stop"}]});
    let usage = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk", "choices": [],
                       "usage": {"prompt_tokens": 9, "completion_tokens": 2, "total_tokens": 11}});
    let body = format!("data: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n");
    // An SSE body with a declared length, as a non-streaming upstream sends.
    tokio::spawn(serve_once(
        upstream,
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        ),
    ));
    let config = chat_route_config("chat-stale-length", openai_chat_endpoint(upstream_port));
    let metrics = rayline_metrics::RouterMetrics::new("test");
    let port = free_port();
    let opts = LocalRouterOptions {
        port,
        config_path: Some(config.clone()),
        metrics: Some(metrics.clone()),
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
            .is_ok_and(|r| r.status().is_success())
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let response = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&chat_request(true))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    // No length may promise the unfiltered body's size.
    let promised = header(&response, "content-length").map(ToOwned::to_owned);
    // The body must arrive whole, not cut off at a stale length.
    let received = response.bytes().await.expect("a complete body").len();
    let _ = std::fs::remove_file(config);
    assert!(received > 0 && received < body.len(), "{received}");
    if let Some(promised) = promised {
        assert_eq!(
            promised.parse::<usize>().ok(),
            Some(received),
            "a stale content-length was forwarded"
        );
    }
}

/// A metered response must not arrive compressed: metrics read the body as
/// it streams and reqwest is built without decompression, so compressed
/// bytes would yield no usage and no terminal event. This holds whether or
/// not the client asked for usage itself (only the first filters), and an
/// endpoint that configures `accept-encoding` must not win over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_metered_stream_asks_for_an_unencoded_body() {
    let finish = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": "PONG"},
                                     "finish_reason": "stop"}]});
    let usage = json!({"id": "chatcmpl_1", "object": "chat.completion.chunk", "choices": [],
                       "usage": {"prompt_tokens": 8, "completion_tokens": 2, "total_tokens": 10}});
    // Left: the client asked for usage (no filtering). Right: it did not.
    for (name, request) in [
        (
            "chat-identity-client-usage",
            json!({"model": "m", "stream": true,
                   "stream_options": {"include_usage": true},
                   "messages": [{"role": "user", "content": "ping"}]}),
        ),
        ("chat-identity-injected-usage", chat_request(true)),
    ] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        let (tx, rx) = oneshot::channel();
        tokio::spawn(serve_once_capture(
            upstream,
            http_sse(&format!(
                "data: {finish}\n\ndata: {usage}\n\ndata: [DONE]\n\n"
            )),
            tx,
        ));
        let config = chat_route_config(
            name,
            json!({"id": "up", "protocol": "openai_chat",
                   "base_url": format!("http://127.0.0.1:{upstream_port}/v1"),
                   "models": ["m"],
                   // A config that would otherwise hand metrics gzip bytes.
                   "headers": {"accept-encoding": "gzip"}}),
        );
        let metrics = rayline_metrics::RouterMetrics::new("test");
        let port = free_port();
        let opts = LocalRouterOptions {
            port,
            config_path: Some(config.clone()),
            metrics: Some(metrics.clone()),
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
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{name}");
        let _ = response.bytes().await;
        let _ = std::fs::remove_file(config);
        let forwarded = rx.await.expect("upstream saw the request");
        let encodings: Vec<&str> = forwarded
            .lines()
            .filter_map(|line| {
                let (header, value) = line.split_once(':')?;
                header
                    .trim()
                    .eq_ignore_ascii_case("accept-encoding")
                    .then(|| value.trim())
            })
            .collect();
        assert_eq!(encodings, vec!["identity"], "{name}: {forwarded}");

        // And the row carries the upstream's counts, not just the estimate.
        let mut row = None;
        for _ in 0..100 {
            let snapshot = metrics.snapshot();
            if snapshot.totals.active_requests == 0 && !snapshot.recent.is_empty() {
                row = snapshot.recent.into_iter().next();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = row.expect("the row was closed");
        assert_eq!(row.state, "completed", "{name}: {row:?}");
        assert_eq!(
            (row.input_tokens, row.output_tokens),
            (Some(8), Some(2)),
            "{name}: {row:?}"
        );
    }
}

/// A non-streaming passthrough answer is relayed without being scanned for
/// SSE events: a compact JSON body has no delimiter, so draining it would
/// retain the whole response and rescan it per chunk. Its usage still lands
/// on the row, read from the body once the stream ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_passthrough_json_is_not_scanned_as_sse() {
    let body = json!({
        "id": "chatcmpl_1", "object": "chat.completion", "created": 1, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "PONG"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 21, "completion_tokens": 6, "total_tokens": 27}
    });
    let (row, totals) = chat_metrics_row(
        "chat-json-not-sse",
        openai_chat_endpoint,
        Some(http_json(&body)),
        chat_request(false),
    )
    .await;
    // A JSON answer has no terminal SSE event, and must not be judged by one.
    assert_eq!(row.state, "completed", "{row:?}");
    assert_eq!(
        (row.input_tokens, row.output_tokens),
        (Some(21), Some(6)),
        "{row:?}"
    );
    assert_eq!(totals.completed_requests, 1);
    assert_eq!(totals.errored_requests, 0);
}

/// A client that leaves while the upstream is stalled is noticed then, on
/// both the passthrough and the translated route: the row is closed as an
/// error rather than staying active while the upstream holds the connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_stalled_upstream_notices_a_departed_client() {
    type Endpoint = fn(u16) -> Value;
    for (name, endpoint) in [
        ("chat-stall-passthrough", openai_chat_endpoint as Endpoint),
        ("chat-stall-translated", anthropic_endpoint as Endpoint),
    ] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        // Send a first event, then stall: never another byte, never a close.
        tokio::spawn(async move {
            let Ok((mut sock, _)) = upstream.accept().await else {
                return;
            };
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(SSE_HEAD.as_bytes()).await;
            let _ = sock.write_all(SSE_START.as_bytes()).await;
            let _ = sock.write_all(sse_text_delta("hi").as_bytes()).await;
            let _ = sock.flush().await;
            // Hold the connection open well past the assertions below.
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let config = chat_route_config(name, endpoint(upstream_port));
        let metrics = rayline_metrics::RouterMetrics::new("test");
        let port = free_port();
        let opts = LocalRouterOptions {
            port,
            config_path: Some(config.clone()),
            metrics: Some(metrics.clone()),
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
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&chat_request(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{name}");
        // Read the first chunk, then drop the body while the upstream stalls.
        let mut stream = response.bytes_stream();
        let _ = stream.next().await.expect("first chunk");
        drop(stream);
        let _ = std::fs::remove_file(config);
        // The row must close without waiting for the upstream to send again.
        let mut row = None;
        for _ in 0..150 {
            let snapshot = metrics.snapshot();
            if snapshot.totals.active_requests == 0 && !snapshot.recent.is_empty() {
                row = snapshot.recent.into_iter().next();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = row
            .unwrap_or_else(|| panic!("{name}: the row stayed active while the upstream stalled"));
        assert_eq!(row.state, "error", "{name}: {row:?}");
        assert!(
            row.error
                .as_deref()
                .is_some_and(|e| e.contains("client disconnected")),
            "{name}: {row:?}"
        );
    }
}

/// A non-streaming Chat request whose Anthropic upstream answers with a
/// buffered SSE body that failed — an `error` event, or a stream that
/// stopped before `message_stop` — is recorded as an error, not a
/// completion, matching the streaming path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_buffered_sse_failure_is_not_recorded_as_completed() {
    let error_event = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"overloaded\"}}\n\n",
    );
    let truncated = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"par\"}}\n\n",
    );
    let complete = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":5}}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    );
    for (name, sse, expected_state, expected_error) in [
        (
            "chat-buffered-error",
            error_event,
            "error",
            Some("overloaded"),
        ),
        (
            "chat-buffered-truncated",
            truncated,
            "error",
            Some("ended before the response completed"),
        ),
        ("chat-buffered-complete", complete, "completed", None),
    ] {
        // stream: false, so the SSE body is read in one piece.
        let (row, _totals) = chat_metrics_row(
            name,
            anthropic_endpoint,
            Some(http_sse(sse)),
            chat_request(false),
        )
        .await;
        assert_eq!(row.state, expected_state, "{name}: {row:?}");
        if let Some(expected_error) = expected_error {
            assert!(
                row.error
                    .as_deref()
                    .is_some_and(|e| e.contains(expected_error)),
                "{name}: {row:?}"
            );
        }
    }
}

/// A client that stops reading after the stream's terminal event, without
/// waiting for the body's EOF, already has its whole answer: the row is a
/// completion, not a cancellation. Both routes, against an upstream that
/// finishes the protocol and then holds the connection open.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_client_leaving_after_the_terminal_event_still_completes() {
    let chat_done = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {"content": "PONG"}, "finish_reason": null}]}),
        json!({"id": "chatcmpl_1", "object": "chat.completion.chunk",
               "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
    );
    let anthropic_done = format!("{SSE_START}{}{SSE_STOP}", sse_text_delta("PONG"));
    type Endpoint = fn(u16) -> Value;
    for (name, endpoint, body) in [
        (
            "chat-done-then-idle-passthrough",
            openai_chat_endpoint as Endpoint,
            chat_done,
        ),
        (
            "chat-done-then-idle-translated",
            anthropic_endpoint as Endpoint,
            anthropic_done,
        ),
    ] {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_port = upstream.local_addr().unwrap().port();
        // Send the complete answer, then hold the socket open: no EOF.
        tokio::spawn(async move {
            let Ok((mut sock, _)) = upstream.accept().await else {
                return;
            };
            let mut buf = [0u8; 16384];
            let _ = sock.read(&mut buf).await;
            let _ = sock.write_all(SSE_HEAD.as_bytes()).await;
            let _ = sock.write_all(body.as_bytes()).await;
            let _ = sock.flush().await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        });
        let config = chat_route_config(name, endpoint(upstream_port));
        let metrics = rayline_metrics::RouterMetrics::new("test");
        let port = free_port();
        let opts = LocalRouterOptions {
            port,
            config_path: Some(config.clone()),
            metrics: Some(metrics.clone()),
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
                .is_ok_and(|r| r.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let response = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&chat_request(true))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{name}");
        // Read until the terminator, then hang up without awaiting EOF.
        let mut stream = response.bytes_stream();
        let mut seen = String::new();
        while let Some(chunk) = stream.next().await {
            seen.push_str(&String::from_utf8_lossy(&chunk.expect("chunk")));
            if seen.contains("[DONE]") {
                break;
            }
        }
        assert!(seen.contains("[DONE]"), "{name}: {seen}");
        drop(stream);
        let _ = std::fs::remove_file(config);
        let mut row = None;
        for _ in 0..150 {
            let snapshot = metrics.snapshot();
            if snapshot.totals.active_requests == 0 && !snapshot.recent.is_empty() {
                row = snapshot.recent.into_iter().next();
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = row.unwrap_or_else(|| panic!("{name}: the row was never closed"));
        assert_eq!(row.state, "completed", "{name}: {row:?}");
        assert!(row.error.is_none(), "{name}: {row:?}");
    }
}

/// A translated Chat request is attributed to the model the upstream
/// actually used when it reports one, not to the route's own value, which
/// may be the routing sentinel. The passthrough path already did this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_translated_row_reports_the_upstream_model() {
    let message = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "PONG"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 4, "output_tokens": 1}
    })
    .to_string();
    let upstream_response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-rayline-selected-model: real-model-1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{message}",
        message.len()
    );
    let (row, _totals) = chat_metrics_row(
        "chat-translated-model",
        anthropic_endpoint,
        Some(upstream_response),
        chat_request(false),
    )
    .await;
    assert_eq!(row.state, "completed", "{row:?}");
    assert_eq!(
        row.selected_model.as_deref(),
        Some("real-model-1"),
        "{row:?}"
    );
}

/// The cloud router's id for a forwarded Chat request lands on its metrics
/// row, on both the passthrough and the translated route, and stays on the
/// row when the upstream answered with an error: the id is recorded as soon
/// as the headers are in, before the body can fail. Before the Chat listener
/// had a lifecycle the id could only be stamped on the response header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inbound_chat_row_carries_the_upstream_request_id() {
    let with_route_id = |status: &str, content_type: &str, route_id: &str, body: String| {
        format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\nx-rayline-route-id: {route_id}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let chat_json = json!({
        "id": "chatcmpl_1", "object": "chat.completion", "created": 1, "model": "m",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "PONG"},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
    })
    .to_string();
    let anthropic_json = json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": "m",
        "content": [{"type": "text", "text": "PONG"}], "stop_reason": "end_turn",
        "usage": {"input_tokens": 3, "output_tokens": 1}
    })
    .to_string();
    let anthropic_429 = json!({"type": "error",
        "error": {"type": "rate_limit_error", "message": "slow down"}})
    .to_string();
    type Endpoint = fn(u16) -> Value;
    for (name, endpoint, upstream, expected_state, route_id) in [
        (
            "chat-upstream-id-passthrough",
            openai_chat_endpoint as Endpoint,
            with_route_id("200 OK", "application/json", "rt_pass_1", chat_json),
            "completed",
            "rt_pass_1",
        ),
        (
            "chat-upstream-id-translated",
            anthropic_endpoint as Endpoint,
            with_route_id("200 OK", "application/json", "rt_tr_1", anthropic_json),
            "completed",
            "rt_tr_1",
        ),
        (
            "chat-upstream-id-on-error",
            anthropic_endpoint as Endpoint,
            with_route_id(
                "429 Too Many Requests",
                "application/json",
                "rt_err_1",
                anthropic_429,
            ),
            "error",
            "rt_err_1",
        ),
    ] {
        let (row, _totals) =
            chat_metrics_row(name, endpoint, Some(upstream), chat_request(false)).await;
        assert_eq!(row.state, expected_state, "{name}: {row:?}");
        assert_eq!(
            row.upstream_request_id.as_deref(),
            Some(route_id),
            "{name}: {row:?}"
        );
    }
}

/// A loopback upstream that answers by request line (`"GET /v1/models"`,
/// matched as a prefix) until the test ends, answering 404 for anything else.
/// Every request line it sees is recorded, so a test can assert what the router
/// asked for (and how often).
async fn serve_paths(
    listener: TcpListener,
    routes: Vec<(&'static str, Value)>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    while let Ok((mut sock, _)) = listener.accept().await {
        let mut buf = [0u8; 16384];
        let n = sock.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let line = request.lines().next().unwrap_or_default().to_owned();
        seen.lock().unwrap().push(line.clone());
        let response = routes
            .iter()
            .find(|(prefix, _)| {
                line.strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with([' ', '?']))
            })
            .map(|(_, body)| http_json(body))
            .unwrap_or_else(|| {
                "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    .to_owned()
            });
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
        let _ = sock.shutdown().await;
    }
}

async fn codex_catalog(port: u16) -> Vec<Value> {
    let resp = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/v1/models"))
        .send()
        .await
        .expect("router request");
    assert_eq!(resp.status(), 200);
    resp.json::<Value>().await.unwrap()["models"]
        .as_array()
        .unwrap()
        .clone()
}

/// `(context_window, max_context_window)` of a Codex catalog entry.
fn catalog_window(models: &[Value], slug: &str) -> (Value, Value) {
    let entry = models
        .iter()
        .find(|model| model["slug"] == slug)
        .unwrap_or_else(|| panic!("{slug} missing from catalog"));
    (
        entry["context_window"].clone(),
        entry["max_context_window"].clone(),
    )
}

/// The Codex catalog advertises the windows a hosted-style endpoint lists
/// (`context_length`, matched by `product` too), the sentinel takes the window
/// of the model it routes to, and the endpoint is asked only once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_endpoint_advertises_listed_context_windows() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(serve_paths(
        upstream,
        vec![(
            "GET /v1/models",
            json!({"object": "list", "data": [
                {"id": "sage-contributor", "product": "sage", "context_length": 1000000,
                 "max_output_tokens": 128000},
                {"id": "swift", "product": "swift", "context_length": 400000}
            ]}),
        )],
        seen.clone(),
    ));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "cloud",
            "protocol": "anthropic_messages",
            "base_url": format!("http://127.0.0.1:{up_port}"),
            "models": ["sage", "swift", "unlisted"]
        }],
        "routes": {
            "main": {"endpoint": "cloud", "model": "sage"},
            "subagent": {"endpoint": "cloud", "model": "swift"},
            "model_routes": {"rayline-local": {"endpoint": "cloud", "model": "sage"}}
        }
    });
    let path = write_config("models-windows-listed", &config);
    start_router(port, path.clone()).await;

    for _ in 0..2 {
        let models = codex_catalog(port).await;
        assert_eq!(
            catalog_window(&models, "rayline-local"),
            (json!(1000000), json!(1000000))
        );
        assert_eq!(
            catalog_window(&models, "sage"),
            (json!(1000000), json!(1000000))
        );
        assert_eq!(
            catalog_window(&models, "swift"),
            (json!(400000), json!(400000))
        );
        // Not listed: the default entry.
        assert_eq!(
            catalog_window(&models, "unlisted"),
            (json!(128000), Value::Null)
        );
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(
        seen.iter()
            .filter(|line| line.starts_with("GET /v1/models "))
            .count(),
        1,
        "the listing is cached: {seen:?}"
    );

    let _ = std::fs::remove_file(path);
}

/// Ollama: a loaded model's `/api/ps` context, else the tag's `num_ctx` from
/// `/api/show`. The router never asks Ollama to load or run a model.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_endpoint_sizes_ollama_models_without_loading_them() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(serve_paths(
        upstream,
        vec![
            (
                "GET /v1/models",
                json!({"object": "list", "data": [{"id": "loaded:9b"}, {"id": "tagged:7b-32k"}]}),
            ),
            (
                "GET /api/ps",
                json!({"models": [{"name": "loaded:9b", "model": "loaded:9b", "context_length": 98304}]}),
            ),
            (
                "POST /api/show",
                json!({"parameters": "stop \"<|im_end|>\"\nnum_ctx                        32768"}),
            ),
        ],
        seen.clone(),
    ));

    let port = free_port();
    let config = json!({
        "endpoints": [{
            "id": "ollama",
            "protocol": "openai_chat",
            "base_url": format!("http://127.0.0.1:{up_port}/v1"),
            "models": ["loaded:9b", "tagged:7b-32k"]
        }],
        "routes": {
            "main": {"endpoint": "ollama", "model": "tagged:7b-32k"},
            "model_routes": {"rayline-local": {"endpoint": "ollama", "model": "tagged:7b-32k"}}
        }
    });
    let path = write_config("models-windows-ollama", &config);
    start_router(port, path.clone()).await;

    let models = codex_catalog(port).await;
    assert_eq!(
        catalog_window(&models, "rayline-local"),
        (json!(32768), json!(32768))
    );
    assert_eq!(
        catalog_window(&models, "tagged:7b-32k"),
        (json!(32768), json!(32768))
    );
    assert_eq!(
        catalog_window(&models, "loaded:9b"),
        (json!(98304), json!(98304))
    );

    let mut seen = seen.lock().unwrap().clone();
    seen.sort();
    assert_eq!(
        seen,
        vec![
            "GET /api/ps HTTP/1.1",
            "GET /v1/models HTTP/1.1",
            "POST /api/show HTTP/1.1",
        ],
        "only metadata calls, and /api/show only for the model that is not loaded"
    );

    let _ = std::fs::remove_file(path);
}

/// llama.cpp: `/props` (asked per model, for its multi-model router mode)
/// reports the slot's `n_ctx`. An endpoint that does not answer keeps the
/// default entry.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn models_endpoint_sizes_llama_cpp_models_from_props() {
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let up_port = upstream.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    tokio::spawn(serve_paths(
        upstream,
        vec![
            (
                "GET /v1/models",
                json!({"object": "list", "data": [{"id": "gguf"}]}),
            ),
            (
                "GET /props",
                json!({"default_generation_settings": {"n_ctx": 16384}, "total_slots": 1}),
            ),
        ],
        seen.clone(),
    ));
    let down_port = free_port();

    let port = free_port();
    let config = json!({
        "endpoints": [
            {
                "id": "llama",
                "protocol": "openai_chat",
                "base_url": format!("http://127.0.0.1:{up_port}/v1"),
                "models": ["gguf"]
            },
            {
                "id": "down",
                "protocol": "openai_chat",
                "base_url": format!("http://127.0.0.1:{down_port}/v1"),
                "models": ["gone"]
            }
        ],
        "routes": {
            "main": {"endpoint": "llama", "model": "gguf"},
            "model_routes": {"rayline-local": {"endpoint": "llama", "model": "gguf"}}
        }
    });
    let path = write_config("models-windows-llama", &config);
    start_router(port, path.clone()).await;

    let models = codex_catalog(port).await;
    assert_eq!(
        catalog_window(&models, "rayline-local"),
        (json!(16384), json!(16384))
    );
    assert_eq!(
        catalog_window(&models, "gguf"),
        (json!(16384), json!(16384))
    );
    assert_eq!(
        catalog_window(&models, "gone"),
        (json!(128000), Value::Null)
    );
    assert!(
        seen.lock()
            .unwrap()
            .iter()
            .any(|line| line.starts_with("GET /props?model=gguf ")),
        "props asked per model"
    );

    let _ = std::fs::remove_file(path);
}
