use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rcgen::{CertificateParams, KeyPair};
use rustls::ServerConfig;
use rustls::pki_types::PrivateKeyDer;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[derive(Clone, Debug)]
struct CapturedRequest {
    method: String,
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
}

type CapturedRequests = Arc<Mutex<Vec<CapturedRequest>>>;

#[derive(Clone)]
struct FakeResponse {
    status: StatusCode,
    headers: Vec<(String, String)>,
    body: Bytes,
}

struct FakeHttpsServer {
    port: u16,
    cert_der: Vec<u8>,
    cert_pem: String,
    captured: CapturedRequests,
}

struct FakeHttpServer {
    port: u16,
    captured: CapturedRequests,
}

fn free_port() -> u16 {
    let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("rayline_proxy=debug")
        .with_test_writer()
        .try_init();
}

async fn spawn_fake_https_server(hostname: &str, response: FakeResponse) -> FakeHttpsServer {
    let port = free_port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let (config, cert_der, cert_pem) = self_signed_server_config(hostname);
    let acceptor = TlsAcceptor::from(config);
    let captured_for_task = captured.clone();

    tokio::spawn(async move {
        let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let listener = TcpListener::bind(addr).await.unwrap();
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let acceptor = acceptor.clone();
            let captured = captured_for_task.clone();
            let response = response.clone();
            tokio::spawn(async move {
                let tls = acceptor.accept(stream).await.unwrap();
                let io = TokioIo::new(tls);
                let svc = service_fn(move |req: Request<Incoming>| {
                    let captured = captured.clone();
                    let response = response.clone();
                    async move {
                        let method = req.method().as_str().to_string();
                        let path_and_query = req
                            .uri()
                            .path_and_query()
                            .map(|p| p.as_str().to_string())
                            .unwrap_or_else(|| "/".to_string());
                        let headers = req
                            .headers()
                            .iter()
                            .map(|(k, v)| {
                                (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                            })
                            .collect();
                        let body = req.into_body().collect().await.unwrap().to_bytes();
                        captured.lock().unwrap().push(CapturedRequest {
                            method,
                            path_and_query,
                            headers,
                            body: body.to_vec(),
                        });

                        let mut builder = Response::builder().status(response.status);
                        for (name, value) in &response.headers {
                            builder = builder.header(name, value);
                        }
                        Ok::<_, Infallible>(builder.body(Full::new(response.body.clone())).unwrap())
                    }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(io, svc)
                    .await;
            });
        }
    });

    FakeHttpsServer {
        port,
        cert_der,
        cert_pem,
        captured,
    }
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
                        captured.lock().unwrap().push(request);
                        let body = Bytes::from_static(
                            br#"{"id":"msg_local","type":"message","role":"assistant","model":"local-qwen","content":[{"type":"text","text":"local ok"}],"usage":{"input_tokens":11,"output_tokens":7}}"#,
                        );
                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .body(Full::new(body))
                                .unwrap(),
                        )
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

async fn spawn_fake_local_router(redirect_port: u16) -> FakeHttpServer {
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
                                            "http://127.0.0.1:{redirect_port}/api/v1/messages?usage_doc_id=doc-local"
                                        ),
                                    )
                                    .body(Full::new(Bytes::new()))
                                    .unwrap(),
                            )
                        } else if path == "/v1/usage/update" {
                            Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Full::new(Bytes::from_static(b"{}")))
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

async fn capture_request(req: Request<Incoming>) -> CapturedRequest {
    let method = req.method().as_str().to_string();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers = req
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = req.into_body().collect().await.unwrap().to_bytes();
    CapturedRequest {
        method,
        path_and_query,
        headers,
        body: body.to_vec(),
    }
}

