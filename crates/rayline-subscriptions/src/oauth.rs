use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CredentialDocument, CredentialError};

pub const DEFAULT_CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const DEFAULT_CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

const MAX_REFRESH_RESPONSE_BYTES: usize = 1024 * 1024;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
pub struct OAuthRefreshClient {
    http: reqwest::Client,
    token_url: reqwest::Url,
    client_id: String,
}

impl OAuthRefreshClient {
    pub fn new(
        http: reqwest::Client,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Result<Self, OAuthRefreshError> {
        let token_url = reqwest::Url::parse(&token_url.into())
            .map_err(|_| OAuthRefreshError::InvalidTokenUrl)?;
        if token_url.scheme() != "https" {
            return Err(OAuthRefreshError::InsecureTokenUrl);
        }
        Ok(Self {
            http,
            token_url,
            client_id: client_id.into(),
        })
    }

    pub(crate) async fn refresh_document(
        &self,
        document: &mut CredentialDocument,
    ) -> Result<(), OAuthRefreshError> {
        tokio::time::timeout(REFRESH_TIMEOUT, self.refresh_document_inner(document))
            .await
            .map_err(|_| OAuthRefreshError::Timeout)?
    }

    async fn refresh_document_inner(
        &self,
        document: &mut CredentialDocument,
    ) -> Result<(), OAuthRefreshError> {
        let refresh_token = document.refresh_token()?;
        let scopes = document.scopes();
        if scopes.is_empty() {
            return Err(OAuthRefreshError::MissingScopes);
        }
        let request = RefreshRequest {
            grant_type: "refresh_token",
            refresh_token: refresh_token.expose(),
            client_id: &self.client_id,
            scope: scopes.join(" "),
        };
        let response = self
            .http
            .post(self.token_url.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(OAuthRefreshError::Request)?;
        let status = response.status();
        let bytes = zeroize::Zeroizing::new(
            read_bounded_response(response, MAX_REFRESH_RESPONSE_BYTES).await?,
        );
        if !status.is_success() {
            return Err(classify_refresh_rejection(status, &bytes));
        }

        let refreshed: RefreshResponse =
            serde_json::from_slice(&bytes).map_err(sanitized_response_error)?;
        if !usable_bearer_token(&refreshed.access_token)
            || refreshed.expires_in <= 0
            || refreshed.refresh_token.as_deref().is_some_and(|token| {
                token.trim().is_empty() || token.bytes().any(|byte| byte.is_ascii_control())
            })
            || refreshed
                .scope
                .as_deref()
                .is_some_and(|scope| parse_scopes(scope).is_empty())
        {
            return Err(OAuthRefreshError::IncompleteResponse);
        }
        let now_ms = unix_now_ms();
        let expires_at = now_ms.saturating_add(refreshed.expires_in.saturating_mul(1000));
        let refresh_token_expires_at = refreshed
            .refresh_token_expires_in
            .filter(|seconds| *seconds > 0)
            .map(|seconds| now_ms.saturating_add(seconds.saturating_mul(1000)));
        let response_scopes = refreshed.scope.as_deref().map(parse_scopes);

        document.apply_refresh(
            refreshed.access_token,
            refreshed.refresh_token,
            expires_at,
            refresh_token_expires_at,
            response_scopes,
        )?;
        Ok(())
    }
}

async fn read_bounded_response(
    response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, OAuthRefreshError> {
    use futures::StreamExt as _;

    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(OAuthRefreshError::ResponseTooLarge);
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(OAuthRefreshError::Request)?;
        if bytes.len().saturating_add(chunk.len()) > limit {
            return Err(OAuthRefreshError::ResponseTooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

impl std::fmt::Debug for OAuthRefreshClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthRefreshClient")
            .field("client_id", &self.client_id)
            .finish_non_exhaustive()
    }
}

#[derive(Serialize)]
struct RefreshRequest<'a> {
    grant_type: &'static str,
    refresh_token: &'a str,
    client_id: &'a str,
    scope: String,
}

#[derive(Deserialize)]
struct RefreshResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    refresh_token_expires_in: Option<i64>,
    scope: Option<String>,
}

#[derive(Deserialize)]
struct RefreshErrorResponse {
    error: Option<String>,
}

#[derive(Debug, Error)]
pub enum OAuthRefreshError {
    #[error("Claude OAuth token URL is invalid")]
    InvalidTokenUrl,
    #[error("Claude OAuth token URL must use HTTPS")]
    InsecureTokenUrl,
    #[error("Claude OAuth credential cannot be refreshed: {0}")]
    Credential(#[from] CredentialError),
    #[error("Claude OAuth credential has no scopes")]
    MissingScopes,
    #[error("Claude OAuth refresh request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("Claude OAuth refresh exceeded its deadline")]
    Timeout,
    #[error("Claude OAuth refresh token is no longer valid; sign in to that profile again")]
    InvalidGrant,
    #[error("Claude OAuth refresh returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("Claude OAuth refresh response exceeded the size limit")]
    ResponseTooLarge,
    #[error(
        "Claude OAuth refresh response was invalid ({category} at line {line}, column {column})"
    )]
    InvalidResponse {
        category: &'static str,
        line: usize,
        column: usize,
    },
    #[error("Claude OAuth refresh response omitted a usable access token or expiry")]
    IncompleteResponse,
}

impl OAuthRefreshError {
    /// Only a provider-confirmed rejected grant warrants quarantine. Transport,
    /// storage, throttling and malformed responses cannot prove a dead grant.
    pub(crate) fn is_definitive(&self) -> bool {
        matches!(self, Self::InvalidGrant)
    }
}

fn sanitized_response_error(error: serde_json::Error) -> OAuthRefreshError {
    let category = match error.classify() {
        serde_json::error::Category::Io => "I/O",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "EOF",
    };
    OAuthRefreshError::InvalidResponse {
        category,
        line: error.line(),
        column: error.column(),
    }
}

fn usable_bearer_token(token: &str) -> bool {
    // OAuth bearer credentials are printable ASCII without whitespace.
    // Validate borrowed bytes: never allocate a second header/token copy.
    !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_graphic())
}

