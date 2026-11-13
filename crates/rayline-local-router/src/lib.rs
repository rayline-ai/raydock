//! Shared helpers for the local process: the loopback request guard,
//! the inbound token guard, session headers, model capabilities, the Decisions
//! URL check and owner-only state on Windows.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::header::{HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Response, StatusCode};
use serde_json::{Value, json};

mod capabilities;
pub mod decisions_url;
pub mod inbound_guard;
#[cfg(windows)]
pub mod windows_state;

pub use capabilities::ModelCapabilities;
pub use inbound_guard::InboundGuard;

/// Client session headers the hosted router keys episodes (conversations) on.
/// A client that names its session only in a header would otherwise be keyed
/// on a derived id once its request passes through a local gateway. The hybrid
/// gateway's cloud classifier pin forwards exactly this set.
pub const SESSION_HEADERS: [&str; 4] = [
    "x-session-affinity",
    "x-claude-code-session-id",
    "x-session-id",
    "x-conversation-id",
];

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn json_response(status: StatusCode, value: Value) -> Response<BoxBody> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(body))
        .unwrap()
}

/// Hosts the router answers as.
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

/// The router listens on loopback without auth and spends stored provider
/// credentials, so refuse traffic a web page could originate: a `Host` other
/// than our loopback host:port (DNS rebinding), a non-loopback `Origin`, a CORS
/// preflight, or a POST body that is not JSON (a "simple" request that skips
/// preflight).
pub fn reject_untrusted_request(
    port: u16,
    method: &Method,
    uri: &hyper::Uri,
    headers: &HeaderMap,
) -> Option<Response<BoxBody>> {
    let refuse = |status: StatusCode, kind: &str, message: &str| {
        Some(json_response(
            status,
            json!({"type":"error","error":{"type":kind,"message":message}}),
        ))
    };
    // HTTP/2 carries the authority in the URI instead of a Host header. When
    // both are present they must agree, and every guarded header must appear at
    // most once: the checks below read one value, so a second one would slip by.
    let host_header = single_header(headers, hyper::header::HOST);
    let host = match (uri.authority(), host_header) {
        (_, Err(())) => None,
        (Some(authority), Ok(None)) => Some(authority.as_str()),
        (Some(authority), Ok(Some(value))) => value
            .to_str()
            .ok()
            .filter(|value| value.eq_ignore_ascii_case(authority.as_str())),
        (None, Ok(value)) => value.and_then(|value| value.to_str().ok()),
    };
    if !host.is_some_and(|host| is_loopback_authority(host, port)) {
        return refuse(
            StatusCode::MISDIRECTED_REQUEST,
            "invalid_request_error",
            "this router only answers requests addressed to its loopback host and port",
        );
    }
    let origin_ok = single_header(headers, hyper::header::ORIGIN).is_ok_and(|origin| {
        origin.is_none_or(|origin| {
            origin.to_str().ok().is_some_and(|origin| {
                origin
                    .strip_prefix("http://")
                    .or_else(|| origin.strip_prefix("https://"))
                    .is_some_and(|authority| is_loopback_authority(authority, port))
            })
        })
    });
    if !origin_ok || *method == Method::OPTIONS {
        return refuse(
            StatusCode::FORBIDDEN,
            "permission_error",
            "cross-origin requests are not accepted",
        );
    }
    let Ok(content_type) = single_header(headers, hyper::header::CONTENT_TYPE) else {
        return refuse(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "invalid_request_error",
            "request bodies must carry exactly one content-type",
        );
    };
    if *method == Method::POST {
        let mime = content_type
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .map(str::trim);
        let allowed = mime.is_some_and(|mime| {
            mime.eq_ignore_ascii_case("application/json")
                // Codex subscription passthrough uploads image edits as
                // multipart; a browser sending this cross-site still carries
                // the Origin refused above.
                || (uri.path() == "/v1/images/edits"
                    && mime.eq_ignore_ascii_case("multipart/form-data"))
        });
        if !allowed {
            return refuse(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "invalid_request_error",
                "request bodies must be sent as application/json",
            );
        }
    }
    None
}