fn self_signed_server_config(hostname: &str) -> (Arc<ServerConfig>, Vec<u8>, String) {
    let mut subject_alt_names = vec![hostname.to_string()];
    if hostname == "localhost" {
        subject_alt_names.push("127.0.0.1".to_string());
    }
    let params = CertificateParams::new(subject_alt_names).unwrap();
    let key_pair = KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().clone();
    let cert_pem = cert.pem();
    let key_der = key_pair.serialize_der();
    let config =
        ServerConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![cert_der.clone()], PrivateKeyDer::Pkcs8(key_der.into()))
            .unwrap();
    (Arc::new(config), cert_der.to_vec(), cert_pem)
}

fn proxy_options(
    port: u16,
    ca_dir: &Path,
    router_url: String,
    anthropic_url: String,
    upstreams: &[&FakeHttpsServer],
) -> rayline_proxy::ProxyOptions {
    let mut opts = rayline_proxy::ProxyOptions::with_ca_paths(
        "rsk-rayline-test",
        ca_dir.join("proxy-ca.pem"),
        ca_dir.join("proxy-ca-key.pem"),
    );
    opts.port = port;
    opts.router_url = router_url;
    opts.anthropic_url = anthropic_url;
    if !upstreams.is_empty() {
        let upstream_ca_path = ca_dir.join("upstream-ca.pem");
        let mut bundle = String::new();
        for upstream in upstreams {
            bundle.push_str(&upstream.cert_pem);
            bundle.push('\n');
        }
        std::fs::write(&upstream_ca_path, bundle).unwrap();
        opts.upstream_ca_path = Some(upstream_ca_path);
    }
    opts
}

async fn spawn_proxy(opts: rayline_proxy::ProxyOptions) {
    let port = opts.port;
    tokio::spawn(rayline_proxy::serve(opts));
    wait_for_proxy_health(port).await;
}

async fn wait_for_proxy_health(port: u16) {
    let url = format!("http://127.0.0.1:{port}/healthz");
    for _ in 0..50 {
        if let Ok(resp) = reqwest::get(&url).await {
            if resp.status().is_success() {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("proxy did not become healthy on {port}");
}

fn proxied_client(proxy_port: u16, ca_cert_path: &Path) -> reqwest::Client {
    let proxy_ca = std::fs::read(ca_cert_path).unwrap();
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(&proxy_ca).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

#[tokio::test]
async fn proxy_routes_router_and_anthropic_paths_with_correct_auth() {
    init_tracing();
    let router = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"ok":"router"}"#),
        },
    )
    .await;
    let anthropic = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"ok":"anthropic"}"#),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client(proxy_port, &ca_cert_path);
    let router_resp = client
        .post("https://api.anthropic.com/v1/messages?beta=true")
        .header("authorization", "Bearer claude-oauth")
        .header("x-api-key", "claude-api-key")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"rayline-router"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(router_resp.status(), StatusCode::OK);

    let router_seen = router.captured.lock().unwrap().clone();
    assert_eq!(router_seen.len(), 1);
    assert_eq!(router_seen[0].method, "POST");
    assert_eq!(router_seen[0].path_and_query, "/v1/messages?beta=true");
    assert_eq!(
        router_seen[0].header("x-api-key"),
        Some("rsk-rayline-test".to_string())
    );
    assert_eq!(router_seen[0].header("authorization"), None);
    assert_eq!(
        router_seen[0].header("anthropic-version"),
        Some("2023-06-01".to_string())
    );
    assert_eq!(
        String::from_utf8(router_seen[0].body.clone()).unwrap(),
        r#"{"model":"rayline-router"}"#
    );

    let anthropic_resp = client
        .get("https://api.anthropic.com/v1/mcp_servers?limit=1000")
        .header("authorization", "Bearer claude-oauth")
        .send()
        .await
        .unwrap();
    assert_eq!(anthropic_resp.status(), StatusCode::OK);

    let anthropic_seen = anthropic.captured.lock().unwrap().clone();
    assert_eq!(anthropic_seen.len(), 1);
    assert_eq!(anthropic_seen[0].method, "GET");
    assert_eq!(
        anthropic_seen[0].path_and_query,
        "/v1/mcp_servers?limit=1000"
    );
    assert_eq!(
        anthropic_seen[0].header("authorization"),
        Some("Bearer claude-oauth".to_string())
    );
    assert_eq!(anthropic_seen[0].header("x-api-key"), None);
}

