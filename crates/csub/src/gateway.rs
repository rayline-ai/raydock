//! Loopback reverse proxy for Claude Code launched with
//! `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>/l/<launch-id>`.
//!
//! `POST /v1/messages` is placed on a pool account: the client's OAuth bearer
//! is replaced by the selected account's token and explicit pre-stream quota,
//! entitlement or credential rejections fail over to another account. Every
//! other request is forwarded unchanged with the client's own credentials.
//! Partially streamed responses and network failures are never replayed.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::{StreamExt as _, TryStreamExt as _};
use http_body_util::{BodyExt, Full, StreamBody, combinators::BoxBody};
use hyper::body::{Frame, Incoming};
use hyper::header::{self, HeaderName, HeaderValue};
use hyper::{HeaderMap, Method, Request, Response, StatusCode};
use rayline_subscriptions::{
    HeaderSnapshot, OAuthRefreshError, ResponseClassification, SubscriptionPoolRuntime,
    SubscriptionRuntimeError, classify_response,
};
use serde_json::{Value, json};
use tracing::{debug, warn};

pub type Body = BoxBody<Bytes, std::io::Error>;

/// Path prefix carrying the launch identity used for account affinity.
pub const LAUNCH_PREFIX: &str = "/l/";
const SESSION_ID_HEADER: &str = "x-claude-code-session-id";
const MAX_REQUEST_BYTES: usize = 64 * 1024 * 1024;
const MAX_ERROR_BYTES: usize = 2 * 1024 * 1024;
const ERROR_BODY_DEADLINE: Duration = Duration::from_secs(15);

pub struct Gateway {
    pool: Arc<SubscriptionPoolRuntime>,
    http: reqwest::Client,
    upstream: String,
}

