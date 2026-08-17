//! Regression coverage for a quarantined subscription account.
//!
//! A refresh token the provider rejected with `invalid_grant` is dead. The
//! daemon must stop retrying it, but it must also notice when the credential
//! source receives a new document, because that is how a user signs in again.
//! Before this coverage existed, a quarantined account stayed quarantined for
//! the whole daemon lifetime.
//!
//! The fake endpoints are loopback only. Every credential document is a file in
//! a temporary directory, and the pool is given no home directory, so no test
//! can reach a real Claude profile or a real Keychain item.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rayline_subscriptions::{
    AccountCredentialReload, AccountRuntimeStatus, CredentialHealth, CredentialReloadSummary,
    CredentialSourceConfig, SubscriptionAccountConfig, SubscriptionPoolConfig,
    SubscriptionPoolRuntime, SubscriptionRuntimeOptions,
};
use rcgen::{CertificateParams, KeyPair};
use rustls::ServerConfig;
use rustls::pki_types::PrivateKeyDer;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

/// Well past the refresh margin, so no account holding it needs the OAuth
/// endpoint at all.
const FRESH_EXPIRY_MS: i64 = 4_000_000_000_000;
/// Long expired, so the first use of this credential must refresh.
const STALE_EXPIRY_MS: i64 = 1_000_000_000_000;

#[tokio::test]
async fn quarantined_account_recovers_when_the_credential_source_changes() {
    let pool = start_pool_with_a_quarantined_account().await;
    let refresh_attempts = pool.token.request_count();
    assert!(
        refresh_attempts >= 1,
        "the expired credential should have been refreshed once and rejected"
    );

    // Another process (a fresh sign-in) replaces the rejected credential.
    write_credential(&pool.broken_dir, "token-restored", FRESH_EXPIRY_MS);
    pool.runtime.refresh_all_usage().await;

    let broken = pool.account("broken");
    assert_eq!(broken.credential_health, CredentialHealth::Healthy);
    assert!(
        broken.usage_snapshot_fresh,
        "a healed account should report a fresh allowance snapshot"
    );
    assert_eq!(broken.last_error, None);
    assert_eq!(
        pool.token.request_count(),
        refresh_attempts,
        "a replaced credential is already valid and must not be refreshed"
    );
}

#[tokio::test]
async fn quarantined_account_never_retries_a_dead_refresh_token() {
    let pool = start_pool_with_a_quarantined_account().await;
    let refresh_attempts = pool.token.request_count();

    pool.runtime.refresh_all_usage().await;

    assert_eq!(
        pool.account("broken").credential_health,
        CredentialHealth::Quarantined
    );
    assert_eq!(
        pool.token.request_count(),
        refresh_attempts,
        "an unchanged credential source must not reach the OAuth endpoint again"
    );
}

#[tokio::test]
async fn reload_credentials_reports_an_unchanged_credential_source() {
    let pool = start_pool_with_a_quarantined_account().await;
    let refresh_attempts = pool.token.request_count();

    let summary = pool.runtime.reload_credentials().await;

    assert_eq!(summary.pool_id, "default");
    let healthy = reload_entry(&summary, "healthy");
    assert_eq!(healthy.previous_health, CredentialHealth::Healthy);
    assert_eq!(healthy.health, CredentialHealth::Healthy);
    assert_eq!(healthy.detail, "unchanged");

    let broken = reload_entry(&summary, "broken");
    assert_eq!(broken.previous_health, CredentialHealth::Quarantined);
    assert_eq!(
        broken.health,
        CredentialHealth::Quarantined,
        "an unchanged credential source cannot end a quarantine"
    );
    assert!(
        broken.detail.contains("sign in"),
        "the detail should tell the user what to do: {}",
        broken.detail
    );
    assert_eq!(
        pool.token.request_count(),
        refresh_attempts,
        "reloading must not retry a rejected refresh token"
    );

    let json = serde_json::to_string(&summary).expect("summary JSON");
    assert!(json.contains("\"previous_health\""));
    assert!(!json.contains("token-"), "a summary must carry no tokens");
}

