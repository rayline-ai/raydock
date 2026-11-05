//! Ignored live check: run real Claude Code through rayline-proxy local mode.
//!
//! This is intentionally not part of default CI. It requires a local Claude Code
//! binary, but does not require claude.ai OAuth because it runs `claude --bare`
//! with a dummy Anthropic key. The test still exercises the real Claude Code
//! HTTP client, the proxy TLS CA path, proxy-internal local 307 handling, the
//! real `rayline-adapter`, and the shared usage auth cache.

use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::process::Command;

static TRACING: Once = Once::new();

fn init_tracing() {
    TRACING.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_env_filter("info")
            .with_test_writer()
            .try_init();
    });
}

fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    path_and_query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl CapturedRequest {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    }

    fn summary(&self) -> String {
        let mut selected_headers: Vec<String> = self
            .headers
            .iter()
            .filter(|(name, _)| {
                matches!(
                    name.to_ascii_lowercase().as_str(),
                    "authorization"
                        | "x-api-key"
                        | "x-rayline-local-available"
                        | "x-rayline-local-model-id"
                        | "content-type"
                        | "location"
                )
            })
            .map(|(name, value)| format!("{name}: {value}"))
            .collect();
        selected_headers.sort();
        format!(
            "{} body={}B [{}]",
            self.path_and_query,
            self.body.len(),
            selected_headers.join(", ")
        )
    }
}

type CapturedRequests = Arc<Mutex<Vec<CapturedRequest>>>;

async fn capture_request(req: Request<Incoming>) -> CapturedRequest {
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = req.into_body().collect().await.unwrap().to_bytes().to_vec();
    CapturedRequest {
        path_and_query,
        headers,
        body,
    }
}

struct FakeHttpServer {
    port: u16,
    captured: CapturedRequests,
}

async fn spawn_fake_local_model() -> FakeHttpServer {
    let port = free_port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_for_task = captured.clone();
    tokio::spawn(async move {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let listener = TcpListener::bind(addr).await.unwrap();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let captured = captured_for_task.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let captured = captured.clone();
                    async move {
                        let request = capture_request(req).await;
                        let wants_stream = serde_json::from_slice::<Value>(&request.body)
                            .ok()
                            .and_then(|v| v.get("stream").and_then(Value::as_bool))
                            .unwrap_or(false);
                        captured.lock().unwrap().push(request);
                        if wants_stream {
                            let body = concat!(
                                "event: message_start\n",
                                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_live\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"local-qwen\",\"content\":[],\"stop_reason\":null,\"stop_sequence\":null,\"usage\":{\"input_tokens\":11,\"output_tokens\":0}}}\n\n",
                                "event: content_block_start\n",
                                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                                "event: content_block_delta\n",
                                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"local-proxy-live-ok\"}}\n\n",
                                "event: content_block_stop\n",
                                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                                "event: message_delta\n",
                                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":7}}\n\n",
                                "event: message_stop\n",
                                "data: {\"type\":\"message_stop\"}\n\n",
                            );
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "text/event-stream")
                                    .body(Full::new(Bytes::from_static(body.as_bytes())))
                                    .unwrap(),
                            )
                        } else {
                            let body = json!({
                                "id": "msg_live",
                                "type": "message",
                                "role": "assistant",
                                "model": "local-qwen",
                                "content": [{"type": "text", "text": "local-proxy-live-ok"}],
                                "usage": {"input_tokens": 11, "output_tokens": 7}
                            });
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from(
                                        serde_json::to_vec(&body).unwrap(),
                                    )))
                                    .unwrap(),
                            )
                        }
                    }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    FakeHttpServer { port, captured }
}

async fn spawn_fake_router(default_redirect_port: u16) -> FakeHttpServer {
    let port = free_port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_for_task = captured.clone();
    tokio::spawn(async move {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let listener = TcpListener::bind(addr).await.unwrap();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let io = TokioIo::new(stream);
            let captured = captured_for_task.clone();
            tokio::spawn(async move {
                let svc = service_fn(move |req: Request<Incoming>| {
                    let captured = captured.clone();
                    async move {
                        let request = capture_request(req).await;
                        let path = request.path_and_query.clone();
                        captured.lock().unwrap().push(request);
                        if path.starts_with("/v1/messages") {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::TEMPORARY_REDIRECT)
                                    .header(
                                        "location",
                                        format!(
                                            "http://127.0.0.1:{default_redirect_port}/api/v1/messages?usage_doc_id=doc-live"
                                        ),
                                    )
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            )
                        } else if path == "/v1/usage/update" {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(b"{}")))
                                    .unwrap(),
                            )
                        } else if path.starts_with("/v1/models/rayline-auto") {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"id":"rayline-auto","type":"model","display_name":"Rayline Router","created_at":"2026-05-27T00:00:00Z"}"#,
                                    )))
                                    .unwrap(),
                            )
                        } else if path.starts_with("/v1/models") {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"data":[{"id":"rayline-auto","type":"model","display_name":"Rayline Router","created_at":"2026-05-27T00:00:00Z"}],"has_more":false,"first_id":"rayline-auto","last_id":"rayline-auto"}"#,
                                    )))
                                    .unwrap(),
                            )
                        } else {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Full::new(Bytes::from_static(b"not found")))
                                    .unwrap(),
                            )
                        }
                    }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });
    FakeHttpServer { port, captured }
}