/// The header's only value, `Ok(None)` when absent, or `Err` when repeated.
fn single_header(headers: &HeaderMap, name: HeaderName) -> Result<Option<&HeaderValue>, ()> {
    let mut values = headers.get_all(name).iter();
    let first = values.next();
    match values.next() {
        Some(_) => Err(()),
        None => Ok(first),
    }
}

/// True for `127.0.0.1`, `localhost` or `[::1]` on exactly `port`.
fn is_loopback_authority(authority: &str, port: u16) -> bool {
    let Some((host, authority_port)) = authority.rsplit_once(':') else {
        return false;
    };
    LOOPBACK_HOSTS.contains(&host.to_ascii_lowercase().as_str())
        && authority_port.parse::<u16>().ok() == Some(port)
}

#[cfg(test)]
mod tests {
    use super::*;

    type GuardHeaders = &'static [(&'static str, &'static str)];

    const GUARD_PORT: u16 = 20811;

    fn guard_status(method: Method, uri: &str, headers: &[(&str, &str)]) -> Option<StatusCode> {
        let uri = uri.parse::<hyper::Uri>().unwrap();
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.append(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        reject_untrusted_request(GUARD_PORT, &method, &uri, &map).map(|response| response.status())
    }

    #[test]
    fn request_guard_allows_loopback_clients() {
        const JSON: (&str, &str) = ("content-type", "application/json");
        let cases: &[(Method, GuardHeaders)] = &[
            (Method::GET, &[("host", "127.0.0.1:20811")]),
            (Method::GET, &[("host", "LOCALHOST:20811")]),
            (Method::GET, &[("host", "[::1]:20811")]),
            (Method::POST, &[("host", "127.0.0.1:20811"), JSON]),
            (
                Method::POST,
                &[
                    ("host", "127.0.0.1:20811"),
                    ("content-type", "Application/JSON; charset=utf-8"),
                ],
            ),
            (
                Method::POST,
                &[
                    ("host", "localhost:20811"),
                    ("origin", "http://127.0.0.1:20811"),
                    JSON,
                ],
            ),
        ];
        for (method, headers) in cases {
            assert_eq!(
                guard_status(method.clone(), "/v1/messages", headers),
                None,
                "{method} {headers:?}"
            );
        }
        // Codex image edits upload multipart; no other route accepts it.
        const MULTIPART: GuardHeaders = &[
            ("host", "127.0.0.1:20811"),
            ("content-type", "multipart/form-data; boundary=x"),
        ];
        assert_eq!(
            guard_status(Method::POST, "/v1/images/edits", MULTIPART),
            None
        );
        assert_eq!(
            guard_status(Method::POST, "/v1/messages", MULTIPART),
            Some(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        );
        // HTTP/2 carries the authority in the URI instead of a Host header.
        assert_eq!(
            guard_status(Method::GET, "http://127.0.0.1:20811/healthz", &[]),
            None
        );
    }

    #[test]
    fn request_guard_rejects_browser_reachable_requests() {
        const HOST: (&str, &str) = ("host", "127.0.0.1:20811");
        const JSON: (&str, &str) = ("content-type", "application/json");
        let cases: &[(Method, GuardHeaders, StatusCode)] = &[
            // DNS rebinding, another port, or no port at all.
            (
                Method::GET,
                &[("host", "evil.example:20811")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                Method::GET,
                &[("host", "attacker.localhost:20811")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                Method::GET,
                &[("host", "evil.example@127.0.0.1:20811")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                Method::GET,
                &[("host", "127.0.0.1:20812")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (
                Method::GET,
                &[("host", "127.0.0.1")],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (Method::GET, &[], StatusCode::MISDIRECTED_REQUEST),
            // Cross-site pages, opaque origins, other loopback ports, preflight.
            (
                Method::POST,
                &[HOST, JSON, ("origin", "https://evil.example")],
                StatusCode::FORBIDDEN,
            ),
            (
                Method::POST,
                &[HOST, JSON, ("origin", "null")],
                StatusCode::FORBIDDEN,
            ),
            (
                Method::GET,
                &[HOST, ("origin", "http://127.0.0.1.evil.example:20811")],
                StatusCode::FORBIDDEN,
            ),
            (
                Method::POST,
                &[HOST, JSON, ("origin", "http://localhost:3000")],
                StatusCode::FORBIDDEN,
            ),
            (Method::OPTIONS, &[HOST], StatusCode::FORBIDDEN),
            // CORS "simple" POSTs that skip preflight, and POSTs with no type.
            (
                Method::POST,
                &[HOST, ("content-type", "text/plain")],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (
                Method::POST,
                &[HOST, ("content-type", "application/x-www-form-urlencoded")],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (Method::POST, &[HOST], StatusCode::UNSUPPORTED_MEDIA_TYPE),
        ];
        for (method, headers, expected) in cases {
            assert_eq!(
                guard_status(method.clone(), "/v1/messages", headers),
                Some(*expected),
                "{method} {headers:?}"
            );
        }
        assert_eq!(
            guard_status(Method::GET, "http://evil.example:20811/healthz", &[]),
            Some(StatusCode::MISDIRECTED_REQUEST)
        );
    }

    #[test]
    fn request_guard_rejects_repeated_or_conflicting_headers() {
        const HOST: (&str, &str) = ("host", "127.0.0.1:20811");
        const JSON: (&str, &str) = ("content-type", "application/json");
        const LOOPBACK_ORIGIN: (&str, &str) = ("origin", "http://127.0.0.1:20811");
        let cases: &[(GuardHeaders, StatusCode)] = &[
            // A second Host the first-value check never looked at.
            (
                &[HOST, ("host", "evil.example:20811"), JSON],
                StatusCode::MISDIRECTED_REQUEST,
            ),
            (&[HOST, HOST, JSON], StatusCode::MISDIRECTED_REQUEST),
            // A loopback Origin followed by a cross-site or opaque one.
            (
                &[
                    HOST,
                    JSON,
                    LOOPBACK_ORIGIN,
                    ("origin", "https://evil.example"),
                ],
                StatusCode::FORBIDDEN,
            ),
            (
                &[HOST, JSON, LOOPBACK_ORIGIN, ("origin", "null")],
                StatusCode::FORBIDDEN,
            ),
            // JSON followed by a CORS "simple" type.
            (
                &[HOST, JSON, ("content-type", "text/plain")],
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ),
            (&[HOST, JSON, JSON], StatusCode::UNSUPPORTED_MEDIA_TYPE),
        ];
        for (headers, expected) in cases {
            assert_eq!(
                guard_status(Method::POST, "/v1/messages", headers),
                Some(*expected),
                "{headers:?}"
            );
        }
        assert_eq!(
            guard_status(
                Method::GET,
                "/healthz",
                &[HOST, ("content-type", "text/plain"), JSON]
            ),
            Some(StatusCode::UNSUPPORTED_MEDIA_TYPE)
        );
    }

    #[test]
    fn request_guard_requires_uri_authority_and_host_to_agree() {
        const URI: &str = "http://127.0.0.1:20811/v1/messages";
        const JSON: (&str, &str) = ("content-type", "application/json");
        for host in ["evil.example:20811", "localhost:20811", "127.0.0.1:20812"] {
            assert_eq!(
                guard_status(Method::POST, URI, &[("host", host), JSON]),
                Some(StatusCode::MISDIRECTED_REQUEST),
                "{host}"
            );
        }
        // A rebound authority is refused even with a loopback Host header.
        assert_eq!(
            guard_status(
                Method::POST,
                "http://evil.example:20811/v1/messages",
                &[("host", "127.0.0.1:20811"), JSON]
            ),
            Some(StatusCode::MISDIRECTED_REQUEST)
        );
        // Agreeing authority and Host (case-insensitive) pass, as does either alone.
        assert_eq!(
            guard_status(Method::POST, URI, &[("host", "127.0.0.1:20811"), JSON]),
            None
        );
        assert_eq!(
            guard_status(
                Method::POST,
                "http://LOCALHOST:20811/v1/messages",
                &[("host", "localhost:20811"), JSON]
            ),
            None
        );
        assert_eq!(guard_status(Method::POST, URI, &[JSON]), None);
    }
}
