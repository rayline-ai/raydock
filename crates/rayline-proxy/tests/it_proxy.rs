use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener as StdTcpListener};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::future::join_all;
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

async fn spawn_fake_subscription_anthropic() -> FakeHttpServer {
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
                        let authorization = request.header("authorization").unwrap_or_default();
                        let body_json = serde_json::from_slice::<serde_json::Value>(&request.body)
                            .unwrap_or_default();
                        captured.lock().unwrap().push(request);

                        if path == "/api/oauth/usage" {
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::OK)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"five_hour":{"utilization":10},"seven_day":{"utilization":20},"extra_usage":{"is_enabled":false},"limits":[{"kind":"weekly_scoped","group":"fable_weekly","percent":30,"scope":{"model":{"display_name":"Fable"}}}]}"#,
                                    )))
                                    .unwrap(),
                            );
                        }

                        if path == "/v1/messages" && authorization == "Bearer token-stale" {
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::UNAUTHORIZED)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#,
                                    )))
                                    .unwrap(),
                            );
                        }

                        if path == "/v1/messages" && authorization == "Bearer token-invalid" {
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::UNAUTHORIZED)
                                    .header("content-type", "application/json")
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"type":"error","error":{"type":"authentication_error","message":"invalid"}}"#,
                                    )))
                                    .unwrap(),
                            );
                        }

                        if path == "/v1/messages"
                            && authorization == "Bearer token-a"
                            && body_json["model"]
                                .as_str()
                                .is_some_and(|model| model.contains("generic-limit"))
                        {
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::TOO_MANY_REQUESTS)
                                    .header("content-type", "application/json")
                                    .header("x-should-retry", "true")
                                    // A provider-capacity 429 may still carry a
                                    // partial claim snapshot. The overall
                                    // allowed status means this is not quota
                                    // evidence and must not poison pool state.
                                    .header("anthropic-ratelimit-unified-status", "allowed")
                                    .header(
                                        "anthropic-ratelimit-unified-representative-claim",
                                        "7d",
                                    )
                                    .header(
                                        "anthropic-ratelimit-unified-7d-status",
                                        "rejected",
                                    )
                                    .header(
                                        "anthropic-ratelimit-unified-reset",
                                        "2030-01-07T00:00:00Z",
                                    )
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"type":"error","error":{"type":"rate_limit_error","message":"transient"}}"#,
                                    )))
                                    .unwrap(),
                            );
                        }

                        if path == "/v1/messages"
                            && authorization == "Bearer token-a"
                            && body_json["model"]
                                .as_str()
                                .is_some_and(|model| model.contains("fable"))
                        {
                            return Ok::<_, Infallible>(
                                Response::builder()
                                    .status(StatusCode::TOO_MANY_REQUESTS)
                                    .header("content-type", "application/json")
                                    .header("anthropic-ratelimit-unified-status", "rejected")
                                    .header(
                                        "anthropic-ratelimit-unified-representative-claim",
                                        "7d_oi",
                                    )
                                    .header(
                                        "anthropic-ratelimit-unified-7d_oi-status",
                                        "rejected",
                                    )
                                    .header(
                                        "anthropic-ratelimit-unified-reset",
                                        "2030-01-07T00:00:00Z",
                                    )
                                    .body(Full::new(Bytes::from_static(
                                        br#"{"type":"error","error":{"type":"rate_limit_error","message":"weekly model limit reached"}}"#,
                                    )))
                                    .unwrap(),
                            );
                        }

                        Ok::<_, Infallible>(
                            Response::builder()
                                .status(StatusCode::OK)
                                .header("content-type", "application/json")
                                .header("anthropic-ratelimit-unified-status", "allowed")
                                .header("anthropic-ratelimit-unified-representative-claim", "5h")
                                .header("anthropic-ratelimit-unified-5h-utilization", "0.25")
                                .body(Full::new(Bytes::from(format!(
                                    r#"{{"ok":true,"account":"{}"}}"#,
                                    authorization.trim_start_matches("Bearer ")
                                ))))
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

fn proxied_client_with_launch(
    proxy_port: u16,
    ca_cert_path: &Path,
    launch_id: &str,
) -> reqwest::Client {
    let proxy_ca = std::fs::read(ca_cert_path).unwrap();
    reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::all(format!("http://rayline:{launch_id}@127.0.0.1:{proxy_port}"))
                .unwrap(),
        )
        .add_root_certificate(reqwest::Certificate::from_pem(&proxy_ca).unwrap())
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}

