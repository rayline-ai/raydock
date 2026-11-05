//! Integration test: boot rayline-adapter against a fake Anthropic-shaped
//! upstream (mimics llama-server's `/v1/messages`) and verify the body is
//! forwarded unchanged.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_metrics::{RouterMetrics, SharedMetricsSink};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

type CapturedBody = Arc<Mutex<Option<Vec<u8>>>>;

async fn fake_anthropic_upstream(port: u16, captured: CapturedBody) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let captured = captured.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let captured = captured.clone();
                async move {
                    let bytes = req.into_body().collect().await.unwrap().to_bytes();
                    *captured.lock().unwrap() = Some(bytes.to_vec());
                    let body = json!({
                        "id": "msg_fake",
                        "type": "message",
                        "role": "assistant",
                        "model": "qwen3.6-35b-a3b",
                        "content": [
                            {"type": "text", "text": "let me check"},
                            {
                                "type": "tool_use",
                                "id": "toolu_01",
                                "name": "Bash",
                                "input": {"command": "ls"}
                            }
                        ],
                        "stop_reason": "tool_use",
                        "stop_sequence": null,
                        "usage": {"input_tokens": 42, "output_tokens": 17}
                    });
                    let resp_bytes = serde_json::to_vec(&body).unwrap();
                    let resp: Response<Full<Bytes>> = Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from(resp_bytes)))
                        .unwrap();
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

#[tokio::test]
async fn adapter_passes_through_anthropic_messages_with_tool_use() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(upstream_port, captured.clone()));
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let req_body = json!({
        "model": "claude-test",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "list files"}],
        "tools": [{
            "name": "Bash",
            "description": "run a shell command",
            "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}}
        }]
    });
    let resp = client
        .post(format!("http://127.0.0.1:{adapter_port}/api/v1/messages"))
        .json(&req_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();

    // Response is byte-for-byte what the upstream produced — including the
    // tool_use block that would have been clobbered by an OpenAI round-trip.
    assert_eq!(body["type"], "message");
    assert_eq!(body["content"][1]["type"], "tool_use");
    assert_eq!(body["content"][1]["name"], "Bash");
    assert_eq!(body["content"][1]["input"]["command"], "ls");
    assert_eq!(body["stop_reason"], "tool_use");
    assert_eq!(body["usage"]["input_tokens"], 42);
    assert_eq!(body["usage"]["output_tokens"], 17);

    // Upstream got the exact Anthropic-shaped body, including the tools array.
    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream never received");
    let sent_json: Value = serde_json::from_slice(&sent).unwrap();
    assert_eq!(sent_json["tools"][0]["name"], "Bash");
    assert_eq!(sent_json["messages"][0]["content"], "list files");
}

/// Regression: a tool whose `input_schema` has a typeless `args` property (only
/// a description, no `type`) made llama-server 400 the whole turn with
/// "Unrecognized schema". The adapter must coerce that node to `{}` (= "any")
/// in the body it forwards upstream, while leaving typed siblings intact.
#[tokio::test]
async fn adapter_sanitizes_typeless_tool_schema_before_forwarding() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(upstream_port, captured.clone()));
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let req_body = json!({
        "model": "claude-test",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "name": "Workflow",
            "description": "Run a workflow",
            "input_schema": {
                "type": "object",
                "properties": {
                    "script": {"type": "string", "description": "the script"},
                    "args": {"description": "Optional input value exposed to the script as the global `args`, verbatim."}
                }
            }
        }]
    });
    let resp = client
        .post(format!("http://127.0.0.1:{adapter_port}/v1/messages"))
        .json(&req_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream never received");
    let sent_json: Value = serde_json::from_slice(&sent).unwrap();
    let props = &sent_json["tools"][0]["input_schema"]["properties"];
    // Typeless `args` was coerced to an empty schema {} that llama-server accepts.
    assert_eq!(props["args"], json!({}));
    // Typed sibling is forwarded unchanged.
    assert_eq!(props["script"]["type"], "string");
    assert_eq!(props["script"]["description"], "the script");
}

#[tokio::test]
async fn adapter_adds_llama_progress_only_when_enabled() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(upstream_port, captured.clone()));
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: None,
        collect_llama_progress: true,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{adapter_port}/v1/messages"))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream never received");
    let sent_json: Value = serde_json::from_slice(&sent).unwrap();
    assert_eq!(sent_json["return_progress"], json!(true));
}

