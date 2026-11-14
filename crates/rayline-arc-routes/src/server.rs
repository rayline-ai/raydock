//! The loopback router: `/v1/routes` answered through a [`Decider`], every
//! other path forwarded to the upstream.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures::TryStreamExt;
use http_body_util::{BodyExt, BodyStream, Full, Limited, StreamBody};
use hyper::body::{Body as _, Frame, Incoming};
use hyper::header::{AUTHORIZATION, CONTENT_TYPE, HOST, HeaderMap, HeaderValue, ORIGIN};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::catalog::Catalog;
use crate::error::Refusal;
use crate::parse::{ParseContext, RequestHeaders, parse};
use crate::shape::shape;

type Body = http_body_util::combinators::BoxBody<Bytes, io::Error>;

const ROUTE_ID_HEADER: &str = "x-rayline-route-id";
const CELL_HEADER: &str = "x-rayline-cell";

pub type DecideFuture<'a> = Pin<Box<dyn Future<Output = Result<Value, Refusal>> + Send + 'a>>;

/// The policy: one decide body in, one decide answer out. The body is a
/// [`crate::DecideInput`] as JSON, its request members as the harness sent them;
/// the answer carries `decision.tier_picks`, `decision.receipt`, `actions[]`
/// and `encoding.token_count`. A refusal is the route's.
pub trait Decider: Send + Sync {
    fn decide<'a>(&'a self, body: &'a str) -> DecideFuture<'a>;
}

#[derive(Debug)]
pub struct RouterError(String);

impl std::fmt::Display for RouterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RouterError {}

/// An origin (`scheme://host[:port]`) requests may be sent to: HTTPS, or
/// plain HTTP on loopback only.
fn origin(url: &str, what: &str) -> Result<String, RouterError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| RouterError(format!("{what} must be an http(s) origin")))?;
    let loopback = matches!(parsed.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
    let scheme_ok = parsed.scheme() == "https" || (parsed.scheme() == "http" && loopback);
    if !scheme_ok
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(RouterError(format!(
            "{what} must be an https origin (http on loopback only) with no path, query or credentials"
        )));
    }
    Ok(parsed.origin().ascii_serialization())
}

pub struct RouterConfig {
    pub catalog: Catalog,
    /// Where every path but `/v1/routes` is forwarded: an origin such as
    /// `https://api.example.com`.
    pub upstream: String,
    /// Mixed into every episode hash, so this process's episodes are its own.
    pub salt: String,
    /// The largest `/v1/routes` body read.
    pub max_body_bytes: usize,
    /// When set, every request must carry `Authorization: Bearer <token>`;
    /// others are refused 401. Set it whenever anything else can reach the
    /// port (any loopback listener can be reached by every local process).
    pub access_token: Option<String>,
    /// The `Authorization` value forwarded requests carry upstream in place
    /// of the caller's, when [`RouterConfig::access_token`] is set (the
    /// caller's header then holds the local token, not an upstream
    /// credential). `None` forwards without one.
    pub upstream_authorization: Option<String>,
}

impl RouterConfig {
    pub fn new(catalog: Catalog, upstream: &str) -> Self {
        Self {
            catalog,
            upstream: upstream.to_owned(),
            salt: String::new(),
            max_body_bytes: 64 << 20,
            access_token: None,
            upstream_authorization: None,
        }
    }
}

pub struct Router {
    catalog: Catalog,
    decider: Arc<dyn Decider>,
    upstream: String,
    client: reqwest::Client,
    salt: String,
    max_body_bytes: usize,
    access_token: Option<String>,
    upstream_authorization: Option<HeaderValue>,
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    let mut response = Response::new(
        Full::new(Bytes::from(value.to_string()))
            .map_err(|never| match never {})
            .boxed(),
    );
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

fn refusal_response(refusal: &Refusal) -> Response<Body> {
    let status = StatusCode::from_u16(refusal.status).unwrap_or(StatusCode::BAD_GATEWAY);
    json_response(status, &refusal.body())
}

fn hop_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// `127.0.0.1`, `localhost` or `[::1]`, with or without a port.
fn loopback_host(host: &str) -> bool {
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map_or("", |(name, _)| name),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    };
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

/// Compares two secrets in time that depends on their lengths only.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

impl Router {
    pub fn new(config: RouterConfig, decider: Arc<dyn Decider>) -> Result<Self, RouterError> {
        // A redirect is the caller's to follow: the caller's credentials are
        // sent to the configured upstream only.
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| RouterError("the upstream client could not be built".to_owned()))?;
        Ok(Self {
            upstream: origin(&config.upstream, "the upstream")?,
            catalog: config.catalog,
            decider,
            client,
            salt: config.salt,
            max_body_bytes: config.max_body_bytes,
            access_token: config.access_token.filter(|token| !token.is_empty()),
            upstream_authorization: match config.upstream_authorization {
                Some(value) => Some(HeaderValue::from_str(&value).map_err(|_| {
                    RouterError("the upstream authorization is not a header value".to_owned())
                })?),
                None => None,
            },
        })
    }

