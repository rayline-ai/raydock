//! The router over real loopback sockets, with a fake policy and a fake upstream.

mod common;

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use common::{answer, catalog, request};
use futures::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_arc_routes::{DecideFuture, Decider, HttpDecider, Refusal, Router, RouterConfig};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};

type TestBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Infallible>;

/// A policy that answers the fixture, counting what runs at once.
#[derive(Default)]
struct FakeDecider {
    delay: Duration,
    /// The refusal a strict decide gets, when one is scripted.
    strict_refusal: Option<Refusal>,
    bodies: Mutex<Vec<Value>>,
    running: Mutex<HashMap<String, usize>>,
    most_per_episode: AtomicUsize,
    most_at_once: AtomicUsize,
}

impl Decider for FakeDecider {
    fn decide<'a>(&'a self, body: &'a Value) -> DecideFuture<'a> {
        Box::pin(async move {
            self.bodies.lock().unwrap().push(body.clone());
            let episode = body["episode_id_hash"].as_str().unwrap().to_owned();
            {
                let mut running = self.running.lock().unwrap();
                *running.entry(episode.clone()).or_default() += 1;
                self.most_per_episode
                    .fetch_max(running[&episode], Ordering::SeqCst);
                self.most_at_once
                    .fetch_max(running.values().sum(), Ordering::SeqCst);
            }
            tokio::time::sleep(self.delay).await;
            *self.running.lock().unwrap().get_mut(&episode).unwrap() -= 1;
            match &self.strict_refusal {
                Some(refusal) if body["episode_mode"] == "strict" => Err(refusal.clone()),
                _ => Ok(answer()),
            }
        })
    }
}

/// What a fake upstream saw of one request.
#[derive(Debug)]
struct Seen {
    method: String,
    target: String,
    authorization: Option<String>,
    host: Option<String>,
    body: Bytes,
}

async fn serve_with<F, Fut>(handler: F) -> SocketAddr
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response<TestBody>> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (socket, _) = listener.accept().await.unwrap();
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let handler = handler.clone();
                    async move { Ok::<_, Infallible>(handler(request).await) }
                });
                let _ = auto::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(socket), service)
                    .await;
            });
        }
    });
    address
}

/// An upstream that reports each request and answers in two chunks, the
/// second only once `release` is notified.
async fn upstream(seen: mpsc::UnboundedSender<Seen>, release: Arc<Notify>) -> SocketAddr {
    serve_with(move |request: Request<Incoming>| {
        let seen = seen.clone();
        let release = release.clone();
        async move {
            let header = |name: &str| {
                request
                    .headers()
                    .get(name)
                    .map(|value| value.to_str().unwrap().to_owned())
            };
            let (authorization, host) = (header("authorization"), header("host"));
            let method = request.method().to_string();
            let target = request.uri().to_string();
            let body = request.into_body().collect().await.unwrap().to_bytes();
            seen.send(Seen {
                method,
                target,
                authorization,
                host,
                body,
            })
            .unwrap();
            let chunks =
                futures::stream::iter(["first;"]).chain(futures::stream::once(async move {
                    release.notified().await;
                    "second"
                }));
            let frames = chunks.map(|chunk| Ok::<_, Infallible>(Frame::data(Bytes::from(chunk))));
            Response::builder()
                .status(201)
                .header("x-upstream", "yes")
                .body(BodyExt::boxed_unsync(StreamBody::new(frames)))
                .unwrap()
        }
    })
    .await
}

struct Harness {
    base: String,
    decider: Arc<FakeDecider>,
    seen: mpsc::UnboundedReceiver<Seen>,
    release: Arc<Notify>,
    client: reqwest::Client,
}

async fn harness(decider: FakeDecider) -> Harness {
    harness_with(decider, catalog(), |_| {}).await
}

