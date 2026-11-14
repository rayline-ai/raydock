//! The router over real loopback sockets, with a fake policy and a fake upstream.

mod common;

use std::convert::Infallible;
use std::net::SocketAddr;
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
use rayline_arc_routes::{DecideFuture, Decider, Refusal, Router, RouterConfig, RoutingPrices};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::{Notify, mpsc};

type TestBody = http_body_util::combinators::UnsyncBoxBody<Bytes, Infallible>;

/// A policy that answers the fixture, counting what runs at once.
/// Answers [`answer`], or refuses with `refusal`; keeps every decide body.
#[derive(Default)]
struct FakeDecider {
    refusal: Option<Refusal>,
    bodies: Mutex<Vec<String>>,
}

impl FakeDecider {
    fn bodies(&self) -> Vec<Value> {
        let bodies = self.bodies.lock().unwrap();
        bodies
            .iter()
            .map(|body| serde_json::from_str(body).unwrap())
            .collect()
    }
}

impl Decider for FakeDecider {
    fn decide<'a>(&'a self, body: &'a str) -> DecideFuture<'a> {
        Box::pin(async move {
            self.bodies.lock().unwrap().push(body.to_owned());
            match &self.refusal {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(answer()),
            }
        })
    }

    fn action_prices(&self, _: Option<&RoutingPrices>) -> Vec<Value> {
        answer()["actions"].as_array().cloned().unwrap_or_default()
    }
}

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
    let bodies = harness.decider.bodies();
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["request"]["messages"], request()["messages"]);
    assert!(harness.seen.try_recv().is_err());
}

#[tokio::test]
async fn the_decider_gets_the_request_members_as_the_harness_sent_them() {
    let harness = harness(FakeDecider::default()).await;
    let mut sent = request();
    sent["tools"] = json!([{"name": "t", "input_schema": {"type": "object"}}]);
    // Keys out of sorted order, as a harness may send them.
    let text = sent
        .to_string()
        .replace(r#"{"type":"object"}"#, r#"{"z":1,"a":2}"#);
    let response = harness
        .routes("conversation")
        .header("content-type", "application/json")
        .body(text)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = harness.decider.bodies.lock().unwrap()[0].clone();
    assert!(body.contains(r#""input_schema":{"z":1,"a":2}"#), "{body}");
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

fn unavailable() -> FakeDecider {
    FakeDecider {
        refusal: Some(Refusal::new("unavailable", "down")),
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
    assert_eq!(harness.decider.bodies().len(), 1);
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
async fn an_unavailable_decider_never_falls_back_to_a_target_that_does_not_fit() {
    // `c` is the package fallback, but the request marks it not to fit.
    let mut manifest: Value = serde_json::from_slice(&common::manifest()).unwrap();
    manifest["decision"] = json!({"fallback_action_id": common::id('c')});
    let catalog =
        rayline_arc_routes::Catalog::from_manifest(manifest.to_string().as_bytes(), common::ALIAS)
            .unwrap();
    let harness = harness_with(unavailable(), catalog, |_| {}).await;
    // Marked not to fit, then (a local target) with its fit unknown.
    let mut unknown = first_turn();
    unknown["routing"]["execution"]["targets"][2]["capacity"] = Value::Null;
    assert_eq!(
        unknown["routing"]["execution"]["targets"][2]["action_id"],
        json!(common::id('c'))
    );
    for request in [first_turn(), unknown] {
        let response = harness
            .routes("conversation")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["receipt"]["decided_by"]["model"], "utility_fallback");
        assert_ne!(body["policy_action_id"], json!(common::id('c')));
    }
}

#[tokio::test]
async fn an_unavailable_decider_keeps_a_held_local_target_whose_fit_is_unknown() {
    let harness = harness(unavailable()).await;
    let mut request = request();
    let execution = &mut request["routing"]["execution"];
    execution["targets"][2]["capacity"] = Value::Null;
    execution["hold"] = json!({"scope": "action", "action_id": common::id('c')});
    let response = harness
        .routes("conversation")
        .json(&request)
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["policy_action_id"], json!(common::id('c')));
    assert_eq!(body["receipt"]["decided_by"]["model"], "held");
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