impl Gateway {
    pub fn new(
        pool: Arc<SubscriptionPoolRuntime>,
        upstream: impl Into<String>,
    ) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            pool,
            http,
            upstream: upstream.into().trim_end_matches('/').to_owned(),
        })
    }

    /// Handles a request whose path starts with [`LAUNCH_PREFIX`] (or a bare
    /// API path, which then uses the Claude Code session id for affinity).
    pub async fn handle(&self, request: Request<Incoming>) -> Response<Body> {
        match self.forward(request).await {
            Ok(response) => response,
            Err(error) => {
                warn!("subscription gateway request failed: {error}");
                error_response(
                    StatusCode::BAD_GATEWAY,
                    "api_error",
                    "Claude subscription gateway could not reach Anthropic",
                )
            }
        }
    }

    async fn forward(&self, request: Request<Incoming>) -> anyhow::Result<Response<Body>> {
        let (parts, body) = request.into_parts();
        let path_and_query = parts
            .uri
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/");
        let Some((launch_from_path, api_path)) = split_launch(path_and_query) else {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid launch id in subscription gateway path",
            ));
        };
        let launch_id = launch_from_path
            .map(str::to_owned)
            .or_else(|| {
                parts
                    .headers
                    .get(SESSION_ID_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .filter(|value| is_valid_launch_id(value))
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        let body = match http_body_util::Limited::new(body, MAX_REQUEST_BYTES)
            .collect()
            .await
        {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                return Ok(error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "invalid_request_error",
                    "request body could not be read",
                ));
            }
        };
        // Dot segments would be normalized by the HTTP client after routing
        // was decided on the raw path; refuse them instead of guessing.
        let path = api_path.split('?').next().unwrap_or(api_path);
        if path
            .split('/')
            .any(|segment| segment == "." || segment == ".." || segment.contains('%'))
        {
            return Ok(error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "unsupported path in subscription gateway request",
            ));
        }
        let upstream_url = format!("{}{}", self.upstream, api_path);
        let method = reqwest::Method::from_bytes(parts.method.as_str().as_bytes())?;
        let requested_model = if parts.method == Method::POST && path == "/v1/messages" {
            // A pooled request is never billed to the client's own account.
            let Some(model) = requested_model(&body) else {
                return Ok(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "subscription gateway requires an uncompressed JSON body with a model",
                ));
            };
            Some(model)
        } else {
            None
        };
        match requested_model {
            Some(model) => {
                self.forward_pooled(PooledRequest {
                    launch_id: &launch_id,
                    requested_model: &model,
                    headers: &parts.headers,
                    method,
                    upstream_url: &upstream_url,
                    body,
                })
                .await
            }
            None => {
                let mut outbound = self.http.request(method, &upstream_url).body(body);
                outbound = copy_headers(outbound, &parts.headers, false);
                Ok(stream_response(outbound.send().await?))
            }
        }
    }

    async fn forward_pooled(&self, request: PooledRequest<'_>) -> anyhow::Result<Response<Body>> {
        let pool = &self.pool;
        let mut excluded = HashSet::new();
        let mut final_quota = None;
        let mut final_entitlement = None;
        let mut auth_failed = false;

        while excluded.len() < pool.account_count() {
            let mut selected = match pool
                .select(request.launch_id, request.requested_model, &excluded)
                .await
            {
                Ok(selected) => selected,
                Err(error) => {
                    if let Some(response) = final_quota.or(final_entitlement) {
                        return Ok(buffered_response(response));
                    }
                    warn!(
                        "subscription pool={} has no eligible account for model={}: {error}",
                        pool.pool_id(),
                        request.requested_model
                    );
                    return Ok(match &error {
                        _ if auth_failed => credentials_unavailable(pool.pool_id()),
                        SubscriptionRuntimeError::NoUsableCredentials { .. } => {
                            credentials_unavailable(pool.pool_id())
                        }
                        SubscriptionRuntimeError::NoEligibleAccount { detail, .. } => {
                            exhausted(pool.pool_id(), request.requested_model, Some(detail))
                        }
                        _ => exhausted(pool.pool_id(), request.requested_model, None),
                    });
                }
            };
            let account_id = selected.account_id.clone();
            debug!(
                "subscription placement pool={} account={} model={} reason={:?}",
                pool.pool_id(),
                account_id,
                selected.model,
                selected.assignment_reason
            );
            let mut access_token = selected.access_token.clone();
            let mut refreshed_after_unauthorized = false;

            loop {
                let mut outbound = self
                    .http
                    .request(request.method.clone(), request.upstream_url)
                    .body(request.body.clone());
                outbound = copy_headers(outbound, request.headers, true)
                    // Error bodies are classified; keep them uncompressed.
                    .header(header::ACCEPT_ENCODING, "identity")
                    .bearer_auth(access_token.expose());

                let observation = pool.begin_observation(&account_id)?;
                // A network failure may happen after Anthropic accepted the
                // request, so it is never replayed against another account.
                let response = outbound.send().await?;
                let status = response.status();
                let headers = header_snapshot(response.headers());

                if status.is_success() {
                    selected.accept();
                    if let Err(error) = pool
                        .observe_response_headers(
                            &account_id,
                            request.requested_model,
                            &headers,
                            observation,
                        )
                        .await
                    {
                        warn!(
                            "subscription pool={} account={account_id} observation failed: {error}",
                            pool.pool_id()
                        );
                    }
                    return Ok(stream_response(response));
                }

                let Some(buffered) = buffer_error(response).await else {
                    return Ok(error_response(
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                        "api_error",
                        "Anthropic error response could not be read",
                    ));
                };
                if serde_json::from_slice::<Value>(&buffered.body).is_err() {
                    return Ok(buffered_response(buffered));
                }
                match classify_response(status.as_u16(), &headers, Some(&buffered.body)) {
                    ResponseClassification::RefreshCredential if !refreshed_after_unauthorized => {
                        refreshed_after_unauthorized = true;
                        match pool
                            .refresh_after_unauthorized(&account_id, &access_token)
                            .await
                        {
                            Ok(refreshed) => {
                                access_token = refreshed;
                                continue;
                            }
                            Err(error) => {
                                warn!(
                                    "subscription pool={} account={account_id} refresh failed: {error}",
                                    pool.pool_id()
                                );
                                if !matches!(
                                    &error,
                                    SubscriptionRuntimeError::Refresh(
                                        OAuthRefreshError::InvalidGrant
                                    )
                                ) && !pool
                                    .corroborate_credential_rejection(&account_id, &access_token)
                                    .await?
                                {
                                    return Ok(buffered_response(buffered));
                                }
                                auth_failed = true;
                            }
                        }
                    }
                    ResponseClassification::RefreshCredential => {
                        if !pool
                            .corroborate_credential_rejection(&account_id, &access_token)
                            .await?
                        {
                            return Ok(buffered_response(buffered));
                        }
                        auth_failed = true;
                    }
                    ResponseClassification::FailoverQuota { .. } => {
                        // Only a classified quota rejection may mark claims
                        // exhausted; transient 429s must not poison snapshots.
                        if let Err(error) = pool
                            .record_quota_rejection(
                                &account_id,
                                request.requested_model,
                                &headers,
                                observation,
                            )
                            .await
                        {
                            warn!(
                                "subscription pool={} account={account_id} quota observation failed: {error}",
                                pool.pool_id()
                            );
                        }
                        final_quota = Some(buffered);
                    }
                    ResponseClassification::EntitlementRejected { .. } => {
                        if let Err(error) = pool
                            .mark_entitlement_unavailable(
                                &account_id,
                                request.requested_model,
                                observation,
                            )
                            .await
                        {
                            warn!(
                                "subscription pool={} account={account_id} entitlement observation failed: {error}",
                                pool.pool_id()
                            );
                        }
                        final_entitlement = Some(buffered);
                    }
                    ResponseClassification::TransientRateLimit
                    | ResponseClassification::ProviderOverloaded
                    | ResponseClassification::PassThrough
                    | ResponseClassification::Success => {
                        return Ok(buffered_response(buffered));
                    }
                }

                warn!(
                    "subscription pool={} account={account_id} unavailable for model={}; trying another account",
                    pool.pool_id(),
                    request.requested_model
                );
                pool.clear_affinity(request.launch_id, request.requested_model, &account_id);
                excluded.insert(account_id.clone());
                break;
            }
        }

        Ok(match final_quota.or(final_entitlement) {
            Some(response) => buffered_response(response),
            None if auth_failed => credentials_unavailable(pool.pool_id()),
            None => exhausted(pool.pool_id(), request.requested_model, None),
        })
    }
}