fn write_subscription_credential(config_dir: &Path, access_token: &str) {
    std::fs::create_dir_all(config_dir).unwrap();
    std::fs::write(
        config_dir.join(".credentials.json"),
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": format!("refresh-{access_token}"),
                "expiresAt": 4_000_000_000_000_i64,
                "scopes": ["user:inference", "user:profile"],
                "subscriptionType": "max"
            }
        }))
        .unwrap(),
    )
    .unwrap();
}

fn subscription_pool_config(
    control_dir: &Path,
    account_a: &Path,
    account_b: &Path,
) -> rayline_subscriptions::SubscriptionPoolConfig {
    rayline_subscriptions::SubscriptionPoolConfig {
        control_config_dir: control_dir.to_owned(),
        accounts: vec![
            rayline_subscriptions::SubscriptionAccountConfig {
                id: "a".to_owned(),
                credential_source: rayline_subscriptions::CredentialSourceConfig {
                    claude_config_dir: account_a.to_owned(),
                },
            },
            rayline_subscriptions::SubscriptionAccountConfig {
                id: "b".to_owned(),
                credential_source: rayline_subscriptions::CredentialSourceConfig {
                    claude_config_dir: account_b.to_owned(),
                },
            },
        ],
        policy: Default::default(),
    }
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
async fn subscription_pool_fails_over_only_the_exhausted_model_pool() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let account_a = temp.path().join("account-a");
    let account_b = temp.path().join("account-b");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&account_a, "token-a");
    write_subscription_credential(&account_b, "token-b");

    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        subscription_pool_config(&control_dir, &account_a, &account_b),
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime);
    opts.claude_config_dir = Some(control_dir);
    let session_status_dir = temp.path().join("session-status");
    opts.session_status_dir = Some(session_status_dir.clone());
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    // This stable launch id places the initial equal-capacity tie on account A,
    // whose fake response exhausts only the Fable claim.
    let client = proxied_client_with_launch(proxy_port, &ca_cert_path, "test_a_fable_2");
    let fable = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer control-profile-token")
        .header("x-api-key", "control-profile-key")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-fable-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(fable.status(), StatusCode::OK);
    assert_eq!(
        fable.headers().get("anthropic-ratelimit-unified-status"),
        None,
        "per-account unified headers must not leak into Claude Code's cache"
    );
    assert_eq!(
        fable.json::<serde_json::Value>().await.unwrap()["account"],
        "token-b"
    );
    let status_id = rayline_subscriptions::derive_status_id("test_a_fable_2");
    let status_path = session_status_dir.join(format!("{status_id}.json"));
    let status_raw = std::fs::read_to_string(&status_path).unwrap();
    assert!(!status_raw.contains("token-a"));
    assert!(!status_raw.contains("token-b"));
    let status: rayline_subscriptions::SessionStatusSnapshot =
        serde_json::from_str(&status_raw).unwrap();
    assert_eq!(status.assignment.primary_account_id, "a");
    assert_eq!(status.assignment.current_account_id, "b");
    assert_eq!(status.assignment.current_model_family, "fable");
    assert_eq!(
        status.assignment.kind,
        rayline_subscriptions::SessionAssignmentKind::ModelOverride
    );
    assert_eq!(
        status.assignment.reason,
        rayline_subscriptions::SessionAssignmentReason::QuotaFailover
    );
    assert_eq!(status.capacity.eligible_accounts, 1);
    assert_eq!(status.capacity.total_accounts, 2);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&session_status_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&status_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    let fable_again = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer control-profile-token")
        .body(r#"{"model":"claude-fable-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        fable_again.json::<serde_json::Value>().await.unwrap()["account"],
        "token-b",
        "the observed Fable rejection should keep Fable away from account a"
    );

    let sonnet = client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer control-profile-token")
        .body(r#"{"model":"claude-sonnet-4-6","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(
        sonnet.json::<serde_json::Value>().await.unwrap()["account"],
        "token-a",
        "a model-scoped Fable rejection must not exhaust Sonnet on that account"
    );

    let message_requests = anthropic
        .captured
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path_and_query == "/v1/messages")
        .cloned()
        .collect::<Vec<_>>();
    let authorizations = message_requests
        .iter()
        .map(|request| request.header("authorization").unwrap_or_default())
        .collect::<Vec<_>>();
    assert_eq!(
        authorizations,
        vec![
            "Bearer token-a",
            "Bearer token-b",
            "Bearer token-b",
            "Bearer token-a"
        ]
    );
    assert!(
        message_requests
            .iter()
            .all(|request| request.header("x-api-key").is_none())
    );
}