    /// One `/v1/routes` lookup: the body as sent and its headers into the
    /// V1 answer, or the refusal.
    pub async fn route(&self, sent: &[u8], headers: &RequestHeaders) -> Result<Value, Refusal> {
        let started = Instant::now();
        let elapsed = || u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let result: Result<(Value, Option<String>), Refusal> = async {
            let parsed = parse(&self.catalog, sent, headers, &ParseContext::now(&self.salt))?;
            let decide = &parsed.decide;
            let body = serde_json::to_string(decide).map_err(|_| {
                Refusal::new("unavailable", "the decide request could not be built.")
            })?;
            let answer = self.decider.decide(&body).await?;
            let route_id = format!("rt_{:032x}", rand::random::<u128>());
            let out = shape(
                &self.catalog,
                &parsed.restate,
                &answer,
                &route_id,
                elapsed(),
            )?;
            Ok((out, decide.event.kind.clone()))
        }
        .await;
        match result {
            Ok((out, kind)) => {
                tracing::info!(
                    ms = elapsed(),
                    route_id = out["route_id"].as_str(),
                    model = out["model"].as_str(),
                    worker = out["worker"].as_str(),
                    thinking_level = out["thinking_level"].as_str(),
                    decided_by = %out["receipt"]["decided_by"],
                    kind = kind.as_deref(),
                    tokens = out["usage"]["encoded_input_tokens"].as_u64(),
                    "route"
                );
                Ok(out)
            }
            Err(refusal) => {
                // Status and code only: a refusal's message can quote the
                // request (an unknown key, say), which never goes to a log.
                tracing::info!(
                    ms = elapsed(),
                    status = refusal.status,
                    code = %refusal.code,
                    "route refused"
                );
                Err(refusal)
            }
        }
    }

    async fn routes(&self, request: Request<Incoming>) -> Response<Body> {
        let headers = RequestHeaders {
            conversation_id: header(request.headers(), "x-conversation-id"),
            branch_id: header(request.headers(), "x-rayline-branch-id"),
            wire_format: header(request.headers(), "x-rayline-wire-format"),
            cell: header(request.headers(), CELL_HEADER),
        };
        let cell = request.headers().get(CELL_HEADER).cloned();
        let invalid = || Refusal::new("invalid_body", "The request body must be a JSON object.");
        let result = match Limited::new(request.into_body(), self.max_body_bytes)
            .collect()
            .await
        {
            Ok(body) => self.route(&body.to_bytes(), &headers).await,
            Err(error) if error.is::<http_body_util::LengthLimitError>() => Err(
                Refusal::with_status(413, "invalid_body", "The request body is too large."),
            ),
            Err(_) => Err(invalid()),
        };
        let mut response = match &result {
            Ok(answer) => {
                let mut response = json_response(StatusCode::OK, answer);
                let route_id = answer["route_id"].as_str().map(HeaderValue::from_str);
                if let Some(Ok(route_id)) = route_id {
                    response.headers_mut().insert(ROUTE_ID_HEADER, route_id);
                }
                response
            }
            Err(refusal) => refusal_response(refusal),
        };
        if let Some(cell) = cell {
            response.headers_mut().insert(CELL_HEADER, cell);
        }
        response
    }

