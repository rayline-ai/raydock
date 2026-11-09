use std::net::SocketAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use csub::gateway::Gateway;
use http_body_util::{BodyExt as _, Full};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use rayline_subscriptions::{
    CredentialSourceConfig, SubscriptionAccountConfig, SubscriptionPoolConfig,
    SubscriptionPoolRuntime, SubscriptionRuntimeOptions,
};
use tokio::net::TcpListener;

const USAGE: &str = r#"{"five_hour":{"utilization":10},"seven_day":{"utilization":20},"extra_usage":{"is_enabled":false}}"#;
const SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\"}\n\n";

#[derive(Default)]
struct Seen {
    messages: Vec<String>,
    passthrough: Vec<String>,
}

/// Fake Anthropic: account `a` is out of weekly quota, account `b` answers.
async fn fake_anthropic(seen: Arc<Mutex<Seen>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let seen = seen.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request: Request<hyper::body::Incoming>| {
                    let seen = seen.clone();
                    async move {
                        let bearer = request
                            .headers()
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .unwrap_or("")
                            .to_owned();
                        let path = request.uri().path().to_owned();
                        let response = if path == "/api/oauth/usage" {
                            Response::builder()
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(USAGE)))
                        } else if path == "/v1/messages" {
                            seen.lock().expect("seen").messages.push(bearer.clone());
                            if bearer == "Bearer token-a" {
                                Response::builder()
                                    .status(429)
                                    .header("content-type", "application/json")
                                    .header("anthropic-ratelimit-unified-status", "rejected")
                                    .header("anthropic-ratelimit-unified-7d-status", "rejected")
                                    .header("anthropic-ratelimit-unified-7d-utilization", "1.0")
                                    .header(
                                        "anthropic-ratelimit-unified-representative-claim",
                                        "seven_day",
                                    )
                                    .header("anthropic-ratelimit-unified-reset", "1893459600")
                                    .header("anthropic-ratelimit-unified-overage-status", "rejected")
                                    .body(Full::new(Bytes::from(
                                        r#"{"type":"error","error":{"type":"rate_limit_error","message":"limit"}}"#,
                                    )))
                            } else {
                                Response::builder()
                                    .header("content-type", "text/event-stream")
                                    .body(Full::new(Bytes::from(SSE)))
                            }
                        } else {
                            seen.lock().expect("seen").passthrough.push(bearer);
                            Response::builder().body(Full::new(Bytes::from("ok")))
                        };
                        Ok::<_, std::convert::Infallible>(response.expect("response"))
                    }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

fn profile(path: &Path, token: &str) {
    std::fs::create_dir_all(path).expect("profile");
    std::fs::write(
        path.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{token}","refreshToken":"refresh-{token}","expiresAt":4000000000000,"scopes":["user:inference","user:profile"]}}}}"#
        ),
    )
    .expect("credentials");
}

