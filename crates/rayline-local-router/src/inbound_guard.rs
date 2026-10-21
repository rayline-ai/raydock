//! Optional inbound guard for a local router that a desktop app reaches over
//! loopback (`rayline claude-desktop`).
//!
//! Binding to 127.0.0.1 alone does not keep other parties out: any local
//! process can connect, and a web page can reach the port through DNS
//! rebinding (its requests carry a non-loopback `Host`) or a simple
//! cross-origin `text/plain` POST. With a guard configured, every request must
//! name a loopback `Host` on the router's port, carry no non-loopback
//! `Origin`, send JSON bodies as `application/json`, and (except `/healthz`)
//! present the per-instance token as `Authorization: Bearer` or `x-api-key`.

use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, ORIGIN};
use hyper::{Method, StatusCode};

/// Env var `rld serve` reads the per-instance token from.
pub const INBOUND_TOKEN_ENV: &str = "RAYLINE_LOCAL_ROUTER_INBOUND_TOKEN";

const API_KEY_HEADER: &str = "x-api-key";
const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];

#[derive(Clone, Eq, PartialEq)]
pub struct InboundGuard {
    token: String,
    port: u16,
}

impl std::fmt::Debug for InboundGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundGuard")
            .field("token", &"<redacted>")
            .field("port", &self.port)
            .finish()
    }
}

/// Why a request was refused: the status and a message safe to return.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Refusal {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: &'static str,
}

impl InboundGuard {
    /// A guard for `port`; `None` for an empty token (no guard is not a guard
    /// with an empty secret).
    pub fn new(token: impl Into<String>, port: u16) -> Option<Self> {
        let token = token.into();
        (!token.is_empty()).then_some(Self { token, port })
    }

    /// The guard named by [`INBOUND_TOKEN_ENV`], if it is set and non-empty.
    pub fn from_env(port: u16) -> Option<Self> {
        Self::new(std::env::var(INBOUND_TOKEN_ENV).ok()?, port)
    }

    pub fn check(&self, method: &Method, path: &str, headers: &HeaderMap) -> Result<(), Refusal> {
        if !self.host_is_loopback(headers) {
            return Err(Refusal {
                status: StatusCode::MISDIRECTED_REQUEST,
                kind: "invalid_request_error",
                message: "this router only answers requests addressed to its loopback host and port",
            });
        }
        if !self.origin_is_loopback(headers) {
            return Err(Refusal {
                status: StatusCode::FORBIDDEN,
                kind: "permission_error",
                message: "cross-origin requests are not accepted",
            });
        }
        if method == Method::OPTIONS {
            return Err(Refusal {
                status: StatusCode::FORBIDDEN,
                kind: "permission_error",
                message: "cross-origin requests are not accepted",
            });
        }
        if path == "/healthz" && method == Method::GET {
            return Ok(());
        }
        if !self.authorized(headers) {
            return Err(Refusal {
                status: StatusCode::UNAUTHORIZED,
                kind: "authentication_error",
                message: "missing or invalid router token",
            });
        }
        if method == Method::POST && !is_json(headers) {
            return Err(Refusal {
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE,
                kind: "invalid_request_error",
                message: "request bodies must be sent as application/json",
            });
        }
        Ok(())
    }

    fn host_is_loopback(&self, headers: &HeaderMap) -> bool {
        let Some(host) = headers.get(HOST).and_then(|value| value.to_str().ok()) else {
            return false;
        };
        self.is_loopback_authority(host)
    }

    fn origin_is_loopback(&self, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(ORIGIN) else {
            return true;
        };
        let Ok(origin) = origin.to_str() else {
            return false;
        };
        let authority = origin
            .strip_prefix("http://")
            .or_else(|| origin.strip_prefix("https://"));
        authority.is_some_and(|authority| self.is_loopback_authority(authority))
    }