struct PooledRequest<'a> {
    launch_id: &'a str,
    requested_model: &'a str,
    headers: &'a HeaderMap,
    method: reqwest::Method,
    upstream_url: &'a str,
    body: Bytes,
}

struct Buffered {
    status: reqwest::StatusCode,
    headers: reqwest::header::HeaderMap,
    body: Bytes,
}

/// Splits `/l/<launch-id>/rest` into the launch id and `/rest`. Paths without
/// the prefix pass through with no launch id; a malformed id is rejected.
pub fn split_launch(path_and_query: &str) -> Option<(Option<&str>, &str)> {
    let Some(rest) = path_and_query.strip_prefix(LAUNCH_PREFIX) else {
        return Some((None, path_and_query));
    };
    let (launch_id, api_path) = match rest.find(['/', '?']) {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    if !is_valid_launch_id(launch_id) {
        return None;
    }
    let api_path = if api_path.is_empty() { "/" } else { api_path };
    Some((Some(launch_id), api_path))
}

fn is_valid_launch_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn requested_model(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("model")?
        .as_str()
        .map(str::to_owned)
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

fn copy_headers(
    mut outbound: reqwest::RequestBuilder,
    headers: &HeaderMap,
    strip_auth: bool,
) -> reqwest::RequestBuilder {
    for (name, value) in headers {
        if is_hop_by_hop(name)
            || name == header::HOST
            || name == header::CONTENT_LENGTH
            || (strip_auth && (name == header::AUTHORIZATION || name.as_str() == "x-api-key"))
            || (strip_auth && name == header::ACCEPT_ENCODING)
        {
            continue;
        }
        outbound = outbound.header(name.as_str(), value.as_bytes());
    }
    outbound
}

fn header_snapshot(headers: &reqwest::header::HeaderMap) -> HeaderSnapshot {
    headers
        .iter()
        .filter_map(|(name, value)| {
            value
                .to_str()
                .ok()
                .map(|value| (name.as_str(), value.to_owned()))
        })
        .collect()
}

/// Reads a non-success body for classification. `None` when it is too large,
/// incomplete or slow; such a response is never classified or replayed.
async fn buffer_error(response: reqwest::Response) -> Option<Buffered> {
    let status = response.status();
    let headers = response.headers().clone();
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    let deadline = tokio::time::Instant::now() + ERROR_BODY_DEADLINE;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Ok(Some(Ok(chunk))) if body.len().saturating_add(chunk.len()) <= MAX_ERROR_BYTES => {
                body.extend_from_slice(&chunk);
            }
            Ok(None) => {
                return Some(Buffered {
                    status,
                    headers,
                    body: Bytes::from(body),
                });
            }
            _ => {
                warn!(
                    "subscription upstream HTTP {} error body unreadable; not replaying",
                    status.as_u16()
                );
                return None;
            }
        }
    }
}