    /// Everything but `/v1/routes`: sent to the upstream with the caller's own
    /// headers (its `Authorization` included), both bodies streamed.
    async fn forward(&self, request: Request<Incoming>) -> Response<Body> {
        let started = Instant::now();
        let method = request.method().clone();
        let path = request.uri().path().to_owned();
        let target = format!(
            "{}{}",
            self.upstream,
            request
                .uri()
                .path_and_query()
                .map_or("/", |target| target.as_str())
        );
        let mut headers = request.headers().clone();
        let hop: Vec<_> = headers
            .keys()
            .filter(|name| hop_header(name.as_str()))
            .cloned()
            .collect();
        for name in hop {
            headers.remove(name);
        }
        headers.remove(HOST);
        if self.access_token.is_some() {
            headers.remove(AUTHORIZATION);
            if let Some(value) = &self.upstream_authorization {
                headers.insert(AUTHORIZATION, value.clone());
            }
        }
        let mut upstream = self.client.request(method.clone(), target).headers(headers);
        if !request.body().is_end_stream() {
            let body = BodyStream::new(request.into_body())
                .try_filter_map(|frame| async move { Ok(frame.into_data().ok()) });
            upstream = upstream.body(reqwest::Body::wrap_stream(body));
        }
        let upstream = match upstream.send().await {
            Ok(upstream) => upstream,
            Err(error) => {
                // The error's own text names the full URL, query included.
                let reason = if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connect"
                } else {
                    "request"
                };
                tracing::warn!(%method, path, reason, "forward failed");
                return refusal_response(&Refusal::with_status(
                    502,
                    "unavailable",
                    "the upstream could not be reached.",
                ));
            }
        };
        let status = upstream.status();
        let mut response = Response::builder().status(status);
        for (name, value) in upstream.headers() {
            if !hop_header(name.as_str()) {
                response = response.header(name, value);
            }
        }
        // Direct stream: a caller that goes away drops the upstream response.
        let stream = upstream
            .bytes_stream()
            .map_ok(Frame::data)
            .map_err(io::Error::other);
        tracing::info!(
            %method,
            path,
            status = status.as_u16(),
            ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "forward"
        );
        response
            .body(BodyExt::boxed(StreamBody::new(stream)))
            .unwrap_or_else(|_| {
                refusal_response(&Refusal::with_status(
                    502,
                    "inconsistent_upstream",
                    "the upstream answer could not be relayed.",
                ))
            })
    }

    /// One request: `POST /v1/routes` decided here, `GET /health` answered
    /// here, anything else forwarded.
    pub async fn handle(&self, request: Request<Incoming>) -> Response<Body> {
        if let Err(refusal) = self.admit(request.headers()) {
            return refusal_response(&refusal);
        }
        match (request.method(), request.uri().path()) {
            (&Method::POST, "/v1/routes") => self.routes(request).await,
            (&Method::GET, "/health") => json_response(
                StatusCode::OK,
                &json!({"status": "ok", "package": {
                    "alias": self.catalog.alias, "package_sha256": self.catalog.package_sha256}}),
            ),
            _ => self.forward(request).await,
        }
    }

    /// Who may ask. A browser page must never reach this service: a request
    /// naming any host but loopback (DNS rebinding) or carrying an `Origin`
    /// (cross-site fetch) is refused. With an access token, the caller must
    /// also present it.
    fn admit(&self, headers: &HeaderMap) -> Result<(), Refusal> {
        let forbidden = |message: &str| Refusal::with_status(403, "forbidden", message);
        let host = headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if !loopback_host(host) {
            return Err(forbidden("This service answers loopback hosts only."));
        }
        if headers.contains_key(ORIGIN) {
            return Err(forbidden("This service does not answer browsers."));
        }
        if let Some(token) = &self.access_token {
            let presented = headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.strip_prefix("Bearer "))
                .unwrap_or_default();
            if !same_secret(presented.as_bytes(), token.as_bytes()) {
                return Err(Refusal::with_status(
                    401,
                    "unauthorized",
                    "A valid local access token is required.",
                ));
            }
        }
        Ok(())
    }

    /// Serves connections until the listener fails. Bind the listener to
    /// loopback, and set an access token unless every local process may ask.
    pub async fn serve(self: Arc<Self>, listener: TcpListener) -> io::Result<()> {
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (socket, _) = accepted?;
                    let router = self.clone();
                    connections.spawn(async move {
                        let service = service_fn(move |request| {
                            let router = router.clone();
                            async move { Ok::<_, Infallible>(router.handle(request).await) }
                        });
                        if let Err(error) = auto::Builder::new(TokioExecutor::new())
                            .serve_connection(TokioIo::new(socket), service)
                            .await
                        {
                            tracing::debug!(%error, "client connection closed");
                        }
                    });
                }
                _ = connections.join_next(), if !connections.is_empty() => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_with_or_without_a_port() {
        for host in [
            "127.0.0.1:9",
            "localhost",
            "LOCALHOST:80",
            "[::1]:5",
            "127.0.0.1",
        ] {
            assert!(loopback_host(host), "{host}");
        }
        for host in [
            "",
            "evil.example",
            "127.0.0.1.evil.example:80",
            "[::2]:5",
            "0.0.0.0:1",
        ] {
            assert!(!loopback_host(host), "{host}");
        }
    }

    #[test]
    fn secrets_match_only_exactly() {
        assert!(same_secret(b"abc", b"abc"));
        assert!(!same_secret(b"abc", b"abd"));
        assert!(!same_secret(b"abc", b"abcd"));
        assert!(!same_secret(b"", b"a"));
    }

    #[test]
    fn an_origin_is_https_or_loopback_http_and_nothing_more() {
        assert_eq!(
            origin("https://api.example.com/", "x").unwrap(),
            "https://api.example.com"
        );
        assert_eq!(
            origin("http://127.0.0.1:8080", "x").unwrap(),
            "http://127.0.0.1:8080"
        );
        for refused in [
            "http://api.example.com",
            "https://api.example.com/v1",
            "https://api.example.com/?a=1",
            "https://user:secret@api.example.com",
            "ftp://127.0.0.1",
            "api.example.com",
        ] {
            assert!(origin(refused, "x").is_err(), "{refused}");
        }
    }
}