async fn gateway_server(upstream: SocketAddr, root: &Path) -> SocketAddr {
    let root = std::fs::canonicalize(root).expect("root");
    std::fs::create_dir_all(root.join("control")).expect("control");
    profile(&root.join("a"), "token-a");
    profile(&root.join("b"), "token-b");
    let account = |id: &str| SubscriptionAccountConfig {
        id: id.to_owned(),
        credential_source: CredentialSourceConfig {
            claude_config_dir: root.join(id),
        },
    };
    let base = format!("http://{upstream}");
    let runtime = SubscriptionPoolRuntime::start(
        "default",
        SubscriptionPoolConfig {
            control_config_dir: root.join("control"),
            accounts: vec![account("a"), account("b")],
            policy: Default::default(),
        },
        SubscriptionRuntimeOptions {
            anthropic_base_url: base.clone(),
            home_dir: None,
            ..Default::default()
        },
    )
    .await
    .expect("runtime");
    runtime.refresh_all_usage().await;
    let gateway = Arc::new(Gateway::new(runtime, base).expect("gateway"));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let address = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let gateway = gateway.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let gateway = gateway.clone();
                    async move { Ok::<_, std::convert::Infallible>(gateway.handle(request).await) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    address
}

#[tokio::test]
async fn swaps_the_bearer_and_fails_over_a_quota_rejection() {
    let temp = tempfile::tempdir().expect("temp");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let upstream = fake_anthropic(seen.clone()).await;
    let gateway = gateway_server(upstream, temp.path()).await;
    let client = reqwest::Client::new();

    let mut bodies = Vec::new();
    for _ in 0..3 {
        let response = client
            .post(format!("http://{gateway}/l/launch-1/v1/messages?beta=true"))
            .header("authorization", "Bearer client-control-token")
            .header("anthropic-beta", "oauth-2025-04-20")
            .json(&serde_json::json!({"model": "claude-sonnet-5-5", "messages": []}))
            .send()
            .await
            .expect("gateway response");
        assert_eq!(response.status(), 200);
        bodies.push(response.text().await.expect("body"));
    }
    assert!(bodies.iter().all(|body| body == SSE));

    let messages = seen.lock().expect("seen").messages.clone();
    assert!(
        !messages
            .iter()
            .any(|bearer| bearer.contains("client-control-token")),
        "the client's own token must never reach a pooled request"
    );
    // The first request tries a, fails over to b within the same client
    // request, and later requests stay on b because a's quota is now known.
    assert_eq!(
        messages,
        [
            "Bearer token-a",
            "Bearer token-b",
            "Bearer token-b",
            "Bearer token-b"
        ]
    );
}

#[tokio::test]
async fn forwards_other_paths_with_the_clients_credentials() {
    let temp = tempfile::tempdir().expect("temp");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let upstream = fake_anthropic(seen.clone()).await;
    let gateway = gateway_server(upstream, temp.path()).await;

    let response = reqwest::Client::new()
        .post(format!(
            "http://{gateway}/l/launch-1/v1/messages/count_tokens"
        ))
        .header("authorization", "Bearer client-control-token")
        .body("{}")
        .send()
        .await
        .expect("gateway response");
    assert_eq!(response.status(), 200);
    assert_eq!(
        seen.lock().expect("seen").passthrough,
        vec!["Bearer client-control-token".to_owned()]
    );
}

#[tokio::test]
async fn rejects_a_malformed_launch_id() {
    let temp = tempfile::tempdir().expect("temp");
    let upstream = fake_anthropic(Arc::default()).await;
    let gateway = gateway_server(upstream, temp.path()).await;
    let response = reqwest::get(format!("http://{gateway}/l/bad%20id/v1/messages"))
        .await
        .expect("gateway response");
    assert_eq!(response.status(), 400);
    let body = response.bytes().await.expect("body");
    let _ = Full::new(body).collect().await;
}

#[tokio::test]
async fn refuses_requests_that_could_escape_pool_routing() {
    let temp = tempfile::tempdir().expect("temp");
    let seen = Arc::new(Mutex::new(Seen::default()));
    let upstream = fake_anthropic(seen.clone()).await;
    let gateway = gateway_server(upstream, temp.path()).await;
    let client = reqwest::Client::new();

    for (path, body) in [
        ("/l/launch-1/v1/messages", "not json"),
        ("/l/launch-1/v1/messages", r#"{"messages":[]}"#),
        (
            "/l/launch-1/v1/%6dessages",
            r#"{"model":"claude-sonnet-5-5"}"#,
        ),
    ] {
        let response = client
            .post(format!("http://{gateway}{path}"))
            .header("authorization", "Bearer client-control-token")
            .body(body)
            .send()
            .await
            .expect("gateway response");
        assert_eq!(response.status(), 400, "{path} {body}");
    }
    // HTTP clients normalize these paths, so send them raw.
    for path in ["/l/launch-1/v1/x/../messages", "/l/launch-1/v1\\messages"] {
        let mut stream = tokio::net::TcpStream::connect(gateway)
            .await
            .expect("connect");
        let body = r#"{"model":"claude-sonnet-5-5"}"#;
        let request = format!(
            "POST {path} HTTP/1.1\r\nhost: 127.0.0.1\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
            .await
            .expect("write");
        let mut response = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut stream, &mut response)
            .await
            .expect("read");
        assert!(response.starts_with("HTTP/1.1 400"), "{path}: {response}");
    }
    let seen = seen.lock().expect("seen");
    assert!(seen.messages.is_empty() && seen.passthrough.is_empty());
}