    fn is_loopback_authority(&self, authority: &str) -> bool {
        let Some((host, port)) = authority.rsplit_once(':') else {
            return false;
        };
        LOOPBACK_HOSTS.contains(&host.to_ascii_lowercase().as_str())
            && port.parse::<u16>().ok() == Some(self.port)
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let bearer = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| {
                value
                    .strip_prefix("Bearer ")
                    .or_else(|| value.strip_prefix("bearer "))
            });
        let api_key = headers
            .get(API_KEY_HEADER)
            .and_then(|value| value.to_str().ok());
        [bearer, api_key]
            .into_iter()
            .flatten()
            .any(|presented| constant_time_eq(presented.trim().as_bytes(), self.token.as_bytes()))
    }
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media| media.trim().eq_ignore_ascii_case("application/json"))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use hyper::header::HeaderValue;

    const TOKEN: &str = "rlt-test-token-0123456789";

    fn guard() -> InboundGuard {
        InboundGuard::new(TOKEN, 20831).expect("guard")
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).expect("header"));
        }
        map
    }

    fn ok_post() -> Vec<(&'static str, String)> {
        vec![
            ("host", "127.0.0.1:20831".to_owned()),
            ("authorization", format!("Bearer {TOKEN}")),
            ("content-type", "application/json".to_owned()),
        ]
    }

    fn check(pairs: &[(&'static str, String)], method: Method, path: &str) -> Result<(), Refusal> {
        let refs: Vec<(&'static str, &str)> = pairs.iter().map(|(n, v)| (*n, v.as_str())).collect();
        guard().check(&method, path, &headers(&refs))
    }

    #[test]
    fn accepts_bearer_and_x_api_key_on_loopback() {
        assert_eq!(check(&ok_post(), Method::POST, "/v1/messages"), Ok(()));
        let api_key = vec![
            ("host", "localhost:20831".to_owned()),
            ("x-api-key", TOKEN.to_owned()),
            ("content-type", "application/json; charset=utf-8".to_owned()),
        ];
        assert_eq!(check(&api_key, Method::POST, "/v1/messages"), Ok(()));
        let ipv6 = vec![
            ("host", "[::1]:20831".to_owned()),
            ("authorization", format!("Bearer {TOKEN}")),
        ];
        assert_eq!(check(&ipv6, Method::GET, "/v1/models"), Ok(()));
    }

    #[test]
    fn refuses_missing_or_wrong_token() {
        let mut no_token = ok_post();
        no_token.remove(1);
        assert_eq!(
            check(&no_token, Method::POST, "/v1/messages").map_err(|r| r.status),
            Err(StatusCode::UNAUTHORIZED)
        );
        let mut wrong = ok_post();
        wrong[1].1 = "Bearer rlt-test-token-0123456780".to_owned();
        assert_eq!(
            check(&wrong, Method::POST, "/v1/messages").map_err(|r| r.status),
            Err(StatusCode::UNAUTHORIZED)
        );
        let mut placeholder = ok_post();
        placeholder[1].1 = "Bearer rayline-local-placeholder".to_owned();
        assert!(check(&placeholder, Method::POST, "/v1/messages").is_err());
    }

    #[test]
    fn refuses_dns_rebinding_hosts_and_wrong_ports() {
        for host in [
            "evil.example:20831",
            "127.0.0.1:20811",
            "127.0.0.1",
            "attacker.localhost:20831",
        ] {
            let mut pairs = ok_post();
            pairs[0].1 = host.to_owned();
            assert_eq!(
                check(&pairs, Method::POST, "/v1/messages").map_err(|r| r.status),
                Err(StatusCode::MISDIRECTED_REQUEST),
                "{host}"
            );
        }
        let mut no_host = ok_post();
        no_host.remove(0);
        assert!(check(&no_host, Method::POST, "/v1/messages").is_err());
    }

    #[test]
    fn refuses_cross_origin_and_preflight() {
        let mut cross = ok_post();
        cross.push(("origin", "https://evil.example".to_owned()));
        assert_eq!(
            check(&cross, Method::POST, "/v1/messages").map_err(|r| r.status),
            Err(StatusCode::FORBIDDEN)
        );
        let mut null_origin = ok_post();
        null_origin.push(("origin", "null".to_owned()));
        assert!(check(&null_origin, Method::POST, "/v1/messages").is_err());
        let mut same = ok_post();
        same.push(("origin", "http://127.0.0.1:20831".to_owned()));
        assert_eq!(check(&same, Method::POST, "/v1/messages"), Ok(()));
        assert_eq!(
            check(&ok_post(), Method::OPTIONS, "/v1/messages").map_err(|r| r.status),
            Err(StatusCode::FORBIDDEN)
        );
    }

    #[test]
    fn refuses_non_json_bodies() {
        for content_type in [
            "text/plain",
            "application/x-www-form-urlencoded",
            "multipart/form-data",
        ] {
            let mut pairs = ok_post();
            pairs[2].1 = content_type.to_owned();
            assert_eq!(
                check(&pairs, Method::POST, "/v1/messages").map_err(|r| r.status),
                Err(StatusCode::UNSUPPORTED_MEDIA_TYPE),
                "{content_type}"
            );
        }
        let mut missing = ok_post();
        missing.remove(2);
        assert!(check(&missing, Method::POST, "/v1/messages").is_err());
    }

    #[test]
    fn healthz_needs_loopback_host_but_no_token() {
        let pairs = vec![("host", "127.0.0.1:20831".to_owned())];
        assert_eq!(check(&pairs, Method::GET, "/healthz"), Ok(()));
        let rebound = vec![("host", "evil.example:20831".to_owned())];
        assert!(check(&rebound, Method::GET, "/healthz").is_err());
    }

    #[test]
    fn empty_token_is_no_guard_and_debug_redacts() {
        assert!(InboundGuard::new("", 1).is_none());
        assert!(!format!("{:?}", guard()).contains(TOKEN));
    }
}
