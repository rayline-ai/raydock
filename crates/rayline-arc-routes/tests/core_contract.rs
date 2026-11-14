//! Every answer the routes service can give, a decision or a fallback for
//! each reason the decider gives none, passes the core's checks.

mod common;

use common::{answer, assert_core_accepts, catalog, request};
use rayline_arc_routes::{
    DecideFuture, Decider, Refusal, RequestHeaders, Router, RouterConfig, RoutingPrices,
};
use serde_json::Value;

/// A decider that answers, or refuses as the local decider does for one
/// reason, and prices actions as the local decider does.
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

    fn action_prices(&self, _: Option<&RoutingPrices>) -> Vec<Value> {
        answer()["actions"].as_array().cloned().unwrap_or_default()
    }
}

#[tokio::test]
async fn every_answer_kind_passes_the_core_checks() {
    let kinds = [
        ("decision", None),
        (
            "oversize",
            Some(Refusal::with_status(503, "input_too_large", "x")),
        ),
        ("timeout", Some(Refusal::with_status(504, "timeout", "x"))),
        (
            "unsupported format",
            Some(Refusal::with_status(503, "request_format_unsupported", "x")),
        ),
        (
            "unsupported media",
            Some(Refusal::with_status(503, "unsupported_request", "x")),
        ),
        (
            "encoder down",
            Some(Refusal::with_status(503, "backend_unavailable", "x")),
        ),
    ];
    for (kind, refusal) in kinds {
        let fallback = refusal.is_some();
        let router = Router::new(
            RouterConfig::new(catalog(), "http://127.0.0.1:9"),
            std::sync::Arc::new(Kind(refusal)),
        )
        .unwrap();
        let headers = RequestHeaders {
            conversation_id: Some("c1".to_owned()),
            ..RequestHeaders::default()
        };
        let sent = request();
        let shaped = router
            .route(&serde_json::to_vec(&sent).unwrap(), &headers)
            .await
            .unwrap_or_else(|refusal| panic!("{kind}: refused {refusal:?}"));
        assert_core_accepts(&shaped, &sent);
        if fallback {
            assert_eq!(shaped["receipt"]["basis"], "encoder_unavailable", "{kind}");
        }
    }
}