#[tokio::test]
async fn selective_proxy_routes_only_subagent_messages_to_router() {
    init_tracing();
    let router = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"ok":"router"}"#),
        },
    )
    .await;
    let anthropic = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"ok":"anthropic"}"#),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client(proxy_port, &ca_cert_path);
    let main_resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer claude-oauth")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(main_resp.status(), StatusCode::OK);
    assert_eq!(
        main_resp.json::<serde_json::Value>().await.unwrap()["ok"],
        "anthropic"
    );

    let subagent_resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer claude-oauth")
        .header("x-api-key", "claude-api-key")
        .header("x-claude-code-agent-id", "agent-123")
        .header("x-claude-code-parent-agent-id", "parent-456")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(subagent_resp.status(), StatusCode::OK);
    assert_eq!(
        subagent_resp.json::<serde_json::Value>().await.unwrap()["ok"],
        "router"
    );

    let anthropic_seen = anthropic.captured.lock().unwrap().clone();
    assert_eq!(anthropic_seen.len(), 1);
    assert_eq!(anthropic_seen[0].path_and_query, "/v1/messages");
    assert_eq!(
        anthropic_seen[0].header("authorization"),
        Some("Bearer claude-oauth".to_string())
    );
    assert_eq!(anthropic_seen[0].header("x-api-key"), None);
    let anthropic_body: serde_json::Value =
        serde_json::from_slice(&anthropic_seen[0].body).unwrap();
    assert_eq!(anthropic_body["model"], "claude-sonnet-4-5");

    let router_seen = router.captured.lock().unwrap().clone();
    assert_eq!(router_seen.len(), 1);
    assert_eq!(router_seen[0].path_and_query, "/v1/messages");
    assert_eq!(
        router_seen[0].header("x-api-key"),
        Some("rsk-rayline-test".to_string())
    );
    assert_eq!(router_seen[0].header("authorization"), None);
    assert_eq!(
        router_seen[0].header("x-claude-code-agent-id"),
        Some("agent-123".to_string())
    );
    assert_eq!(
        router_seen[0].header("x-claude-code-parent-agent-id"),
        Some("parent-456".to_string())
    );
    assert_eq!(
        router_seen[0].header("anthropic-version"),
        Some("2023-06-01".to_string())
    );
    let router_body: serde_json::Value = serde_json::from_slice(&router_seen[0].body).unwrap();
    assert_eq!(router_body["model"], "rayline-router");
    assert_eq!(
        router_seen[0].header("content-length"),
        Some(router_seen[0].body.len().to_string())
    );
}

#[tokio::test]
async fn selective_proxy_routes_model_list_discovery_to_router() {
    init_tracing();
    let router = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"data":[{"id":"rayline-router"}]}"#),
        },
    )
    .await;
    let anthropic = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"data":[{"id":"claude-sonnet-4-5"}]}"#),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client(proxy_port, &ca_cert_path);
    let resp = client
        .get("https://api.anthropic.com/v1/models")
        .header("authorization", "Bearer claude-oauth")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["data"][0]["id"],
        "rayline-router"
    );

    let provider_detail_resp = client
        .get("https://api.anthropic.com/v1/models/z-ai/glm-x-preview")
        .header("authorization", "Bearer claude-oauth")
        .send()
        .await
        .unwrap();
    assert_eq!(provider_detail_resp.status(), StatusCode::OK);

    let router_seen = router.captured.lock().unwrap().clone();
    assert_eq!(router_seen.len(), 2);
    assert_eq!(router_seen[0].method, "GET");
    assert_eq!(router_seen[0].path_and_query, "/v1/models");
    assert_eq!(router_seen[1].method, "GET");
    assert_eq!(
        router_seen[1].path_and_query,
        "/v1/models/z-ai/glm-x-preview"
    );
    assert_eq!(
        router_seen[0].header("x-api-key"),
        Some("rsk-rayline-test".to_string())
    );
    assert_eq!(router_seen[0].header("authorization"), None);
    assert!(anthropic.captured.lock().unwrap().is_empty());
}