type CapturedHeaders = Arc<Mutex<Option<HashMap<String, String>>>>;

/// Fake cloud router that records the headers of the first
/// `POST /v1/usage/update` callback it receives, then 200s.
async fn fake_router_capturing_usage(port: u16, captured: CapturedHeaders) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let captured = captured.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let captured = captured.clone();
                async move {
                    if req.uri().path() == "/v1/usage/update" {
                        let mut hdrs = HashMap::new();
                        for (k, v) in req.headers().iter() {
                            if let Ok(s) = v.to_str() {
                                hdrs.insert(k.as_str().to_lowercase(), s.to_string());
                            }
                        }
                        *captured.lock().unwrap() = Some(hdrs);
                    }
                    let _ = req.into_body().collect().await;
                    let resp: Response<Full<Bytes>> = Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                        .unwrap();
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// Regression: the `/v1/usage/update` callback must authenticate with the
/// router key the injector stashed pre-redirect, not the Max-plan OAuth JWT
/// that survives the cross-origin 307 in `Authorization`. The router rejects
/// the JWT ("Decoding Firebase ID token failed"), leaving local usage stuck as
/// a 0-token `local_redirect` placeholder. The adapter must prefer the stash
/// whenever a `usage_doc_id` is present, even though the inbound headers here
/// are non-empty.
#[tokio::test]
async fn usage_callback_prefers_stashed_router_key_over_inbound_oauth_jwt() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let router_port = free_port();

    let captured_upstream: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(
        upstream_port,
        captured_upstream.clone(),
    ));

    let captured_callback: CapturedHeaders = Arc::new(Mutex::new(None));
    tokio::spawn(fake_router_capturing_usage(
        router_port,
        captured_callback.clone(),
    ));

    // Injector stashed the user's real router key (in x-api-key) keyed by the
    // usage_doc_id before the cloud router's 307.
    let doc_id = "rt_deadbeef-001";
    let auth_cache: rayline_adapter::AuthCache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut stash = HashMap::new();
        stash.insert(
            "x-api-key".to_string(),
            "rayline-stashed-router-key".to_string(),
        );
        auth_cache.lock().unwrap().insert(doc_id.to_string(), stash);
    }

    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: format!("http://127.0.0.1:{router_port}"),
        auth_cache: Some(auth_cache),
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/messages?usage_doc_id={doc_id}"
        ))
        // Post-307 inbound state: only the unverifiable OAuth JWT survives; the
        // Router x-api-key is gone.
        .header("authorization", "Bearer oauth.jwt.token")
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // The callback is fired fire-and-forget; poll for it (≤2s).
    let mut hdrs = None;
    for _ in 0..40 {
        if let Some(h) = captured_callback.lock().unwrap().clone() {
            hdrs = Some(h);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let hdrs = hdrs.expect("router never received /v1/usage/update callback");

    assert_eq!(
        hdrs.get("x-api-key").map(String::as_str),
        Some("rayline-stashed-router-key"),
        "callback must carry the stashed router key"
    );
    assert_ne!(
        hdrs.get("authorization").map(String::as_str),
        Some("Bearer oauth.jwt.token"),
        "callback must not forward the inbound OAuth JWT"
    );
}

type CapturedJson = Arc<Mutex<Option<Value>>>;

/// Fake cloud router that records the JSON body of the first
/// `POST /v1/usage/update` callback it receives, then 200s.
async fn fake_router_capturing_body(port: u16, captured: CapturedJson) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let captured = captured.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let captured = captured.clone();
                async move {
                    let is_update = req.uri().path() == "/v1/usage/update";
                    let bytes = req.into_body().collect().await.unwrap().to_bytes();
                    if is_update {
                        if let Ok(v) = serde_json::from_slice::<Value>(&bytes) {
                            *captured.lock().unwrap() = Some(v);
                        }
                    }
                    let resp: Response<Full<Bytes>> = Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from_static(b"{\"ok\":true}")))
                        .unwrap();
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// Fake upstream that rejects every turn with a 400 — mimics llama-server
/// 400ing a turn it can't build a grammar for.
async fn fake_anthropic_upstream_400(port: u16) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| async move {
                let _ = req.into_body().collect().await;
                let resp: Response<Full<Bytes>> = Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .header("content-type", "application/json")
                    .body(Full::new(Bytes::from_static(
                        b"{\"error\":{\"message\":\"Unrecognized schema\"}}",
                    )))
                    .unwrap();
                Ok::<_, Infallible>(resp)
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// Regression: when local inference fails (upstream non-2xx), the adapter must
/// close the placeholder by firing `/v1/usage/update` with `status: "error"`
/// instead of returning 502 and leaving the row to leak as a 0-token
/// `local_redirect` placeholder forever.
#[tokio::test]
async fn upstream_error_closes_placeholder_as_error() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let router_port = free_port();

    tokio::spawn(fake_anthropic_upstream_400(upstream_port));

    let captured_body: CapturedJson = Arc::new(Mutex::new(None));
    tokio::spawn(fake_router_capturing_body(
        router_port,
        captured_body.clone(),
    ));

    let doc_id = "rt_deadbeef-002";
    let auth_cache: rayline_adapter::AuthCache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut stash = HashMap::new();
        stash.insert(
            "x-api-key".to_string(),
            "rayline-stashed-router-key".to_string(),
        );
        auth_cache.lock().unwrap().insert(doc_id.to_string(), stash);
    }

    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: format!("http://127.0.0.1:{router_port}"),
        auth_cache: Some(auth_cache),
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/messages?usage_doc_id={doc_id}"
        ))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    // Client sees the upstream failure surfaced as a 502.
    assert_eq!(resp.status(), 502);

    // The error-close callback is fire-and-forget; poll for it (≤2s).
    let mut body = None;
    for _ in 0..40 {
        if let Some(v) = captured_body.lock().unwrap().clone() {
            body = Some(v);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let body = body.expect("router never received /v1/usage/update error-close");
    assert_eq!(body["routeId"], doc_id);
    assert_eq!(body["status"], "error");
    assert_eq!(body["inputTokens"], 0);
    assert_eq!(body["outputTokens"], 0);
}

/// Regression: when local inference is unreachable (`send().await` errors —
/// the local server is down/restarting or the process OOM'd), the adapter must
/// still close the placeholder via /v1/usage/update with `status: "error"`
/// instead of letting `?` bypass the callback and leak the row forever.
#[tokio::test]
async fn upstream_unreachable_closes_placeholder_as_error() {
    // free_port() binds then releases — nothing is listening on it, so the
    // adapter's outbound request is refused before any headers arrive.
    let dead_upstream_port = free_port();
    let adapter_port = free_port();
    let router_port = free_port();

    let captured_body: CapturedJson = Arc::new(Mutex::new(None));
    tokio::spawn(fake_router_capturing_body(
        router_port,
        captured_body.clone(),
    ));

    let doc_id = "rt_deadbeef-003";
    let auth_cache: rayline_adapter::AuthCache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut stash = HashMap::new();
        stash.insert(
            "x-api-key".to_string(),
            "rayline-stashed-router-key".to_string(),
        );
        auth_cache.lock().unwrap().insert(doc_id.to_string(), stash);
    }

    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{dead_upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: format!("http://127.0.0.1:{router_port}"),
        auth_cache: Some(auth_cache),
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/messages?usage_doc_id={doc_id}"
        ))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);

    let mut body = None;
    for _ in 0..40 {
        if let Some(v) = captured_body.lock().unwrap().clone() {
            body = Some(v);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let body = body.expect("router never received /v1/usage/update error-close");
    assert_eq!(body["routeId"], doc_id);
    assert_eq!(body["status"], "error");
    assert_eq!(body["inputTokens"], 0);
    assert_eq!(body["outputTokens"], 0);
}

/// Fake upstream that returns a 200 chunked `text/event-stream`, writes
/// `sse_body` as one chunk, then either completes the chunked body cleanly
/// (`terminate = true`) or drops the connection without the terminating chunk
/// (`terminate = false`, so the adapter's body stream surfaces a transport
/// error mid-stream rather than a clean EOF).
async fn fake_chunked_sse_upstream(port: u16, sse_body: String, terminate: bool) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (mut sock, _) = listener.accept().await.unwrap();
        let sse_body = sse_body.clone();
        tokio::spawn(async move {
            // Drain the request head (best-effort; the small JSON request fits
            // in one loopback segment, already buffered before we respond).
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
            let chunk = format!("{:x}\r\n{}\r\n", sse_body.len(), sse_body);
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(chunk.as_bytes()).await;
            if terminate {
                let _ = sock.write_all(b"0\r\n\r\n").await;
            }
            let _ = sock.flush().await;
        });
    }
}

