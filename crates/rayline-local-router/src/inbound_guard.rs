//! Optional per-instance token for a local router that a desktop app reaches
//! over loopback (`rayline claude-desktop`).
//!
//! Every local router already refuses browser-reachable traffic (non-loopback
//! `Host` or `Origin`, CORS preflight, non-JSON POST) before routing. That
//! keeps web pages out but not other local processes. With a guard configured,
//! every request except `GET /healthz` must also present the per-instance
//! token as `Authorization: Bearer` or `x-api-key`.

use hyper::header::{AUTHORIZATION, HeaderMap};
use hyper::{Method, StatusCode};

/// Env var `rld serve` reads the per-instance token from.
pub const INBOUND_TOKEN_ENV: &str = "RAYLINE_LOCAL_ROUTER_INBOUND_TOKEN";

const API_KEY_HEADER: &str = "x-api-key";

#[derive(Clone, Eq, PartialEq)]
pub struct InboundGuard {
    token: String,
}

impl std::fmt::Debug for InboundGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InboundGuard")
            .field("token", &"<redacted>")
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
    /// A guard for `token`; `None` for an empty token (no guard is not a guard
    /// with an empty secret).
    pub fn new(token: impl Into<String>) -> Option<Self> {
        let token = token.into();
        (!token.is_empty()).then_some(Self { token })
    }

    /// The guard named by [`INBOUND_TOKEN_ENV`], if it is set and non-empty.
    pub fn from_env() -> Option<Self> {
        Self::new(std::env::var(INBOUND_TOKEN_ENV).ok()?)
    }

    pub fn check(&self, method: &Method, path: &str, headers: &HeaderMap) -> Result<(), Refusal> {
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
        Ok(())
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
            .any(|presented| self.is_token(presented))
    }

    /// Whether `presented` (a bearer token or `x-api-key` value) is this
    /// guard's token. A route that forwards caller credentials upstream uses
    /// it to keep the router's own token from reaching a provider.
    pub fn is_token(&self, presented: &str) -> bool {
        constant_time_eq(presented.trim().as_bytes(), self.token.as_bytes())
    }
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

    fn check(pairs: &[(&'static str, &str)], method: Method, path: &str) -> Result<(), Refusal> {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(*name, HeaderValue::from_str(value).expect("header"));
        }
        InboundGuard::new(TOKEN)
            .expect("guard")
            .check(&method, path, &map)
    }

    #[test]
    fn accepts_bearer_and_x_api_key() {
        let bearer = format!("Bearer {TOKEN}");
        assert_eq!(
            check(&[("authorization", &bearer)], Method::POST, "/v1/messages"),
            Ok(())
        );
        assert_eq!(
            check(&[("x-api-key", TOKEN)], Method::GET, "/v1/models"),
            Ok(())
        );
    }

    #[test]
    fn refuses_missing_or_wrong_token() {
        for pairs in [
            &[][..],
            &[("authorization", "Bearer rlt-test-token-0123456780")][..],
            &[("x-api-key", "rayline-local-placeholder")][..],
            &[("authorization", TOKEN)][..],
        ] {
            assert_eq!(
                check(pairs, Method::POST, "/v1/messages").map_err(|r| r.status),
                Err(StatusCode::UNAUTHORIZED),
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn only_get_healthz_skips_the_token() {
        assert_eq!(check(&[], Method::GET, "/healthz"), Ok(()));
        assert!(check(&[], Method::POST, "/healthz").is_err());
        assert!(check(&[], Method::GET, "/healthz?x=1").is_err());
        assert!(check(&[], Method::GET, "/v1/models").is_err());
    }

    #[test]
    fn empty_token_is_no_guard_and_debug_redacts() {
        assert!(InboundGuard::new("").is_none());
        let guard = InboundGuard::new(TOKEN).expect("guard");
        assert!(!format!("{guard:?}").contains(TOKEN));
    }
}
