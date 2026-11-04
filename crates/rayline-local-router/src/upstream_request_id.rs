//! The cloud router's id for a forwarded request.
//!
//! The router answers each forward it routes with its own route id in
//! `x-rayline-route-id`. Nothing else on the machine sees that id, so each
//! forwarder reads it as soon as the upstream headers are in: it records the id
//! on the request's metrics row right away (before any body parsing that may
//! fail) and stamps it on the response it builds as
//! [`RAYLINE_UPSTREAM_REQUEST_ID_HEADER`]. The listener that routed the request
//! reads the stamp back to log it next to its route line. The header is on the
//! response before the first body byte, so streaming responses need no
//! buffering.
//!
//! A forward to a provider directly, rather than through the router, has no
//! route id and so records none; see `UPSTREAM_ROUTE_ID_HEADER`.

use hyper::HeaderMap;
use hyper::header::HeaderValue;
use rayline_metrics::{MetricsUpdate, SharedMetricsSink, UPSTREAM_ROUTE_ID_HEADER};
use tracing::info;

/// Response header this router sets with the upstream's id for the request.
pub const RAYLINE_UPSTREAM_REQUEST_ID_HEADER: &str = "x-rayline-upstream-request-id";

/// The cloud router's route id for the request, from its response headers.
/// Blank values count as absent, as does a response from an upstream that is
/// not the router.
pub(crate) fn upstream_request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(UPSTREAM_ROUTE_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

/// Put the upstream's id on a response this router is about to return.
/// No-op when the upstream sent none.
pub(crate) fn stamp_upstream_request_id(
    headers: &mut HeaderMap,
    upstream_request_id: Option<&str>,
) {
    if let Some(value) = upstream_request_id.and_then(|id| HeaderValue::from_str(id).ok()) {
        headers.insert(RAYLINE_UPSTREAM_REQUEST_ID_HEADER, value);
    }
}

/// Keep the upstream's id on the metrics row for `request_id`. Call it as soon
/// as the upstream headers are in, so a body that later fails to parse, or a
/// redirect that cannot be followed, still leaves the id on the (errored) row.
pub(crate) fn record_upstream_request_id(
    metrics: Option<&SharedMetricsSink>,
    request_id: &str,
    upstream_request_id: Option<&str>,
) {
    if let (Some(metrics), Some(upstream_request_id)) = (metrics, upstream_request_id) {
        metrics.record(MetricsUpdate::UpstreamRequestId {
            request_id: request_id.to_owned(),
            upstream_request_id: upstream_request_id.to_owned(),
        });
    }
}

/// The id a forwarder stamped on its response, if any.
pub(crate) fn stamped_upstream_request_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(RAYLINE_UPSTREAM_REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// Log the upstream's id for a routed request. `surface` names the listener
/// (`local`, `chat`, `codex`) so the line sits next to that listener's route
/// line, which carries the same `request_id`. Silent when the response carries
/// no upstream id (direct local inference, or an upstream that sends none).
pub(crate) fn log_upstream_request_id<B>(
    surface: &str,
    request_id: &str,
    response: &hyper::Response<B>,
) {
    let Some(upstream_request_id) = stamped_upstream_request_id(response.headers()) else {
        return;
    };
    info!(
        "{surface} route upstream request_id={request_id} upstream_request_id={upstream_request_id} status={}",
        response.status().as_u16()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.append(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn reads_the_gateway_route_id_and_ignores_other_id_headers() {
        assert_eq!(
            upstream_request_id(&headers(&[("x-rayline-route-id", "rt_rayline")])).as_deref(),
            Some("rt_rayline")
        );
        // A direct-provider forward relays the provider's own `x-request-id`,
        // and the legacy name is only ever a duplicate of the one we read.
        assert_eq!(
            upstream_request_id(&headers(&[
                ("x-request-id", "provider_id"),
                ("x-workshop-route-id", "rt_legacy"),
            ])),
            None
        );
        assert_eq!(
            upstream_request_id(&headers(&[("content-type", "application/json")])),
            None
        );
    }

    #[test]
    fn blank_and_padded_ids_are_handled() {
        assert_eq!(
            upstream_request_id(&headers(&[("x-rayline-route-id", "  ")])),
            None
        );
        assert_eq!(
            upstream_request_id(&headers(&[("x-rayline-route-id", " rt_padded ")])).as_deref(),
            Some("rt_padded")
        );
    }

    #[test]
    fn stamps_records_and_reads_back_the_rayline_header() {
        let metrics = rayline_metrics::RouterMetrics::new("test");
        let sink: SharedMetricsSink = metrics.clone();
        let upstream = headers(&[("x-rayline-route-id", "rt_abc")]);
        let id = upstream_request_id(&upstream);
        record_upstream_request_id(Some(&sink), "req_1", id.as_deref());
        let mut response = HeaderMap::new();
        stamp_upstream_request_id(&mut response, id.as_deref());
        assert_eq!(
            stamped_upstream_request_id(&response).as_deref(),
            Some("rt_abc")
        );
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.active[0].request_id, "req_1");
        assert_eq!(
            snapshot.active[0].upstream_request_id.as_deref(),
            Some("rt_abc")
        );

        // No upstream id: no header, and no row conjured for it.
        let mut untouched = HeaderMap::new();
        let none = upstream_request_id(&HeaderMap::new());
        record_upstream_request_id(Some(&sink), "req_2", none.as_deref());
        stamp_upstream_request_id(&mut untouched, none.as_deref());
        assert!(untouched.is_empty());
        assert_eq!(metrics.snapshot().active.len(), 1, "no id, no row");
    }
}