/// Like `fake_chunked_sse_upstream` but writes `sse_body` as TWO HTTP chunks split
/// at `split_at` bytes, so a CRLF frame separator can straddle a chunk boundary.
async fn fake_chunked_sse_upstream_split(port: u16, sse_body: String, split_at: usize) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (mut sock, _) = listener.accept().await.unwrap();
        let sse_body = sse_body.clone();
        tokio::spawn(async move {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n";
            let (a, b) = sse_body.split_at(split_at);
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock
                .write_all(format!("{:x}\r\n{}\r\n", a.len(), a).as_bytes())
                .await;
            let _ = sock.flush().await;
            // Small gap so the adapter processes the first chunk before the second.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let _ = sock
                .write_all(format!("{:x}\r\n{}\r\n", b.len(), b).as_bytes())
                .await;
            let _ = sock.write_all(b"0\r\n\r\n").await;
            let _ = sock.flush().await;
        });
    }
}

/// Boot the adapter (+ a usage-capturing fake router) pointed at `upstream_port`,
/// fire one streaming `/api/v1/messages` carrying `usage_doc_id`, and return the
/// JSON body of the `/v1/usage/update` close callback the router received.
async fn drive_streaming_and_capture_close(upstream_port: u16, doc_id: &str) -> Value {
    let adapter_port = free_port();
    let router_port = free_port();

    let captured_body: CapturedJson = Arc::new(Mutex::new(None));
    tokio::spawn(fake_router_capturing_body(
        router_port,
        captured_body.clone(),
    ));

    let auth_cache: rayline_adapter::AuthCache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut stash = HashMap::new();
        stash.insert(
            "x-api-key".to_string(),
            "rayline-stashed-router-key".to_string(),
        );
        auth_cache.lock().unwrap().insert(doc_id.to_string(), stash);
    }

    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: format!("http://127.0.0.1:{router_port}"),
        auth_cache: Some(auth_cache),
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/messages?usage_doc_id={doc_id}"
        ))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    // The adapter returns 200 + begins streaming before any terminal/drop.
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await; // drain; may error on a truncated stream

    for _ in 0..60 {
        if let Some(v) = captured_body.lock().unwrap().clone() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router never received /v1/usage/update close callback");
}