async fn harness_with(
    decider: FakeDecider,
    catalog: rayline_arc_routes::Catalog,
    configure: impl FnOnce(&mut RouterConfig),
) -> Harness {
    let (sender, seen) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let upstream = upstream(sender, release.clone()).await;
    let decider = Arc::new(decider);
    let mut config = RouterConfig::new(catalog, &format!("http://{upstream}"));
    configure(&mut config);
    let router = Arc::new(Router::new(config, decider.clone()).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(router.serve(listener));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    Harness {
        base,
        decider,
        seen,
        release,
        client,
    }
}

impl Harness {
    fn routes(&self, conversation: &str) -> reqwest::RequestBuilder {
        self.client
            .post(format!("{}/v1/routes", self.base))
            .header("authorization", "Bearer test-token")
            .header("x-conversation-id", conversation)
            .header("x-rayline-branch-id", "branch")
            .header("x-rayline-wire-format", "anthropic.messages.v1")
            .json(&request())
    }
}

#[tokio::test]
async fn routes_is_answered_here_with_its_route_id_and_cell() {
    let mut harness = harness(FakeDecider::default()).await;
    let response = harness
        .routes("conversation")
        .header("x-rayline-cell", "cell-7")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["x-rayline-cell"], "cell-7");
    let route_id = response.headers()["x-rayline-route-id"]
        .to_str()
        .unwrap()
        .to_owned();
    let body: Value = response.json().await.unwrap();
    assert!(route_id.starts_with("rt_") && route_id.len() == 35);
    assert_eq!(body["route_id"], json!(route_id));
    assert_eq!(body["model"], "anthropic/claude-haiku-5.5");
    assert_eq!(body["routing"]["execution"]["status"], "applied");

    // The policy was handed the harness body's own members, and nothing went upstream.
    let bodies = harness.decider.bodies.lock().unwrap();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["request"]["messages"], request()["messages"]);
    assert_eq!(bodies[0]["episode_mode"], "strict");
    assert!(harness.seen.try_recv().is_err());
}

#[tokio::test]
async fn a_refusal_is_the_error_envelope_with_the_cell_and_no_route_id() {
    let harness = harness(FakeDecider::default()).await;
    let response = harness
        .client
        .post(format!("{}/v1/routes", harness.base))
        .header("x-rayline-cell", "cell-7")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    assert_eq!(response.headers()["x-rayline-cell"], "cell-7");
    assert!(response.headers().get("x-rayline-route-id").is_none());
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "invalid_body");

    let mut invalid = request();
    invalid["routing"]["policy"] = json!({"preset": "nope"});
    let response = harness.routes("c").json(&invalid).send().await.unwrap();
    assert_eq!(response.status(), 400);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "policy_invalid");
    assert!(harness.decider.bodies.lock().unwrap().is_empty());
}