#[tokio::test]
async fn proxy_writes_route_status_sidecar_from_rayline_headers() {
    init_tracing();
    let router = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "x-rayline-selected-model".to_string(),
                    "glm-4.6".to_string(),
                ),
                (
                    "x-rayline-virtual-model".to_string(),
                    "rayline-router".to_string(),
                ),
                ("x-rayline-policy".to_string(), "balanced".to_string()),
                ("x-rayline-task-class".to_string(), "debugging".to_string()),
                ("x-rayline-route-id".to_string(), "route-it".to_string()),
            ],
            body: Bytes::from_static(br#"{"ok":"router"}"#),
        },
    )
    .await;
    let anthropic = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: Vec::new(),
            body: Bytes::from_static(b"unused"),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let status_dir = tempfile::tempdir().unwrap();
    let status_path = status_dir.path().join("route-status.json");
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    opts.route_status_path = Some(status_path.clone());
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client(proxy_port, &ca_cert_path);
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer claude-oauth")
        .header("x-rayline-local-available", "false")
        .header("x-rayline-local-model-id", "stale-client-model")
        .header("x-rayline-local-hint", "1")
        .body(r#"{"model":"rayline-router"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // The proxy writes the sidecar from a detached task, so poll briefly.
    let mut parsed = None;
    for _ in 0..50 {
        if let Ok(raw) = std::fs::read_to_string(&status_path) {
            parsed = Some(serde_json::from_str::<serde_json::Value>(&raw).unwrap());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let parsed = parsed.expect("route-status sidecar should be written");
    assert_eq!(parsed["selected_model"], "glm-4.6");
    assert_eq!(parsed["virtual_model"], "rayline-router");
    assert_eq!(parsed["policy"], "balanced");
    assert_eq!(parsed["task_class"], "debugging");
    assert_eq!(parsed["route_id"], "route-it");
    assert!(parsed["ts"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn selective_main_passthrough_clears_route_status_sidecar() {
    init_tracing();
    let router = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                (
                    "x-rayline-selected-model".to_string(),
                    "z-ai/glm-5.1".to_string(),
                ),
                (
                    "x-rayline-virtual-model".to_string(),
                    "rayline-router".to_string(),
                ),
                ("x-rayline-policy".to_string(), "delegated".to_string()),
                (
                    "x-rayline-task-class".to_string(),
                    "exploration".to_string(),
                ),
                (
                    "x-rayline-route-id".to_string(),
                    "route-subagent".to_string(),
                ),
            ],
            body: Bytes::from_static(br#"{"ok":"router"}"#),
        },
    )
    .await;
    let anthropic = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            body: Bytes::from_static(br#"{"ok":"anthropic"}"#),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let status_dir = tempfile::tempdir().unwrap();
    let status_path = status_dir.path().join("route-status.json");
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.route_status_path = Some(status_path.clone());
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client(proxy_port, &ca_cert_path);
    let subagent_resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-claude-code-agent-id", "agent-123")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(subagent_resp.status(), StatusCode::OK);

    for _ in 0..50 {
        if status_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(status_path.exists(), "subagent should write route status");

    let main_resp = client
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(main_resp.status(), StatusCode::OK);

    for _ in 0..50 {
        if !status_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !status_path.exists(),
        "selective main passthrough should clear stale route status"
    );
    assert_eq!(anthropic.captured.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn proxy_stashes_router_auth_for_local_307() {
    init_tracing();
    let router = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::TEMPORARY_REDIRECT,
            headers: vec![(
                "location".to_string(),
                "http://127.0.0.1:20808/api/v1/messages?usage_doc_id=doc-307".to_string(),
            )],
            body: Bytes::new(),
        },
    )
    .await;
    let anthropic = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: Vec::new(),
            body: Bytes::from_static(b"unused"),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    let cache = rayline_proxy::new_auth_cache();
    opts.local_available = true;
    opts.local_model_id = Some("local-qwen".to_string());
    opts.auth_cache = Some(cache.clone());
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client(proxy_port, &ca_cert_path);
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer claude-oauth")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);

    let guard = cache.lock().unwrap();
    let headers = guard.get("doc-307").unwrap();
    assert_eq!(
        headers.get("x-api-key"),
        Some(&"rsk-rayline-test".to_string())
    );
    assert!(!headers.contains_key("authorization"));
}

#[tokio::test]
async fn proxy_blind_tunnels_non_anthropic_https_without_proxy_ca() {
    init_tracing();
    let third = spawn_fake_https_server(
        "third.rayline.invalid",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_string(), "text/plain".to_string())],
            body: Bytes::from_static(b"blind-ok"),
        },
    )
    .await;
    let proxy_port = free_port();
    let ca_dir = tempfile::tempdir().unwrap();
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        "https://127.0.0.1:1".to_string(),
        "https://127.0.0.1:1".to_string(),
        &[],
    );
    opts.connect_overrides = HashMap::from([(
        "third.rayline.invalid:443".to_string(),
        format!("127.0.0.1:{}", third.port),
    )]);
    spawn_proxy(opts).await;

    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://127.0.0.1:{proxy_port}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_der(&third.cert_der).unwrap())
        .build()
        .unwrap();
    let resp = client
        .get("https://third.rayline.invalid/blind")
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.text().await.unwrap(), "blind-ok");
    let third_seen = third.captured.lock().unwrap().clone();
    assert_eq!(third_seen.len(), 1);
    assert_eq!(third_seen[0].path_and_query, "/blind");
}