fn classify_refresh_rejection(status: StatusCode, bytes: &[u8]) -> OAuthRefreshError {
    let error_code = serde_json::from_slice::<RefreshErrorResponse>(bytes)
        .ok()
        .and_then(|error| error.error);
    if matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED)
        && error_code.as_deref() == Some("invalid_grant")
    {
        OAuthRefreshError::InvalidGrant
    } else {
        OAuthRefreshError::HttpStatus(status)
    }
}

fn parse_scopes(value: &str) -> Vec<String> {
    value
        .split_ascii_whitespace()
        .filter(|scope| !scope.is_empty())
        .map(str::to_owned)
        .collect()
}

pub(crate) fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_success_preserves_credentials_and_sanitizes_errors() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        const SENTINEL: &str = "REFRESH_RESPONSE_SECRET_SENTINEL";
        let mut responses = vec![serde_json::json!({
            "access_token": "new-access", "refresh_token": "new-refresh", "expires_in": SENTINEL,
        }).to_string()];
        for access in [
            "bad\naccess",
            "bad\raccess",
            "bad\0access",
            " \t ",
            "bad access",
        ] {
            responses.push(
                serde_json::json!({
                    "access_token": access, "refresh_token": "new-refresh", "expires_in": 3600,
                })
                .to_string(),
            );
        }
        let cases = responses.len();
        let params =
            rcgen::CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
                .expect("certificate params");
        let key = rcgen::KeyPair::generate().expect("key");
        let certificate = params.self_signed(&key).expect("certificate");
        let config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![certificate.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .expect("server config");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener");
        let address = listener.local_addr().expect("address");
        let server = tokio::spawn(async move {
            for body in responses {
                let (stream, _) = listener.accept().await.expect("accept");
                let mut stream = acceptor.accept(stream).await.expect("TLS");
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let read = stream.read(&mut chunk).await.expect("request");
                    assert_ne!(read, 0, "complete request required");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(head_end) = request
                        .windows(4)
                        .position(|part| part == b"\r\n\r\n")
                        .map(|index| index + 4)
                    {
                        let head = std::str::from_utf8(&request[..head_end]).expect("head");
                        let length = head
                            .lines()
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().expect("length"))
                            })
                            .unwrap_or(0);
                        if request.len() >= head_end + length {
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body,
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
                stream.shutdown().await.expect("shutdown");
            }
        });
        let http = reqwest::Client::builder()
            .add_root_certificate(
                reqwest::Certificate::from_pem(certificate.pem().as_bytes()).expect("root"),
            )
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("client");
        let client = OAuthRefreshClient::new(
            http,
            format!("https://{address}/oauth/token"),
            DEFAULT_CLAUDE_OAUTH_CLIENT_ID,
        )
        .expect("OAuth client");
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join(".credentials.json");
        let original = serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "old-access", "refreshToken": "old-refresh",
                "expiresAt": 1000, "scopes": ["user:inference"],
            },
            "preserved": true,
        }))
        .expect("credential");
        let config_dir = dir.path().to_owned();
        let write_path = path.clone();
        let write_bytes = original.clone();
        let initial = tokio::task::spawn_blocking(move || {
            std::fs::write(write_path, write_bytes).expect("write");
            crate::CredentialStore::new(config_dir)
                .expect("store")
                .load()
                .expect("load")
        })
        .await
        .expect("load task");
        for index in 0..cases {
            let mut document = initial.clone();
            let error = client
                .refresh_document(&mut document)
                .await
                .expect_err("malformed success rejected");
            if index == 0 {
                assert!(matches!(
                    &error,
                    OAuthRefreshError::InvalidResponse {
                        category: "data",
                        ..
                    }
                ));
                assert!(std::error::Error::source(&error).is_none());
            } else {
                assert!(matches!(&error, OAuthRefreshError::IncompleteResponse));
            }
            assert!(!error.is_definitive());
            let mut diagnostics = format!("{error} {error:?}");
            let mut source = std::error::Error::source(&error);
            while let Some(error) = source {
                diagnostics.push_str(&format!(" {error} {error:?}"));
                source = error.source();
            }
            assert!(!diagnostics.contains(SENTINEL));
            assert!(!diagnostics.contains("new-refresh"));
            assert_eq!(
                document.access_token().expect("access").expose(),
                "old-access"
            );
            assert_eq!(
                document.refresh_token().expect("refresh").expose(),
                "old-refresh"
            );
            assert_eq!(document.expires_at_unix_ms().expect("expiry"), 1000);
            assert!(document.has_same_auth_version(&initial));
            let read_path = path.clone();
            assert_eq!(
                tokio::task::spawn_blocking(move || std::fs::read(read_path).expect("read"))
                    .await
                    .expect("read task"),
                original
            );
        }
        server.await.expect("fake endpoint served every case");
    }

    #[test]
    fn parses_space_separated_response_scopes() {
        assert_eq!(
            parse_scopes("user:inference  user:profile "),
            vec!["user:inference", "user:profile"]
        );
    }

    #[test]
    fn only_authoritative_invalid_grants_are_definitive() {
        let invalid_grant = br#"{"error":"invalid_grant"}"#;
        for status in [StatusCode::BAD_REQUEST, StatusCode::UNAUTHORIZED] {
            assert!(classify_refresh_rejection(status, invalid_grant).is_definitive());
        }
        for status in [
            StatusCode::TOO_MANY_REQUESTS,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::FORBIDDEN,
        ] {
            assert!(!classify_refresh_rejection(status, invalid_grant).is_definitive());
        }
        assert!(
            !classify_refresh_rejection(StatusCode::BAD_REQUEST, b"invalid_grant").is_definitive()
        );
        assert!(
            !classify_refresh_rejection(
                StatusCode::UNAUTHORIZED,
                br#"{"error":"temporarily_unavailable"}"#
            )
            .is_definitive()
        );
        assert!(!OAuthRefreshError::Timeout.is_definitive());
        assert!(!OAuthRefreshError::IncompleteResponse.is_definitive());
        assert!(!OAuthRefreshError::ResponseTooLarge.is_definitive());
        assert!(
            !OAuthRefreshError::Credential(CredentialError::MissingOAuthField("refreshToken"))
                .is_definitive()
        );
    }

    #[test]
    fn rejects_cleartext_refresh_endpoint() {
        let result = OAuthRefreshClient::new(
            reqwest::Client::new(),
            "http://127.0.0.1/oauth/token",
            DEFAULT_CLAUDE_OAUTH_CLIENT_ID,
        );
        assert!(matches!(result, Err(OAuthRefreshError::InsecureTokenUrl)));
    }
}