#[tokio::test]
async fn reload_credentials_activates_a_replaced_credential_and_refreshes_usage() {
    let pool = start_pool_with_a_quarantined_account().await;
    let refresh_attempts = pool.token.request_count();
    write_credential(&pool.broken_dir, "token-restored", FRESH_EXPIRY_MS);

    let summary = pool.runtime.reload_credentials().await;

    let broken = reload_entry(&summary, "broken");
    assert_eq!(broken.previous_health, CredentialHealth::Quarantined);
    assert_eq!(broken.health, CredentialHealth::Healthy);
    assert!(
        broken.detail.contains("reloaded"),
        "the detail should say the credential was reloaded: {}",
        broken.detail
    );

    let status = pool.account("broken");
    assert_eq!(status.credential_health, CredentialHealth::Healthy);
    assert!(
        status.usage_snapshot_fresh,
        "a follow-up status call should show a fresh allowance"
    );
    assert_eq!(status.last_error, None);
    assert_eq!(
        pool.token.request_count(),
        refresh_attempts,
        "a replaced credential is already valid and must not be refreshed"
    );
}

#[tokio::test]
async fn reload_credentials_reports_a_credential_source_that_cannot_be_read() {
    let pool = start_pool_with_a_quarantined_account().await;
    std::fs::remove_file(pool.healthy_dir.join(".credentials.json")).expect("remove credential");

    let summary = pool.runtime.reload_credentials().await;

    let healthy = reload_entry(&summary, "healthy");
    assert_eq!(healthy.previous_health, CredentialHealth::Healthy);
    assert_eq!(
        healthy.health,
        CredentialHealth::Healthy,
        "an unreadable store must not evict an account whose token still works"
    );
    assert!(
        healthy.detail.contains("could not be read"),
        "the detail should explain the failure: {}",
        healthy.detail
    );
    assert!(
        pool.account("healthy")
            .last_error
            .is_some_and(|error| error.contains("could not be read")),
        "status should keep the read failure visible"
    );
}

fn reload_entry<'a>(summary: &'a CredentialReloadSummary, id: &str) -> &'a AccountCredentialReload {
    summary
        .accounts
        .iter()
        .find(|account| account.id == id)
        .expect("account reload entry")
}

/// A two-account pool where `healthy` keeps the pool usable and `broken` is
/// quarantined during startup: its credential is expired, and the fake token
/// endpoint rejects every refresh with `invalid_grant`.
struct TestPool {
    _temp: tempfile::TempDir,
    _usage: FakeEndpoint,
    token: FakeEndpoint,
    healthy_dir: PathBuf,
    broken_dir: PathBuf,
    runtime: Arc<SubscriptionPoolRuntime>,
}

impl TestPool {
    fn account(&self, id: &str) -> AccountRuntimeStatus {
        self.runtime
            .status()
            .accounts
            .into_iter()
            .find(|account| account.id == id)
            .expect("account status")
    }
}

async fn start_pool_with_a_quarantined_account() -> TestPool {
    let usage = spawn_plain_endpoint(http_response("200 OK", USAGE_BODY)).await;
    let token = spawn_tls_endpoint(http_response("400 Bad Request", INVALID_GRANT_BODY)).await;
    let temp = tempfile::tempdir().expect("tempdir");
    let control_dir = temp.path().join("control");
    let healthy_dir = temp.path().join("healthy");
    let broken_dir = temp.path().join("broken");
    std::fs::create_dir_all(&control_dir).expect("control dir");
    write_credential(&healthy_dir, "token-healthy", FRESH_EXPIRY_MS);
    write_credential(&broken_dir, "token-stale", STALE_EXPIRY_MS);

    let runtime = SubscriptionPoolRuntime::start(
        "default",
        SubscriptionPoolConfig {
            control_config_dir: control_dir,
            accounts: vec![
                account_config("healthy", &healthy_dir),
                account_config("broken", &broken_dir),
            ],
            policy: Default::default(),
        },
        SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", usage.port),
            token_url: format!("https://localhost:{}/v1/oauth/token", token.port),
            trusted_ca_pem: Some(token.cert_pem.as_bytes().to_vec()),
            request_timeout: Duration::from_secs(5),
            home_dir: None,
            ..Default::default()
        },
    )
    .await
    .expect("pool runtime");

    let pool = TestPool {
        _temp: temp,
        _usage: usage,
        token,
        healthy_dir,
        broken_dir,
        runtime,
    };
    assert_eq!(
        pool.account("broken").credential_health,
        CredentialHealth::Quarantined,
        "a rejected refresh token should quarantine the account"
    );
    pool
}

