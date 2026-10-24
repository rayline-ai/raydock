use super::*;
use http_body_util::StreamBody;
use hyper::body::Frame;
use std::io::Write as _;

type TestBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

#[derive(Clone)]
enum Delivery {
    Complete,
    Incomplete,
    Stall,
    Network,
}

async fn reliability_server(responses: Vec<FakeResponse>, delivery: Delivery) -> FakeHttpServer {
    reliability_server_with_auth(responses, delivery, false).await
}

async fn reliability_server_with_auth(
    responses: Vec<FakeResponse>,
    delivery: Delivery,
    usage_auth: bool,
) -> FakeHttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured: CapturedRequests = Arc::new(Mutex::new(Vec::new()));
    let log = captured.clone();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let log = log.clone();
            let responses = responses.clone();
            let delivery = delivery.clone();
            tokio::spawn(async move {
                let service = service_fn(move |req: Request<Incoming>| {
                    let log = log.clone();
                    let responses = responses.clone();
                    let delivery = delivery.clone();
                    async move {
                        let request = capture_request(req).await;
                        let messages = request.path_and_query == "/v1/messages";
                        let rejected_refresh = usage_auth
                            && request.header("authorization").as_deref()
                                == Some("Bearer token-refreshed");
                        let index = {
                            let mut log = log.lock().unwrap();
                            let index = log
                                .iter()
                                .filter(|r| r.path_and_query == "/v1/messages")
                                .count();
                            log.push(request);
                            index
                        };
                        if messages && matches!(delivery, Delivery::Network) {
                            return Err(std::io::Error::other("connection lost before response"));
                        }
                        let response = if messages {
                            responses[index.min(responses.len() - 1)].clone()
                        } else if rejected_refresh {
                            FakeResponse { status: StatusCode::UNAUTHORIZED, headers: vec![], body: Bytes::from_static(br#"{"type":"error","error":{"type":"authentication_error","message":"invalid"}}"#) }
                        } else {
                            FakeResponse { status: StatusCode::OK, headers: vec![], body: Bytes::from_static(br#"{"five_hour":{"utilization":10},"seven_day":{"utilization":20},"extra_usage":{"is_enabled":false}}"#) }
                        };
                        let body: TestBody = if messages {
                            match delivery {
                                Delivery::Complete | Delivery::Network => Full::new(response.body)
                                    .map_err(|never| match never {})
                                    .boxed(),
                                Delivery::Incomplete => StreamBody::new(futures::StreamExt::chain(
                                    futures::stream::iter(vec![Ok(Frame::data(response.body))]),
                                    futures::stream::once(async {
                                        tokio::time::sleep(Duration::from_millis(40)).await;
                                        Err(std::io::Error::other("incomplete upstream"))
                                    }),
                                ))
                                .boxed(),
                                Delivery::Stall => StreamBody::new(futures::StreamExt::chain(
                                    futures::stream::iter(vec![Ok::<_, std::io::Error>(
                                        Frame::data(response.body),
                                    )]),
                                    futures::stream::pending(),
                                ))
                                .boxed(),
                            }
                        } else {
                            Full::new(response.body)
                                .map_err(|never| match never {})
                                .boxed()
                        };
                        let mut builder = Response::builder().status(response.status);
                        for (name, value) in response.headers {
                            builder = builder.header(name, value);
                        }
                        Ok::<_, std::io::Error>(builder.body(body).unwrap())
                    }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    FakeHttpServer { port, captured }
}

fn quota(body: Bytes) -> FakeResponse {
    FakeResponse {
        status: StatusCode::TOO_MANY_REQUESTS,
        headers: vec![
            (
                "anthropic-ratelimit-unified-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-representative-claim".into(),
                "5h".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-status".into(),
                "rejected".into(),
            ),
            (
                "anthropic-ratelimit-unified-5h-utilization".into(),
                "1".into(),
            ),
        ],
        body,
    }
}

fn quota_json() -> Bytes {
    Bytes::from_static(
        br#"{"type":"error","error":{"type":"rate_limit_error","message":"usage limit reached"}}"#,
    )
}

fn compressed(bytes: &[u8], encoding: &str) -> Bytes {
    match encoding {
        "gzip" => {
            let mut writer =
                flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            writer.write_all(bytes).unwrap();
            Bytes::from(writer.finish().unwrap())
        }
        "deflate" => {
            let mut writer =
                flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
            writer.write_all(bytes).unwrap();
            Bytes::from(writer.finish().unwrap())
        }
        "br" => {
            let mut output = Vec::new();
            {
                let mut writer = brotli::CompressorWriter::new(&mut output, 4096, 5, 22);
                writer.write_all(bytes).unwrap();
            }
            Bytes::from(output)
        }
        "zstd" => Bytes::from(zstd::stream::encode_all(bytes, 1).unwrap()),
        _ => unreachable!(),
    }
}

struct ReliabilityHarness {
    _temp: tempfile::TempDir,
    client: reqwest::Client,
    status_path: std::path::PathBuf,
    runtime: Arc<rayline_subscriptions::SubscriptionPoolRuntime>,
}

async fn harness(server: &FakeHttpServer, oauth: Option<&FakeHttpsServer>) -> ReliabilityHarness {
    harness_with_storage_failure(server, oauth, false).await
}

async fn harness_with_storage_failure(
    server: &FakeHttpServer,
    oauth: Option<&FakeHttpsServer>,
    fail_storage: bool,
) -> ReliabilityHarness {
    let temp = tempfile::tempdir().unwrap();
    let control = temp.path().join("control");
    std::fs::create_dir_all(&control).unwrap();
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    write_subscription_credential(&a, "token-a");
    write_subscription_credential(&b, "token-b");
    let mut options = rayline_subscriptions::SubscriptionRuntimeOptions {
        anthropic_base_url: format!("http://127.0.0.1:{}", server.port),
        request_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    if let Some(oauth) = oauth {
        options.token_url = format!("https://localhost:{}/oauth/token", oauth.port);
        options.trusted_ca_pem = Some(oauth.cert_pem.as_bytes().to_vec());
    }
    let state_dir = temp.path().canonicalize().unwrap().join("routing-state");
    let mut config = subscription_pool_config(&control, &a, &b);
    if fail_storage {
        config.accounts.truncate(1);
        options.state_dir = Some(state_dir.clone());
    }
    let runtime = rayline_subscriptions::SubscriptionPoolRuntime::start("default", config, options)
        .await
        .unwrap();
    runtime.refresh_all_usage().await;
    #[cfg(unix)]
    if fail_storage {
        use std::os::unix::fs::PermissionsExt as _;
        // Break private-directory validation after startup; a retained directory
        // descriptor means merely replacing its path would not inject a failure.
        std::fs::set_permissions(&state_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let port = free_port();
    let mut opts = proxy_options(
        port,
        temp.path(),
        "http://127.0.0.1:9".into(),
        format!("http://127.0.0.1:{}", server.port),
        &[],
    );
    opts.routing_mode = rayline_proxy::ProxyRoutingMode::Passthrough;
    opts.subscription_pool = Some(runtime.clone());
    opts.claude_config_dir = Some(control);
    let status_dir = temp.path().join("status");
    opts.session_status_dir = Some(status_dir.clone());
    let ca = opts.ca_cert_path.clone();
    spawn_proxy(opts).await;
    ReliabilityHarness {
        client: proxied_client_with_launch(port, &ca, "reliability"),
        status_path: status_dir.join(format!(
            "{}.json",
            rayline_subscriptions::derive_status_id("reliability")
        )),
        runtime,
        _temp: temp,
    }
}

async fn send(harness: &ReliabilityHarness) -> reqwest::Response {
    harness
        .client
        .post("https://api.anthropic.com/v1/messages")
        .header("authorization", "Bearer control-token")
        .header("x-api-key", "control-key")
        .header("anthropic-version", "2023-06-01")
        .body(r#"{"model":"claude-sonnet-4-5","messages":[]}"#)
        .send()
        .await
        .unwrap()
}

fn message_requests(server: &FakeHttpServer) -> Vec<CapturedRequest> {
    server
        .captured
        .lock()
        .unwrap()
        .iter()
        .filter(|r| r.path_and_query == "/v1/messages")
        .cloned()
        .collect()
}

#[tokio::test]
async fn compressed_quota_classification_rotates_and_preserves_original_error_bytes() {
    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let encoded = compressed(&quota_json(), encoding);
        let mut response = quota(encoded.clone());
        response
            .headers
            .push(("content-encoding".into(), encoding.into()));
        let server = reliability_server(vec![response], Delivery::Complete).await;
        let harness = harness(&server, None).await;
        let response = send(&harness).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()["content-encoding"], encoding);
        assert_eq!(response.bytes().await.unwrap(), encoded);
        let requests = message_requests(&server);
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body, requests[1].body,
            "only authoritative rejections permit identical replay"
        );
        assert!(
            !harness.status_path.exists(),
            "rejected accounts are not serving"
        );
    }
}

#[tokio::test]
async fn unsafe_error_bodies_preserve_status_without_replay() {
    let mut invalid_gzip = quota(Bytes::from_static(b"invalid gzip"));
    invalid_gzip
        .headers
        .push(("content-encoding".into(), "gzip".into()));
    let cases = vec![
        (quota(Bytes::from_static(b"not json")), Delivery::Complete),
        (invalid_gzip, Delivery::Complete),
        (
            quota(Bytes::from(vec![b'x'; 2 * 1024 * 1024 + 1])),
            Delivery::Complete,
        ),
        (quota(quota_json()), Delivery::Incomplete),
        (quota(quota_json()), Delivery::Stall),
    ];
    for (response, delivery) in cases {
        let must_fail_body = matches!(&delivery, Delivery::Incomplete | Delivery::Stall);
        let original = response.body.clone();
        let server = reliability_server(vec![response], delivery).await;
        let harness = harness(&server, None).await;
        let response = tokio::time::timeout(Duration::from_secs(18), send(&harness))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body = response.bytes().await;
        if must_fail_body {
            assert!(
                body.is_err(),
                "truncated/stalled original response must not advertise normal EOF"
            );
        } else {
            assert_eq!(
                body.unwrap(),
                original,
                "complete oversized/malformed response forwards every original byte"
            );
        }
        assert_eq!(message_requests(&server).len(), 1);
        assert!(!harness.status_path.exists());
    }
    for encoding in ["gzip", "deflate", "br", "zstd"] {
        let mut response = quota(compressed(&vec![b'x'; 2 * 1024 * 1024 + 1], encoding));
        response
            .headers
            .push(("content-encoding".into(), encoding.into()));
        let original = response.body.clone();
        let server = reliability_server(vec![response], Delivery::Complete).await;
        let harness = harness(&server, None).await;
        let response = send(&harness).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.bytes().await.unwrap(), original);
        assert_eq!(message_requests(&server).len(), 1);
    }
}

#[tokio::test]
async fn overload_unknown_forbidden_server_errors_and_redirects_never_rotate() {
    for status in [429, 529, 403, 500, 307] {
        let server = reliability_server(
            vec![FakeResponse {
                status: StatusCode::from_u16(status).unwrap(),
                headers: vec![(
                    "location".into(),
                    "http://127.0.0.1:9/should-not-follow".into(),
                )],
                body: Bytes::from_static(
                    br#"{"type":"error","error":{"type":"unknown","message":"try later"}}"#,
                ),
            }],
            Delivery::Complete,
        )
        .await;
        let harness = harness(&server, None).await;
        let response = send(&harness).await;
        assert_eq!(response.status().as_u16(), status);
        let _ = response.bytes().await;
        let requests = message_requests(&server);
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].header("host").unwrap(),
            format!("127.0.0.1:{}", server.port)
        );
        assert!(requests[0].header("x-api-key").is_none());
        assert_ne!(
            requests[0].header("authorization").unwrap(),
            "Bearer control-token"
        );
        assert!(!harness.status_path.exists());
    }
}

#[tokio::test]
async fn accepted_success_is_not_replayed_when_body_fails_and_publishes_serving_account() {
    let server = reliability_server(
        vec![FakeResponse {
            status: StatusCode::OK,
            headers: vec![],
            body: Bytes::from_static(b"partial"),
        }],
        Delivery::Stall,
    )
    .await;
    let harness = harness(&server, None).await;
    let response = send(&harness).await;
    assert_eq!(response.status(), StatusCode::OK);
    drop(response);
    assert_eq!(message_requests(&server).len(), 1);
    assert!(harness.status_path.exists());
}

#[tokio::test]
async fn mixed_quota_and_invalid_grant_returns_quota_not_auth() {
    for quota_first in [true, false] {
        let oauth = spawn_fake_https_server(
            "localhost",
            FakeResponse {
                status: StatusCode::BAD_REQUEST,
                headers: vec![],
                body: Bytes::from_static(br#"{"error":"invalid_grant"}"#),
            },
        )
        .await;
        let auth = FakeResponse {
            status: StatusCode::UNAUTHORIZED,
            headers: vec![],
            body: Bytes::from_static(
                br#"{"type":"error","error":{"type":"authentication_error","message":"expired"}}"#,
            ),
        };
        let responses = if quota_first {
            vec![quota(quota_json()), auth]
        } else {
            vec![auth, quota(quota_json())]
        };
        let server = reliability_server(responses, Delivery::Complete).await;
        let harness = harness(&server, Some(&oauth)).await;
        let response = send(&harness).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.bytes().await.unwrap(), quota_json());
        assert_eq!(message_requests(&server).len(), 2);
    }
}

#[tokio::test]
async fn repeated_401_requires_account_specific_corroboration_and_attempts_are_bounded() {
    for corroborated in [false, true] {
        let oauth = spawn_fake_https_server("localhost", FakeResponse {
            status: StatusCode::OK, headers: vec![],
            body: Bytes::from_static(br#"{"access_token":"token-refreshed","refresh_token":"refresh-new","expires_in":3600,"token_type":"Bearer"}"#),
        }).await;
        let auth = FakeResponse {
            status: StatusCode::UNAUTHORIZED,
            headers: vec![],
            body: Bytes::from_static(
                br#"{"type":"error","error":{"type":"authentication_error","message":"invalid"}}"#,
            ),
        };
        let server =
            reliability_server_with_auth(vec![auth], Delivery::Complete, corroborated).await;
        let harness = harness(&server, Some(&oauth)).await;
        let response = send(&harness).await;
        assert_eq!(
            response.status(),
            if corroborated {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::UNAUTHORIZED
            }
        );
        assert_eq!(
            message_requests(&server).len(),
            if corroborated { 4 } else { 2 }
        );
        assert_eq!(
            oauth.captured.lock().unwrap().len(),
            if corroborated { 2 } else { 1 }
        );
        assert!(!harness.status_path.exists());
    }
}

#[tokio::test]
async fn final_quota_precedes_entitlement_in_either_attempt_order() {
    for quota_first in [true, false] {
        let entitlement = FakeResponse { status: StatusCode::FORBIDDEN, headers: vec![], body: Bytes::from_static(br#"{"type":"error","error":{"type":"permission_error","details":{"error_code":"credits_required"}}}"#) };
        let responses = if quota_first {
            vec![quota(quota_json()), entitlement]
        } else {
            vec![entitlement, quota(quota_json())]
        };
        let server = reliability_server(responses, Delivery::Complete).await;
        let harness = harness(&server, None).await;
        let response = send(&harness).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.bytes().await.unwrap(), quota_json());
        assert_eq!(message_requests(&server).len(), 2);
    }
}

#[tokio::test]
async fn ambiguous_network_failure_never_replays_on_another_account() {
    let server = reliability_server(
        vec![FakeResponse {
            status: StatusCode::OK,
            headers: vec![],
            body: Bytes::new(),
        }],
        Delivery::Network,
    )
    .await;
    let harness = harness(&server, None).await;
    let response = send(&harness).await;
    assert!(response.status().is_server_error());
    assert_eq!(message_requests(&server).len(), 1);
    assert!(!harness.status_path.exists());
}

#[tokio::test]
async fn error_body_deadline_before_any_bytes_keeps_original_status_with_diagnostic() {
    let server = reliability_server(vec![quota(Bytes::new())], Delivery::Stall).await;
    let harness = harness(&server, None).await;
    let response = tokio::time::timeout(Duration::from_secs(18), send(&harness))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    let body = response.json::<serde_json::Value>().await.unwrap();
    assert_eq!(body["error"]["type"], "proxy_error");
    assert_eq!(body["error"]["message"], "read deadline exceeded");
    assert_eq!(message_requests(&server).len(), 1);
    assert!(!harness.status_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn persistence_failure_never_replaces_accepted_stream_or_real_quota_rejection() {
    let accepted_body = Bytes::from_static(b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":1}}\n\n");
    let cases = [
        FakeResponse { status: StatusCode::OK, headers: vec![("content-type".into(), "text/event-stream".into()), ("anthropic-ratelimit-unified-5h-status".into(), "rejected".into())], body: accepted_body },
        quota(quota_json()),
        FakeResponse { status: StatusCode::FORBIDDEN, headers: vec![], body: Bytes::from_static(br#"{"type":"error","error":{"type":"permission_error","details":{"error_code":"credits_required"}}}"#) },
    ];
    for upstream in cases {
        let status = upstream.status;
        let original = upstream.body.clone();
        let server = reliability_server(vec![upstream], Delivery::Complete).await;
        let harness = harness_with_storage_failure(&server, None, true).await;
        let response = send(&harness).await;
        assert_eq!(
            response.status(),
            status,
            "local persistence failure must not replace upstream status"
        );
        assert_eq!(response.bytes().await.unwrap(), original);
        assert_eq!(message_requests(&server).len(), 1);
        assert_eq!(harness.status_path.exists(), status.is_success());
        assert!(
            harness.runtime.status().accounts[0].last_error.is_some(),
            "persistence failure remains available in account diagnostics"
        );
    }
}