async fn spawn_proxy(opts: rayline_proxy::ProxyOptions) {
    let port = opts.port;
    tokio::spawn(rayline_proxy::serve(opts));
    let client = reqwest::Client::new();
    for _ in 0..50 {
        if client
            .get(format!("http://127.0.0.1:{port}/healthz"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("proxy did not become healthy on port {port}");
}

async fn wait_for_usage_update(captured: &CapturedRequests) -> CapturedRequest {
    for _ in 0..100 {
        if let Some(update) = captured
            .lock()
            .unwrap()
            .iter()
            .find(|req| req.path_and_query == "/v1/usage/update")
            .cloned()
        {
            return update;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router did not receive /v1/usage/update");
}

#[tokio::test]
#[ignore = "requires local Claude Code binary; run with CLAUDE_BIN=/path/to/claude cargo test -p rayline-proxy --test it_claude_live -- --ignored --nocapture"]
async fn real_claude_bare_follows_local_redirect_and_records_usage() {
    init_tracing();
    let local_model = spawn_fake_local_model().await;
    let adapter_port = free_port();
    let router = spawn_fake_router(rayline_adapter::DEFAULT_PORT).await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let auth_cache = rayline_proxy::new_auth_cache();

    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{}", local_model.port),
        upstream_model: "local-qwen".to_string(),
        router_url: format!("http://127.0.0.1:{}", router.port),
        auth_cache: Some(auth_cache.clone()),
        metrics: None,
        collect_llama_progress: false,
    }));

    let mut opts = rayline_proxy::ProxyOptions::with_ca_paths(
        "rsk-rayline-live-test",
        ca_dir.path().join("proxy-ca.pem"),
        ca_dir.path().join("proxy-ca-key.pem"),
    );
    opts.port = proxy_port;
    opts.router_url = format!("http://127.0.0.1:{}", router.port);
    opts.local_available = true;
    opts.local_model_id = Some("local-qwen".to_string());
    opts.local_adapter_port = Some(adapter_port);
    opts.auth_cache = Some(auth_cache);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let claude_bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
    let claude_config = tempfile::tempdir().unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(90),
        Command::new(claude_bin)
            .arg("--bare")
            .arg("-p")
            .arg("Reply exactly: local-proxy-live-ok")
            .env("CLAUDE_CONFIG_DIR", claude_config.path())
            .env("ANTHROPIC_API_KEY", "rayline-dummy-anthropic-key")
            .env("ANTHROPIC_MODEL", "rayline-auto")
            .env("HTTPS_PROXY", format!("http://127.0.0.1:{proxy_port}"))
            .env("NODE_EXTRA_CA_CERTS", &ca_cert_path)
            .env("CLAUDE_CODE_DISABLE_AGENT_VIEW", "1")
            .output(),
    )
    .await
    .expect("claude timed out")
    .expect("spawn claude");

    let router_seen = router.captured.lock().unwrap().clone();
    let local_seen = local_model.captured.lock().unwrap().clone();
    assert!(
        output.status.success(),
        "claude failed\nstdout:\n{}\nstderr:\n{}\nrouter captured:\n{}\nlocal captured:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        router_seen
            .iter()
            .map(CapturedRequest::summary)
            .collect::<Vec<_>>()
            .join("\n"),
        local_seen
            .iter()
            .map(CapturedRequest::summary)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("local-proxy-live-ok"), "stdout: {stdout}");

    let update = wait_for_usage_update(&router.captured).await;
    assert_eq!(
        update.header("x-api-key"),
        Some("rsk-rayline-live-test".to_string())
    );
    assert_eq!(update.header("authorization"), None);
    let usage_body: Value = serde_json::from_slice(&update.body).unwrap();
    assert_eq!(usage_body["routeId"], "doc-live");
    assert_eq!(usage_body["inputTokens"], 11);
    assert_eq!(usage_body["outputTokens"], 7);

    assert!(
        local_seen
            .iter()
            .any(|req| req.path_and_query == "/v1/messages"),
        "local model did not receive /v1/messages"
    );
}