fn account_config(id: &str, config_dir: &Path) -> SubscriptionAccountConfig {
    SubscriptionAccountConfig {
        id: id.to_owned(),
        credential_source: CredentialSourceConfig {
            claude_config_dir: config_dir.to_owned(),
        },
    }
}

fn write_credential(config_dir: &Path, access_token: &str, expires_at_unix_ms: i64) {
    std::fs::create_dir_all(config_dir).expect("credential dir");
    let document = serde_json::to_vec(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access_token,
            "refreshToken": format!("refresh-{access_token}"),
            "expiresAt": expires_at_unix_ms,
            "scopes": ["user:inference", "user:profile"],
            "subscriptionType": "max"
        }
    }))
    .expect("credential JSON");
    std::fs::write(config_dir.join(".credentials.json"), document).expect("write credential");
}

const USAGE_BODY: &str = r#"{"five_hour":{"utilization":10},"seven_day":{"utilization":20},"extra_usage":{"is_enabled":false}}"#;
const INVALID_GRANT_BODY: &str = r#"{"error":"invalid_grant"}"#;

/// A loopback HTTP/1.1 endpoint that answers every request with one canned
/// response and counts the requests it completed. `connection: close` stops the
/// client from pooling connections, so every counted request is a round trip.
struct FakeEndpoint {
    port: u16,
    cert_pem: String,
    requests: Arc<AtomicUsize>,
}

impl FakeEndpoint {
    fn request_count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

async fn spawn_plain_endpoint(response: String) -> FakeEndpoint {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind plain");
    let port = listener.local_addr().expect("plain addr").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let response = response.clone();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move { serve_one(stream, response, counter).await });
        }
    });
    FakeEndpoint {
        port,
        cert_pem: String::new(),
        requests,
    }
}

async fn spawn_tls_endpoint(response: String) -> FakeEndpoint {
    let (config, cert_pem) = self_signed_server_config();
    let acceptor = TlsAcceptor::from(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind tls");
    let port = listener.local_addr().expect("tls addr").port();
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let response = response.clone();
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    serve_one(tls, response, counter).await;
                }
            });
        }
    });
    FakeEndpoint {
        port,
        cert_pem,
        requests,
    }
}

fn self_signed_server_config() -> (Arc<ServerConfig>, String) {
    let params = CertificateParams::new(vec!["localhost".to_owned(), "127.0.0.1".to_owned()])
        .expect("certificate params");
    let key_pair = KeyPair::generate().expect("key pair");
    let certificate = params
        .self_signed(&key_pair)
        .expect("self signed certificate");
    let certificate_pem = certificate.pem();
    let config =
        ServerConfig::builder_with_provider(rustls::crypto::aws_lc_rs::default_provider().into())
            .with_safe_default_protocol_versions()
            .expect("protocol versions")
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                PrivateKeyDer::Pkcs8(key_pair.serialize_der().into()),
            )
            .expect("server certificate");
    (Arc::new(config), certificate_pem)
}

async fn serve_one<S>(mut stream: S, response: String, requests: Arc<AtomicUsize>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    if read_request(&mut stream).await {
        requests.fetch_add(1, Ordering::SeqCst);
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.flush().await;
    }
    let _ = stream.shutdown().await;
}

/// Reads one whole request before replying, so the client never sees a closed
/// socket while it is still writing its body.
async fn read_request<S: AsyncRead + Unpin>(stream: &mut S) -> bool {
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return false,
            Ok(read) => request.extend_from_slice(&chunk[..read]),
        }
        if let Some(head_length) = head_length(&request)
            && request.len() >= head_length + content_length(&request[..head_length])
        {
            return true;
        }
    }
}

fn head_length(request: &[u8]) -> Option<usize> {
    request
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4)
}

fn content_length(head: &[u8]) -> usize {
    String::from_utf8_lossy(head)
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0)
}

fn http_response(status: &str, body: &str) -> String {
    format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}