#[tokio::test]
async fn concurrent_new_launches_balance_across_equal_subscriptions() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let account_a = temp.path().join("account-a");
    let account_b = temp.path().join("account-b");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&account_a, "token-a");
    write_subscription_credential(&account_b, "token-b");
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        subscription_pool_config(&control_dir, &account_a, &account_b),
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime);
    opts.claude_config_dir = Some(control_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let requests = (0..20).map(|index| {
        let client = proxied_client_with_launch(
            proxy_port,
            &ca_cert_path,
            &format!("balanced_launch_{index}"),
        );
        async move {
            client
                .post("https://api.anthropic.com/v1/messages")
                .body(r#"{"model":"claude-sonnet-4-6","messages":[]}"#)
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap()["account"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    });
    let accounts = join_all(requests).await;
    let account_a_count = accounts
        .iter()
        .filter(|account| *account == "token-a")
        .count();
    let account_b_count = accounts
        .iter()
        .filter(|account| *account == "token-b")
        .count();
    assert_eq!(account_a_count + account_b_count, 20);
    assert!(
        account_a_count.abs_diff(account_b_count) <= 2,
        "active leases should keep equal subscriptions balanced"
    );
}

#[tokio::test]
async fn exhausted_subscription_pool_preserves_the_final_real_unified_rejection() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let account_a = temp.path().join("account-a");
    let account_b = temp.path().join("account-b");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&account_a, "token-a");
    write_subscription_credential(&account_b, "token-a");
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        subscription_pool_config(&control_dir, &account_a, &account_b),
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let runtime_for_assertion = Arc::clone(&runtime);

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime);
    opts.claude_config_dir = Some(control_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let response = proxied_client_with_launch(proxy_port, &ca_cert_path, "exhausted_launch")
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-fable-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-unified-status")
            .and_then(|value| value.to_str().ok()),
        Some("rejected")
    );
    assert_eq!(
        response
            .headers()
            .get("anthropic-ratelimit-unified-representative-claim")
            .and_then(|value| value.to_str().ok()),
        Some("7d_oi")
    );
    assert_eq!(
        anthropic
            .captured
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path_and_query == "/v1/messages")
            .count(),
        2
    );
    assert!(
        runtime_for_assertion
            .status()
            .accounts
            .iter()
            .all(|account| {
                account.claims.iter().any(|claim| {
                    claim.key == "seven_day_overage_included" && claim.is_hard_exhausted()
                })
            }),
        "a classified unified rejection must persist the exhausted model claim"
    );
}

#[tokio::test]
async fn generic_rate_limit_does_not_rotate_subscription_accounts() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let account_a = temp.path().join("account-a");
    let account_b = temp.path().join("account-b");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&account_a, "token-a");
    write_subscription_credential(&account_b, "token-b");
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        subscription_pool_config(&control_dir, &account_a, &account_b),
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let runtime_for_assertion = Arc::clone(&runtime);

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime);
    opts.claude_config_dir = Some(control_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    // Exercise the generic 429 response on account A; equal-capacity launches
    // are intentionally distributed by their stable launch hash.
    let response = proxied_client_with_launch(proxy_port, &ca_cert_path, "test_a_generic-limit_5")
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-generic-limit","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let message_requests = anthropic
        .captured
        .lock()
        .unwrap()
        .iter()
        .filter(|request| request.path_and_query == "/v1/messages")
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(message_requests.len(), 1);
    assert_eq!(
        message_requests[0].header("authorization").as_deref(),
        Some("Bearer token-a")
    );
    let account_a = runtime_for_assertion
        .status()
        .accounts
        .into_iter()
        .find(|account| account.id == "a")
        .unwrap();
    assert!(
        !account_a
            .claims
            .iter()
            .any(|claim| claim.is_hard_exhausted()),
        "a transient provider 429 must not exhaust an account's local allowance state"
    );
}