#[tokio::test]
async fn other_paths_are_forwarded_streaming_with_the_callers_authorization() {
    let mut harness = harness(FakeDecider::default()).await;
    let mut response = harness
        .client
        .post(format!("{}/v1/messages?beta=true", harness.base))
        .header("authorization", "Bearer test-token")
        .header("x-custom", "kept")
        .body(r#"{"model": "m", "messages": []}"#)
        .send()
        .await
        .unwrap();
    let seen = harness.seen.recv().await.unwrap();
    assert_eq!(seen.method, "POST");
    assert_eq!(seen.target, "/v1/messages?beta=true");
    assert_eq!(seen.authorization.as_deref(), Some("Bearer test-token"));
    assert_eq!(seen.body, r#"{"model": "m", "messages": []}"#);
    // The upstream is addressed as itself, not as this router.
    assert_ne!(seen.host.as_deref(), harness.base.strip_prefix("http://"));

    assert_eq!(response.status(), 201);
    assert_eq!(response.headers()["x-upstream"], "yes");
    // The first chunk arrives while the upstream still holds the second.
    let first = tokio::time::timeout(Duration::from_secs(5), response.chunk())
        .await
        .expect("the first chunk is not buffered behind the second")
        .unwrap()
        .unwrap();
    assert_eq!(first, "first;");
    harness.release.notify_one();
    assert_eq!(response.chunk().await.unwrap().unwrap(), "second");
    assert!(response.chunk().await.unwrap().is_none());
    assert!(harness.decider.bodies.lock().unwrap().is_empty());

    // A method other than POST on the routes path is not this router's.
    harness.release.notify_one();
    let response = harness
        .client
        .get(format!("{}/v1/routes", harness.base))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let seen = harness.seen.recv().await.unwrap();
    assert_eq!(
        (seen.method.as_str(), seen.target.as_str()),
        ("GET", "/v1/routes")
    );
    assert_eq!(seen.authorization, None);
    assert!(seen.body.is_empty());
}

#[tokio::test]
async fn an_unreachable_upstream_is_a_502() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    let config = RouterConfig::new(catalog(), &format!("http://{closed}"));
    let router = Arc::new(Router::new(config, Arc::new(FakeDecider::default())).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(router.serve(listener));
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .get(format!("{base}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["error"]["code"], "unavailable");

    let health: Value = client
        .get(format!("{base}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
    assert_eq!(
        health["package"]["package_sha256"],
        json!(catalog().package_sha256)
    );
}

#[tokio::test]
async fn one_decide_is_in_flight_per_conversation_and_branch() {
    let harness = harness(FakeDecider {
        delay: Duration::from_millis(40),
        ..FakeDecider::default()
    })
    .await;
    let lookups = ["one", "one", "one", "one", "two", "two"]
        .map(|conversation| harness.routes(conversation).send());
    for response in futures::future::join_all(lookups).await {
        assert_eq!(response.unwrap().status(), 200);
    }
    assert_eq!(harness.decider.bodies.lock().unwrap().len(), 6);
    assert_eq!(harness.decider.most_per_episode.load(Ordering::SeqCst), 1);
    // Different conversations do not wait on each other.
    assert_eq!(harness.decider.most_at_once.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_refused_strict_decide_is_asked_again_relaxed() {
    let harness = harness(FakeDecider {
        strict_refusal: Some(Refusal::with_status(409, "session_busy", "busy")),
        ..FakeDecider::default()
    })
    .await;
    let response = harness.routes("conversation").send().await.unwrap();
    assert_eq!(response.status(), 200);
    let modes: Vec<Value> = harness
        .decider
        .bodies
        .lock()
        .unwrap()
        .iter()
        .map(|body| body["episode_mode"].clone())
        .collect();
    assert_eq!(modes, [json!("strict"), json!("relaxed")]);
}

fn unavailable() -> FakeDecider {
    FakeDecider {
        strict_refusal: Some(Refusal::new("unavailable", "down")),
        ..FakeDecider::default()
    }
}

/// [`request`] as a conversation's first turn: no model served yet.
fn first_turn() -> Value {
    let mut first = request();
    let execution = &mut first["routing"]["execution"];
    execution["last_served"] = Value::Null;
    execution["history"] = json!([]);
    first["messages"] = json!([{"role": "user", "content": "first"}]);
    first
}

#[tokio::test]
async fn an_unavailable_decider_keeps_the_held_model_and_is_not_asked_again() {
    let harness = harness(unavailable()).await;
    let response = harness.routes("conversation").send().await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    // The turn before was served by `a`.
    assert_eq!(body["model"], "anthropic/claude-haiku-5.5");
    assert_eq!(body["policy_action_id"], json!(common::id('a')));
    assert_eq!(body["receipt"]["decided_by"]["model"], "held");
    assert_eq!(
        body["presets"]["sage"]["policy_action_id"],
        json!(common::id('a'))
    );
    assert_eq!(harness.decider.bodies.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_unavailable_decider_with_no_history_serves_the_package_fallback() {
    let mut manifest: Value = serde_json::from_slice(&common::manifest()).unwrap();
    manifest["decision"] = json!({"fallback_action_id": common::id('b')});
    let catalog =
        rayline_arc_routes::Catalog::from_manifest(manifest.to_string().as_bytes(), common::ALIAS)
            .unwrap();
    let harness = harness_with(unavailable(), catalog, |_| {}).await;
    let response = harness
        .routes("conversation")
        .json(&first_turn())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["model"], "anthropic/claude-opus-5.5");
    assert_eq!(body["receipt"]["decided_by"]["model"], "utility_fallback");
}

#[tokio::test]
async fn an_unavailable_decider_without_a_package_fallback_still_routes() {
    // The fixture package names no fallback action, and the turn has no
    // history: an offered action is served rather than failing the turn.
    let harness = harness(unavailable()).await;
    let response = harness
        .routes("conversation")
        .json(&first_turn())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["receipt"]["decided_by"]["model"], "utility_fallback");
    assert!(body["model"].is_string());
}

#[tokio::test]
async fn a_decide_service_over_http_answers_or_refuses_with_its_code() {
    let service = serve_with(|request: Request<Incoming>| async move {
        assert_eq!(request.uri().path(), "/v1/rayline/arc/policy/decide");
        let body: Value =
            serde_json::from_slice(&request.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        let (status, answer) = match body["episode_mode"].as_str() {
            Some("strict") => (
                409,
                json!({"error": {"code": "conflict", "message": "private text",
                                                      "detail": {"reason": "session_busy"}}}),
            ),
            _ => (200, json!({"echo": body["turn_key"]})),
        };
        Response::builder()
            .status(status)
            .body(http_body_util::Full::new(Bytes::from(answer.to_string())).boxed_unsync())
            .unwrap()
    })
    .await;
    let decider = HttpDecider::new(&format!("http://{service}")).unwrap();
    let refusal = decider
        .decide(&json!({"episode_mode": "strict"}))
        .await
        .unwrap_err();
    assert_eq!(
        (refusal.status, refusal.code.as_str()),
        (409, "session_busy")
    );
    assert!(!refusal.message.contains("private"));
    let answer = decider
        .decide(&json!({"episode_mode": "relaxed", "turn_key": "k"}))
        .await
        .unwrap();
    assert_eq!(answer, json!({"echo": "k"}));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed = listener.local_addr().unwrap();
    drop(listener);
    let refusal = HttpDecider::new(&format!("http://{closed}"))
        .unwrap()
        .decide(&json!({}))
        .await
        .unwrap_err();
    assert_eq!(
        (refusal.status, refusal.code.as_str()),
        (503, "unavailable")
    );
}

#[tokio::test]
async fn only_loopback_callers_with_the_access_token_are_answered() {
    let mut harness = harness_with(FakeDecider::default(), catalog(), |config| {
        config.access_token = Some("local-secret".to_owned());
        config.upstream_authorization = Some("Bearer upstream-key".to_owned());
    })
    .await;
    let messages = format!("{}/v1/messages", harness.base);
    let status = |request: reqwest::RequestBuilder| async move {
        request.send().await.unwrap().status().as_u16()
    };
    // No token, a wrong one, a browser origin, a rebound host name.
    assert_eq!(status(harness.client.post(&messages)).await, 401);
    assert_eq!(
        status(harness.client.post(&messages).bearer_auth("local-secreT")).await,
        401
    );
    assert_eq!(
        status(
            harness
                .client
                .post(&messages)
                .bearer_auth("local-secret")
                .header("origin", "https://evil.example")
        )
        .await,
        403
    );
    assert_eq!(
        status(
            harness
                .client
                .post(&messages)
                .bearer_auth("local-secret")
                .header("host", "evil.example")
        )
        .await,
        403
    );
    assert_eq!(
        status(harness.client.post(format!("{}/v1/routes", harness.base))).await,
        401
    );
    assert!(harness.decider.bodies.lock().unwrap().is_empty());

    // The token admits the caller; upstream sees the upstream credential.
    harness.release.notify_one();
    let response = harness
        .client
        .post(&messages)
        .bearer_auth("local-secret")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let seen = harness.seen.recv().await.unwrap();
    assert_eq!(seen.authorization.as_deref(), Some("Bearer upstream-key"));
}
