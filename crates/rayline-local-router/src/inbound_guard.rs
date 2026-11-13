//! The per-instance token for the local process, which clients reach over
//! loopback.
//!
//! The local process already refuses browser-reachable traffic (non-loopback
//! `Host` or `Origin`, CORS preflight, non-JSON POST) before routing. That
//! keeps web pages out but not other local processes, so every request except
//! `GET /healthz` must also present the per-instance token as
//! `Authorization: Bearer` or `x-api-key`.

use hyper::header::{AUTHORIZATION, HeaderMap};
use hyper::{Method, StatusCode};

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
    /// A guard for `token`, which must not be empty: an empty token would
    /// admit an empty `x-api-key`.
    pub fn new(token: impl Into<String>) -> Self {
        let token = token.into();
        assert!(!token.is_empty(), "an inbound guard needs a token");
        Self { token }
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
            .any(|presented| constant_time_eq(presented.trim().as_bytes(), self.token.as_bytes()))
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
        InboundGuard::new(TOKEN).check(&method, path, &map)
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
    #[should_panic(expected = "an inbound guard needs a token")]
    fn an_empty_token_is_refused() {
        InboundGuard::new("");
    }

    #[test]
    fn debug_redacts_the_token() {
        let guard = InboundGuard::new(TOKEN);
        assert!(!format!("{guard:?}").contains(TOKEN));
    }
}