#[tokio::test]
async fn subscription_pool_refreshes_a_rejected_token_and_persists_rotation() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let oauth = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::OK,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Bytes::from_static(
                br#"{"access_token":"token-refreshed","refresh_token":"refresh-rotated","expires_in":3600,"refresh_token_expires_in":7200,"scope":"user:inference user:profile"}"#,
            ),
        },
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let account_dir = temp.path().join("account");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&account_dir, "token-stale");
    let pool = rayline_subscriptions::SubscriptionPoolConfig {
        control_config_dir: control_dir.clone(),
        accounts: vec![rayline_subscriptions::SubscriptionAccountConfig {
            id: "only".to_owned(),
            credential_source: rayline_subscriptions::CredentialSourceConfig {
                claude_config_dir: account_dir.clone(),
            },
        }],
        policy: Default::default(),
    };
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        pool,
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            token_url: format!("https://localhost:{}/oauth/token", oauth.port),
            trusted_ca_pem: Some(oauth.cert_pem.as_bytes().to_vec()),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime);
    opts.claude_config_dir = Some(control_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client_with_launch(proxy_port, &ca_cert_path, "refresh_launch");
    let request_one = client
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-sonnet-4-6","messages":[]}"#)
        .send();
    let request_two = client
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-sonnet-4-6","messages":[]}"#)
        .send();
    let (response_one, response_two) = tokio::join!(request_one, request_two);
    for response in [response_one.unwrap(), response_two.unwrap()] {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.json::<serde_json::Value>().await.unwrap()["account"],
            "token-refreshed"
        );
    }

    let credential: serde_json::Value =
        serde_json::from_slice(&std::fs::read(account_dir.join(".credentials.json")).unwrap())
            .unwrap();
    assert_eq!(
        credential["claudeAiOauth"]["accessToken"],
        "token-refreshed"
    );
    assert_eq!(
        credential["claudeAiOauth"]["refreshToken"],
        "refresh-rotated"
    );
    assert_eq!(
        oauth
            .captured
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path_and_query == "/oauth/token")
            .count(),
        1
    );
}

#[tokio::test]
async fn subscription_pool_recovers_when_another_process_rotates_the_credential() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let oauth = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Bytes::from_static(br#"{"error":"invalid_grant"}"#),
        },
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let account_dir = temp.path().join("account");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&account_dir, "token-stale");
    let pool = rayline_subscriptions::SubscriptionPoolConfig {
        control_config_dir: control_dir.clone(),
        accounts: vec![rayline_subscriptions::SubscriptionAccountConfig {
            id: "only".to_owned(),
            credential_source: rayline_subscriptions::CredentialSourceConfig {
                claude_config_dir: account_dir.clone(),
            },
        }],
        policy: Default::default(),
    };
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        pool,
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            token_url: format!("https://localhost:{}/oauth/token", oauth.port),
            trusted_ca_pem: Some(oauth.cert_pem.as_bytes().to_vec()),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    // Simulate a standalone Claude process rotating the profile credential
    // after the pool daemon cached its original token pair.
    write_subscription_credential(&account_dir, "token-current");

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime.clone());
    opts.claude_config_dir = Some(control_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let response = proxied_client_with_launch(proxy_port, &ca_cert_path, "rotated_elsewhere")
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-sonnet-4-6","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["account"],
        "token-current"
    );
    assert_eq!(
        runtime.status().accounts[0].credential_health,
        rayline_subscriptions::CredentialHealth::Healthy
    );
    assert_eq!(
        oauth
            .captured
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path_and_query == "/oauth/token")
            .count(),
        1
    );
}