/// Regression: a local stream that drops mid-flight (partial output, no terminal
/// event) must close the placeholder as `status: "error"`. Token counts can't
/// distinguish a truncation from a clean finish, so the adapter gates on the
/// terminal `message_stop`/`[DONE]` marker — not on `output_tokens > 0`.
#[tokio::test]
async fn truncated_local_stream_closes_as_error() {
    let upstream_port = free_port();
    let partial = "event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"model\":\"qwen3.6-35b-a3b\",\"usage\":{\"input_tokens\":42,\"output_tokens\":17}}}\n\n"
        .to_string();
    tokio::spawn(fake_chunked_sse_upstream(upstream_port, partial, false));

    let body = drive_streaming_and_capture_close(upstream_port, "rt_deadbeef-004").await;
    assert_eq!(body["routeId"], "rt_deadbeef-004");
    // Truncated mid-stream with no terminal event → error, even though
    // outputTokens > 0.
    assert_eq!(body["status"], "error");
    assert_eq!(body["outputTokens"], 17);
}

/// Regression: a complete streaming turn (full SSE ending in `message_stop`)
/// must still close as `status: "success"`. Guards the terminal-marker gate
/// against mislabeling real successful local streams as errors.
#[tokio::test]
async fn streaming_success_closes_as_success() {
    let upstream_port = free_port();
    let complete = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"qwen3.6-35b-a3b\",\"usage\":{\"input_tokens\":42,\"output_tokens\":1}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string();
    tokio::spawn(fake_chunked_sse_upstream(upstream_port, complete, true));

    let body = drive_streaming_and_capture_close(upstream_port, "rt_deadbeef-005").await;
    assert_eq!(body["routeId"], "rt_deadbeef-005");
    assert_eq!(body["status"], "success");
    assert_eq!(body["outputTokens"], 17);
    assert_eq!(body["selectedModel"], "qwen3.6-35b-a3b");
}