fn response_head(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> hyper::http::response::Builder {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    for (name, value) in headers {
        let name = name.as_str();
        if matches!(
            name,
            "connection" | "keep-alive" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            builder = builder.header(name, value);
        }
    }
    builder
}

fn stream_response(response: reqwest::Response) -> Response<Body> {
    let builder = response_head(response.status(), response.headers());
    let stream = response
        .bytes_stream()
        .map_ok(Frame::data)
        .map_err(std::io::Error::other);
    builder
        .body(BodyExt::boxed(StreamBody::new(stream)))
        .unwrap_or_else(|_| internal_error())
}

fn buffered_response(buffered: Buffered) -> Response<Body> {
    response_head(buffered.status, &buffered.headers)
        .body(full(buffered.body))
        .unwrap_or_else(|_| internal_error())
}

pub fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into())
        .map_err(|never| match never {})
        .boxed()
}

pub fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = Response::new(full(value.to_string()));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    response
}

fn error_response(status: StatusCode, kind: &str, message: &str) -> Response<Body> {
    json_response(
        status,
        json!({"type": "error", "error": {"type": kind, "message": message}}),
    )
}

fn internal_error() -> Response<Body> {
    let mut response = Response::new(full("internal error"));
    *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    response
}

fn exhausted(pool_id: &str, requested_model: &str, detail: Option<&str>) -> Response<Body> {
    let message = match detail {
        Some(detail) => format!(
            "Claude subscription pool {pool_id:?} has no eligible account for model {requested_model:?}: {detail}"
        ),
        None => format!(
            "Claude subscription pool {pool_id:?} has no remaining included allowance for model {requested_model:?}"
        ),
    };
    error_response(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &message)
}

fn credentials_unavailable(pool_id: &str) -> Response<Body> {
    error_response(
        StatusCode::SERVICE_UNAVAILABLE,
        "api_error",
        &format!(
            "Claude subscription pool {pool_id:?} has no usable OAuth credential; run `csub status --verbose` for the account-specific error"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_the_launch_prefix_from_the_api_path() {
        assert_eq!(
            split_launch("/l/abc-1/v1/messages?beta=true"),
            Some((Some("abc-1"), "/v1/messages?beta=true"))
        );
        assert_eq!(split_launch("/l/abc"), Some((Some("abc"), "/")));
        assert_eq!(split_launch("/v1/messages"), Some((None, "/v1/messages")));
        assert_eq!(split_launch("/l//v1/messages"), None);
        assert_eq!(split_launch("/l/a%2Fb/v1/messages"), None);
    }

    #[test]
    fn reads_the_requested_model() {
        assert_eq!(
            requested_model(br#"{"model":"claude-opus-5-5","messages":[]}"#).as_deref(),
            Some("claude-opus-5-5")
        );
        assert_eq!(requested_model(b"not json"), None);
    }
}
