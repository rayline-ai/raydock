//! A decision passes the core's checks; a decider's refusal is the route's.

mod common;

use common::{answer, assert_core_accepts, catalog, request};
use rayline_arc_routes::{DecideFuture, Decider, Refusal, RequestHeaders, Router, RouterConfig};

/// A decider that answers, or refuses with `refusal`.
struct Kind(Option<Refusal>);

impl Decider for Kind {
    fn decide<'a>(&'a self, _body: &'a str) -> DecideFuture<'a> {
        Box::pin(async move {
            match &self.0 {
                Some(refusal) => Err(refusal.clone()),
                None => Ok(answer()),
            }
        })
    }
}

async fn route(decider: Kind) -> Result<serde_json::Value, Refusal> {
    let router = Router::new(
        RouterConfig::new(catalog(), "http://127.0.0.1:9"),
        std::sync::Arc::new(decider),
    )
    .unwrap();
    let headers = RequestHeaders {
        conversation_id: Some("c1".to_owned()),
        ..RequestHeaders::default()
    };
    router
        .route(&serde_json::to_vec(&request()).unwrap(), &headers)
        .await
}

#[tokio::test]
async fn a_decision_passes_the_core_checks_and_a_refusal_is_returned() {
    let shaped = route(Kind(None)).await.expect("a decision");
    assert_core_accepts(&shaped, &request());

    let down = Refusal::with_status(503, "backend_unavailable", "x");
    assert_eq!(route(Kind(Some(down.clone()))).await.unwrap_err(), down);
}