/// Drive one streaming `/api/v1/responses` (may-local 307 target) carrying
/// `usage_doc_id`, return the `/v1/usage/update` close-callback body.
async fn drive_responses_streaming_and_capture_close(upstream_port: u16, doc_id: &str) -> Value {
    let adapter_port = free_port();
    let router_port = free_port();

    let captured_body: CapturedJson = Arc::new(Mutex::new(None));
    tokio::spawn(fake_router_capturing_body(
        router_port,
        captured_body.clone(),
    ));

    let auth_cache: rayline_adapter::AuthCache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut stash = HashMap::new();
        stash.insert(
            "x-api-key".to_string(),
            "rayline-stashed-router-key".to_string(),
        );
        auth_cache.lock().unwrap().insert(doc_id.to_string(), stash);
    }

    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-27b-iq3xxs".into(),
        router_url: format!("http://127.0.0.1:{router_port}"),
        auth_cache: Some(auth_cache),
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/responses?usage_doc_id={doc_id}"
        ))
        .json(&json!({
            "model": "rayline-auto", "stream": true,
            "input": [{"role": "user", "content": [{"type": "input_text", "text": "hi"}]}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;

    for _ in 0..60 {
        if let Some(v) = captured_body.lock().unwrap().clone() {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("router never received /v1/usage/update close callback");
}

/// Regression: a CRLF-delimited Responses SSE stream (standards-compliant
/// `\r\n\r\n` frame separators) must still be parsed — its `response.completed`
/// observed and the turn closed as `status: "success"`. A parser that only
/// splits on `\n\n` never sees the terminal event and wrongly closes a
/// successful may-local turn as an error.
#[tokio::test]
async fn crlf_responses_stream_closes_as_success() {
    let upstream_port = free_port();
    // CRLF line endings AND CRLF frame separators, as a compliant SSE server emits.
    let complete = concat!(
        "event: response.created\r\n",
        "data: {\"type\":\"response.created\",\"response\":{\"model\":\"qwen3.6-27b-iq3xxs\"}}\r\n\r\n",
        "event: response.completed\r\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":17}}}\r\n\r\n",
    )
    .to_string();
    tokio::spawn(fake_chunked_sse_upstream(upstream_port, complete, true));

    let body = drive_responses_streaming_and_capture_close(upstream_port, "rt_deadbeef-crlf").await;
    assert_eq!(body["routeId"], "rt_deadbeef-crlf");
    assert_eq!(body["status"], "success");
}

/// Regression: a CRLF frame separator split across a TCP chunk boundary (chunk 1
/// ends with `\r`, chunk 2 starts with `\n`) must still be normalized — otherwise
/// per-chunk normalization misses it and the terminal event is never observed.
#[tokio::test]
async fn crlf_responses_stream_split_across_chunks_closes_as_success() {
    let upstream_port = free_port();
    let complete = concat!(
        "event: response.created\r\n",
        "data: {\"type\":\"response.created\",\"response\":{\"model\":\"qwen3.6-27b-iq3xxs\"}}\r\n\r\n",
        "event: response.completed\r\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":42,\"output_tokens\":17}}}\r\n\r\n",
    )
    .to_string();
    // Split inside the frame separator after `created`'s data line: place the cut
    // between the first `\r` and its `\n` so neither chunk contains a whole `\r\n`.
    let sep = complete.find("}\r\n\r\nevent: response.completed").unwrap();
    let split_at = sep + 1 + 1; // after `}` and the first `\r`, before its `\n`
    tokio::spawn(fake_chunked_sse_upstream_split(
        upstream_port,
        complete,
        split_at,
    ));

    let body =
        drive_responses_streaming_and_capture_close(upstream_port, "rt_deadbeef-crlf-split").await;
    assert_eq!(body["routeId"], "rt_deadbeef-crlf-split");
    assert_eq!(body["status"], "success");
}

#[tokio::test]
async fn streaming_prompt_progress_records_prompt_cache_ratio() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let router_port = free_port();
    let doc_id = "rt_deadbeef-007";
    let complete = concat!(
        "event: completion\n",
        "data: {\"prompt_progress\":{\"total\":100,\"cache\":75,\"processed\":100,\"time_ms\":500}}\n\n",
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"qwen3.6-35b-a3b\",\"usage\":{\"input_tokens\":100,\"output_tokens\":0}}}\n\n",
        "event: message_delta\n",
        "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":17}}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string();
    tokio::spawn(fake_chunked_sse_upstream(upstream_port, complete, true));

    let captured_body: CapturedJson = Arc::new(Mutex::new(None));
    tokio::spawn(fake_router_capturing_body(
        router_port,
        captured_body.clone(),
    ));

    let auth_cache: rayline_adapter::AuthCache = Arc::new(Mutex::new(HashMap::new()));
    {
        let mut stash = HashMap::new();
        stash.insert(
            "x-api-key".to_string(),
            "rayline-stashed-router-key".to_string(),
        );
        auth_cache.lock().unwrap().insert(doc_id.to_string(), stash);
    }

    let metrics = RouterMetrics::new("test-router");
    let metrics_sink: SharedMetricsSink = metrics.clone();
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: format!("http://127.0.0.1:{router_port}"),
        auth_cache: Some(auth_cache),
        metrics: Some(metrics_sink),
        collect_llama_progress: true,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let resp = client
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/messages?usage_doc_id={doc_id}"
        ))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _ = resp.bytes().await;

    for _ in 0..60 {
        let snapshot = metrics.snapshot();
        if let Some(row) = snapshot.recent.first() {
            assert_eq!(row.prompt_tokens, Some(100));
            assert_eq!(row.prompt_cache_tokens, Some(75));
            assert_eq!(row.prompt_processed_tokens, Some(100));
            assert_eq!(row.cache_hit_ratio, Some(0.75));
            assert_eq!(row.prefill_tps, Some(50.0));
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("metrics never received completed local stream");
}

#[tokio::test]
async fn direct_messages_without_usage_doc_does_not_start_metrics_row() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(upstream_port, captured));

    let metrics = RouterMetrics::new("test-router");
    let metrics_sink: SharedMetricsSink = metrics.clone();
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: Some(metrics_sink),
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{adapter_port}/v1/messages"))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _body: Value = resp.json().await.unwrap();

    let snapshot = metrics.snapshot();
    assert!(snapshot.active.is_empty());
    assert!(snapshot.recent.is_empty());
    assert_eq!(snapshot.totals.active_requests, 0);
    assert_eq!(snapshot.totals.completed_requests, 0);
    assert_eq!(snapshot.totals.errored_requests, 0);
}

