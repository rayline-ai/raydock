use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{CredentialDocument, CredentialError};

pub const DEFAULT_CLAUDE_TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub const DEFAULT_CLAUDE_OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

const MAX_REFRESH_RESPONSE_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct OAuthRefreshClient {
    http: reqwest::Client,
    token_url: String,
    client_id: String,
}

impl OAuthRefreshClient {
    pub fn new(
        http: reqwest::Client,
        token_url: impl Into<String>,
        client_id: impl Into<String>,
    ) -> Self {
        Self {
            http,
            token_url: token_url.into(),
            client_id: client_id.into(),
        }
    }

    pub(crate) async fn refresh_document(
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
            .post(&self.token_url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&request)
            .send()
            .await
            .map_err(OAuthRefreshError::Request)?;
        let status = response.status();
        let bytes = read_bounded_response(response, MAX_REFRESH_RESPONSE_BYTES).await?;
        if !status.is_success() {
            let error_code = serde_json::from_slice::<RefreshErrorResponse>(&bytes)
                .ok()
                .and_then(|error| error.error);
            if error_code.as_deref() == Some("invalid_grant") {
                return Err(OAuthRefreshError::InvalidGrant);
            }
            return Err(OAuthRefreshError::HttpStatus(status));
        }

        let refreshed: RefreshResponse =
            serde_json::from_slice(&bytes).map_err(OAuthRefreshError::InvalidResponse)?;
        if refreshed.access_token.is_empty() || refreshed.expires_in <= 0 {
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
            .field("token_url", &self.token_url)
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
    #[error("Claude OAuth credential cannot be refreshed: {0}")]
    Credential(#[from] CredentialError),
    #[error("Claude OAuth credential has no scopes")]
    MissingScopes,
    #[error("Claude OAuth refresh request failed: {0}")]
    Request(#[source] reqwest::Error),
    #[error("Claude OAuth refresh token is no longer valid; sign in to that profile again")]
    InvalidGrant,
    #[error("Claude OAuth refresh returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("Claude OAuth refresh response exceeded the size limit")]
    ResponseTooLarge,
    #[error("Claude OAuth refresh response was invalid: {0}")]
    InvalidResponse(#[source] serde_json::Error),
    #[error("Claude OAuth refresh response omitted a usable access token or expiry")]
    IncompleteResponse,
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

    #[test]
    fn parses_space_separated_response_scopes() {
        assert_eq!(
            parse_scopes("user:inference  user:profile "),
            vec!["user:inference", "user:profile"]
        );
    }
}