#[tokio::test]
async fn subscription_pool_quarantines_invalid_grant_and_uses_next_account() {
    init_tracing();
    let anthropic = spawn_fake_subscription_anthropic().await;
    let oauth = spawn_fake_https_server(
        "localhost",
        FakeResponse {
            status: StatusCode::BAD_REQUEST,
            headers: vec![("content-type".to_owned(), "application/json".to_owned())],
            body: Bytes::from_static(br#"{"error":"invalid_grant"}"#),
        },
    )
    .await;
    let temp = tempfile::tempdir().unwrap();
    let control_dir = temp.path().join("control");
    let invalid_dir = temp.path().join("invalid");
    let healthy_dir = temp.path().join("healthy");
    std::fs::create_dir_all(&control_dir).unwrap();
    write_subscription_credential(&invalid_dir, "token-invalid");
    write_subscription_credential(&healthy_dir, "token-b");
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start(
        "default",
        subscription_pool_config(&control_dir, &invalid_dir, &healthy_dir),
        rayline_subscriptions::SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", anthropic.port),
            token_url: format!("https://localhost:{}/oauth/token", oauth.port),
            trusted_ca_pem: Some(oauth.cert_pem.as_bytes().to_vec()),
            request_timeout: Duration::from_secs(2),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    let proxy_port = free_port();
    let mut opts = proxy_options(
        proxy_port,
        temp.path(),
        "http://127.0.0.1:9".to_owned(),
        format!("http://127.0.0.1:{}", anthropic.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::SelectiveSubagents;
    opts.subscription_pool = Some(runtime.clone());
    opts.claude_config_dir = Some(control_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    // Account A owns the invalid credential in this fixture.
    let response = proxied_client_with_launch(proxy_port, &ca_cert_path, "test_a_sonnet_0")
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-sonnet-4-6","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.json::<serde_json::Value>().await.unwrap()["account"],
        "token-b"
    );
    let invalid = runtime
        .status()
        .accounts
        .into_iter()
        .find(|account| account.id == "a")
        .unwrap();
    assert_eq!(
        invalid.credential_health,
        rayline_subscriptions::CredentialHealth::Quarantined
    );
    runtime.refresh_all_usage().await;
    assert_eq!(
        oauth
            .captured
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.path_and_query == "/oauth/token")
            .count(),
        1,
        "a quarantined credential must not trigger repeated refresh attempts"
    );

    runtime
        .mark_credential_unavailable("b", "synthetic credential failure")
        .unwrap();
    let unavailable = proxied_client_with_launch(proxy_port, &ca_cert_path, "no_credentials")
        .post("https://api.anthropic.com/v1/messages")
        .body(r#"{"model":"claude-fable-5","messages":[]}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    let unavailable_body = unavailable.json::<serde_json::Value>().await.unwrap();
    assert!(
        unavailable_body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no usable OAuth credential"))
    );
    assert!(
        !unavailable_body["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("allowance"))
    );
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
    assert_eq!(router_body["model"], "claude-sonnet-4-5");
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
    let session_status_dir = status_dir.path().join("session-status");
    std::fs::create_dir_all(&session_status_dir).unwrap();
    let launch_id = "route_status_launch";
    let session_status_path = session_status_dir.join(format!(
        "{}.json",
        rayline_subscriptions::derive_status_id(launch_id)
    ));
    std::fs::write(
        &session_status_path,
        serde_json::to_vec(&serde_json::json!({
            "schema": 1,
            "pool_id": "default",
            "assignment": {
                "primary_account_id": "a",
                "current_account_id": "a",
                "current_model_family": "sonnet",
                "kind": "primary",
                "reason": "balanced_new_launch",
                "assigned_at_unix": 100,
                "last_seen_at_unix": 100
            },
            "capacity": {
                "usage_snapshot_fresh": true,
                "effective_headroom": 0.8,
                "bottleneck": null,
                "applicable": []
            },
            "placement": {
                "strategy": "balanced_sessions",
                "score": 0.8,
                "active_global_leases": 1,
                "active_model_leases": 1
            },
            "route": null,
            "updated_at_unix": 100
        }))
        .unwrap(),
    )
    .unwrap();
    let mut opts = proxy_options(
        proxy_port,
        ca_dir.path(),
        format!("https://127.0.0.1:{}", router.port),
        format!("https://127.0.0.1:{}", anthropic.port),
        &[&router, &anthropic],
    );
    opts.route_status_path = Some(status_path.clone());
    opts.session_status_dir = Some(session_status_dir);
    let ca_cert_path = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;

    let client = proxied_client_with_launch(proxy_port, &ca_cert_path, launch_id);
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

    let mut launch_status = None;
    for _ in 0..50 {
        if let Ok(raw) = std::fs::read_to_string(&session_status_path) {
            let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
            if !parsed["route"].is_null() {
                launch_status = Some(parsed);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let launch_status = launch_status.expect("launch-scoped route should be merged");
    assert_eq!(launch_status["assignment"]["current_account_id"], "a");
    assert_eq!(launch_status["route"]["selected_model"], "glm-4.6");
    assert_eq!(launch_status["route"]["route_id"], "route-it");
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