#[tokio::test]
async fn redirected_messages_use_query_request_id_for_metrics() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(upstream_port, captured));

    let metrics = RouterMetrics::new("test-router");
    let metrics_sink: SharedMetricsSink = metrics.clone();
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: Some(metrics_sink),
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let resp = reqwest::Client::new()
        .post(format!(
            "http://127.0.0.1:{adapter_port}/api/v1/messages?usage_doc_id=local-1&rayline_request_id=req_shared"
        ))
        .json(&json!({
            "model": "claude-test",
            "max_tokens": 32,
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let _body: Value = resp.json().await.unwrap();

    for _ in 0..30 {
        let snapshot = metrics.snapshot();
        if let Some(row) = snapshot.recent.first() {
            assert!(snapshot.active.is_empty());
            assert_eq!(row.request_id, "req_shared");
            assert_eq!(row.route_id.as_deref(), Some("local-1"));
            assert_eq!(row.input_tokens, Some(42));
            assert_eq!(row.output_tokens, Some(17));
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("metrics never received completed redirected request");
}

/// Regression: a 200 stream that reports an application failure as an
/// `event: error` payload and then closes cleanly (no `message_stop`) must
/// close as `status: "error"`, not a savings-bearing success row.
#[tokio::test]
async fn sse_error_event_closes_as_error() {
    let upstream_port = free_port();
    let err_stream = concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"model\":\"qwen3.6-35b-a3b\",\"usage\":{\"input_tokens\":42,\"output_tokens\":3}}}\n\n",
        "event: error\n",
        "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"oom\"}}\n\n",
    )
    .to_string();
    tokio::spawn(fake_chunked_sse_upstream(upstream_port, err_stream, true));

    let body = drive_streaming_and_capture_close(upstream_port, "rt_deadbeef-006").await;
    assert_eq!(body["routeId"], "rt_deadbeef-006");
    // Clean close, no message_stop, error event seen → error.
    assert_eq!(body["status"], "error");
}

#[tokio::test]
async fn adapter_healthz() {
    let port = free_port();
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port,
        target: "http://127.0.0.1:1".into(),
        upstream_model: "x".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/healthz"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["port"], port);
}

/// Fake Responses-shaped upstream (mimics llama-server / LM Studio's
/// `/v1/responses`): records the forwarded body, returns a minimal Response.
async fn fake_responses_upstream(port: u16, captured: CapturedBody) {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr).await.unwrap();
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        let io = TokioIo::new(stream);
        let captured = captured.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req: Request<Incoming>| {
                let captured = captured.clone();
                async move {
                    let bytes = req.into_body().collect().await.unwrap().to_bytes();
                    *captured.lock().unwrap() = Some(bytes.to_vec());
                    let body = json!({
                        "id": "resp_fake",
                        "object": "response",
                        "status": "completed",
                        "model": "qwen3.6-27b-iq3xxs",
                        "output": [{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "ok"}]
                        }],
                        "usage": {"input_tokens": 12, "output_tokens": 3}
                    });
                    let resp: Response<Full<Bytes>> = Response::builder()
                        .status(StatusCode::OK)
                        .header("content-type", "application/json")
                        .body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                        .unwrap();
                    Ok::<_, Infallible>(resp)
                }
            });
            let _ = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await;
        });
    }
}