#[tokio::test]
async fn local_proxy_redirect_uses_shared_router_auth_for_usage_update() {
    init_tracing();
    let local_model = spawn_fake_local_model().await;
    let adapter_port = free_port();
    let router = spawn_fake_local_router(rayline_adapter::DEFAULT_PORT).await;
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
        "rsk-rayline-test",
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

    let proxy_ca = std::fs::read(ca_cert_path).unwrap();
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::https(format!("http://127.0.0.1:{proxy_port}")).unwrap())
        .add_root_certificate(reqwest::Certificate::from_pem(&proxy_ca).unwrap())
        .build()
        .unwrap();
    let resp = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer claude-oauth")
        .body(r#"{"model":"rayline-router"}"#)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.json::<serde_json::Value>().await.unwrap()["id"],
        "msg_local"
    );

    let updates = wait_for_usage_updates(&router.captured).await;
    assert_eq!(updates.len(), 1);
    assert_eq!(
        updates[0].header("x-api-key"),
        Some("rsk-rayline-test".to_string())
    );
    assert_eq!(updates[0].header("authorization"), None);
    let update_body: serde_json::Value = serde_json::from_slice(&updates[0].body).unwrap();
    assert_eq!(update_body["routeId"], "doc-local");
    assert_eq!(update_body["inputTokens"], 11);
    assert_eq!(update_body["outputTokens"], 7);

    let router_seen = router.captured.lock().unwrap().clone();
    let message_req = router_seen
        .iter()
        .find(|req| req.path_and_query == "/v1/messages")
        .expect("router did not receive /v1/messages");
    assert_eq!(
        message_req.header("x-rayline-local-available"),
        Some("true".to_string())
    );
    assert_eq!(
        message_req.header("x-rayline-local-model-id"),
        Some("local-qwen".to_string())
    );

    let local_seen = local_model.captured.lock().unwrap().clone();
    assert_eq!(local_seen.len(), 1);
    assert_eq!(local_seen[0].path_and_query, "/v1/messages");
}

async fn wait_for_usage_updates(captured: &CapturedRequests) -> Vec<CapturedRequest> {
    for _ in 0..50 {
        let updates: Vec<CapturedRequest> = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|req| req.path_and_query == "/v1/usage/update")
            .cloned()
            .collect();
        if !updates.is_empty() {
            return updates;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Vec::new()
}