/// The `web_search` hosted tool a real Codex explorer subagent ships cannot run
/// on any local backend: llama-server drops it silently, LM Studio misroutes the
/// model's search intent onto another tool. The adapter must strip it from the
/// body it forwards on the Responses path (both may-local 307 → `/api/v1/responses`
/// and direct `/v1/responses`), leaving the executable function tools intact.
#[tokio::test]
async fn adapter_strips_web_search_hosted_tool_on_responses_path() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_responses_upstream(upstream_port, captured.clone()));
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-27b-iq3xxs".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let req_body = json!({
        "model": "rayline-auto",
        "stream": false,
        "input": [{"role": "user", "content": [{"type": "input_text", "text": "find it"}]}],
        "tools": [
            {"type": "web_search"},
            {"type": "function", "name": "exec_command", "parameters": {"type": "object"}}
        ]
    });
    let resp = client
        .post(format!("http://127.0.0.1:{adapter_port}/api/v1/responses"))
        .json(&req_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream never received");
    let sent_json: Value = serde_json::from_slice(&sent).unwrap();
    let tools = sent_json["tools"].as_array().unwrap();
    // web_search is gone; the executable function tool survives.
    assert!(
        !tools.iter().any(|t| t["type"] == "web_search"),
        "web_search must be stripped before the local forward"
    );
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "exec_command");
}

/// Symmetric to the Responses test: on the Anthropic `/v1/messages` path the
/// hosted server-tool shape is `{"type":"web_search_20250305"}`. It must be
/// stripped before the local forward, leaving client-executed tools (no `type`)
/// intact.
#[tokio::test]
async fn adapter_strips_web_search_hosted_tool_on_messages_path() {
    let upstream_port = free_port();
    let adapter_port = free_port();
    let captured: CapturedBody = Arc::new(Mutex::new(None));
    tokio::spawn(fake_anthropic_upstream(upstream_port, captured.clone()));
    tokio::spawn(rayline_adapter::serve(rayline_adapter::AdapterOptions {
        port: adapter_port,
        target: format!("http://127.0.0.1:{upstream_port}"),
        upstream_model: "qwen3.6-35b-a3b".into(),
        router_url: "http://127.0.0.1:1".into(),
        auth_cache: None,
        metrics: None,
        collect_llama_progress: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;

    let client = reqwest::Client::new();
    let req_body = json!({
        "model": "claude-test",
        "max_tokens": 32,
        "messages": [{"role": "user", "content": "find it"}],
        "tools": [
            {"type": "web_search_20250305", "name": "web_search"},
            {"name": "Bash", "input_schema": {"type": "object"}}
        ]
    });
    let resp = client
        .post(format!("http://127.0.0.1:{adapter_port}/v1/messages"))
        .json(&req_body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    let sent = captured
        .lock()
        .unwrap()
        .clone()
        .expect("upstream never received");
    let sent_json: Value = serde_json::from_slice(&sent).unwrap();
    let tools = sent_json["tools"].as_array().unwrap();
    // The versioned web_search server tool is gone; the client tool survives.
    assert!(
        !tools.iter().any(|t| t["type"]
            .as_str()
            .is_some_and(|s| s.starts_with("web_search"))),
        "web_search_* must be stripped before the local forward"
    );
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["name"], "Bash");
}
