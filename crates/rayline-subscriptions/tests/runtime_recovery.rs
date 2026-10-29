use rayline_subscriptions::{
    CredentialHealth, CredentialSourceConfig, HeaderSnapshot, SubscriptionAccountConfig,
    SubscriptionPoolConfig, SubscriptionPoolRuntime, SubscriptionRuntimeOptions,
};
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpListener,
    sync::Semaphore,
};
use tokio_rustls::TlsAcceptor;

const USAGE: &str = r#"{"five_hour":{"utilization":10},"seven_day":{"utilization":20},"extra_usage":{"is_enabled":false}}"#;
const GRANT: &str =
    r#"{"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}"#;

struct Endpoint {
    port: u16,
    pem: String,
    count: Arc<AtomicUsize>,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    paused: Arc<AtomicBool>,
}
impl Endpoint {
    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.acquire())
            .await
            .expect("request deadline")
            .expect("request signal")
            .forget();
    }
    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.release.add_permits(32);
    }
}
async fn endpoint(tls: bool, body: &'static str, paused: bool) -> Endpoint {
    endpoint_with_followup(tls, body, None, paused).await
}
async fn endpoint_with_followup(
    tls: bool,
    body: &'static str,
    followup: Option<&'static str>,
    paused: bool,
) -> Endpoint {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback listener");
    let port = listener.local_addr().expect("addr").port();
    let count = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let paused = Arc::new(AtomicBool::new(paused));
    let (acceptor, pem) = if tls {
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = rcgen::CertificateParams::new(vec!["localhost".into()])
            .expect("params")
            .self_signed(&key)
            .expect("cert");
        let config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::aws_lc_rs::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(key.serialize_der().into()),
        )
        .expect("tls config");
        (Some(TlsAcceptor::from(Arc::new(config))), cert.pem())
    } else {
        (None, String::new())
    };
    let c = count.clone();
    let e = entered.clone();
    let r = release.clone();
    let p = paused.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let a = acceptor.clone();
            let c = c.clone();
            let e = e.clone();
            let r = r.clone();
            let p = p.clone();
            tokio::spawn(async move {
                if let Some(a) = a {
                    if let Ok(stream) = a.accept(stream).await {
                        serve(stream, body, followup, c, e, r, p).await;
                    }
                } else {
                    serve(stream, body, followup, c, e, r, p).await;
                }
            });
        }
    });
    Endpoint {
        port,
        pem,
        count,
        entered,
        release,
        paused,
    }
}
async fn serve<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    body: &str,
    followup: Option<&str>,
    count: Arc<AtomicUsize>,
    entered: Arc<Semaphore>,
    release: Arc<Semaphore>,
    paused: Arc<AtomicBool>,
) {
    let mut bytes = Vec::new();
    let mut buf = [0; 4096];
    loop {
        let Ok(n) = stream.read(&mut buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        bytes.extend_from_slice(&buf[..n]);
        if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            let len = String::from_utf8_lossy(&bytes[..end])
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= end + 4 + len {
                break;
            }
        }
    }
    // Reset discovery is unsupported by this quota/recovery fixture. Keep its
    // request sequence and barriers about quota reads, not optional discovery.
    if bytes.starts_with(b"GET /api/oauth/usage?") {
        let body = r#"{"cedar_ember":null,"juniper_tide":null}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
        return;
    }
    let index = count.fetch_add(1, Ordering::SeqCst);
    entered.add_permits(1);
    let body = if index > 0 {
        followup.unwrap_or(body)
    } else {
        body
    };
    if paused.load(Ordering::SeqCst) {
        release.acquire().await.expect("release").forget();
    }
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}
fn credentials(path: &Path) {
    std::fs::create_dir_all(path).expect("profile");
    std::fs::write(path.join(".credentials.json"), r#"{"claudeAiOauth":{"accessToken":"initial-access","refreshToken":"initial-refresh","expiresAt":4000000000000,"scopes":["user:inference","user:profile"]}}"#).expect("credentials");
}
async fn runtime(
    root: &Path,
    usage: &Endpoint,
    token: &Endpoint,
    persisted: bool,
) -> Arc<SubscriptionPoolRuntime> {
    runtime_with_backoff(root, usage, token, persisted, Duration::from_millis(1)).await
}
async fn runtime_with_backoff(
    root: &Path,
    usage: &Endpoint,
    token: &Endpoint,
    persisted: bool,
    recovery_backoff: Duration,
) -> Arc<SubscriptionPoolRuntime> {
    runtime_with_accounts(
        root,
        usage,
        token,
        persisted,
        recovery_backoff,
        &[("a", "profile")],
    )
    .await
}
async fn runtime_with_accounts(
    root: &Path,
    usage: &Endpoint,
    token: &Endpoint,
    persisted: bool,
    recovery_backoff: Duration,
    accounts: &[(&str, &str)],
) -> Arc<SubscriptionPoolRuntime> {
    let root = std::fs::canonicalize(root).expect("physical temp root");
    std::fs::create_dir_all(root.join("control")).expect("control");
    SubscriptionPoolRuntime::start(
        "pool",
        SubscriptionPoolConfig {
            control_config_dir: root.join("control"),
            accounts: accounts
                .iter()
                .map(|(id, profile)| SubscriptionAccountConfig {
                    id: (*id).into(),
                    credential_source: CredentialSourceConfig {
                        claude_config_dir: root.join(profile),
                    },
                })
                .collect(),
            policy: Default::default(),
        },
        SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", usage.port),
            token_url: format!("https://localhost:{}/token", token.port),
            trusted_ca_pem: Some(token.pem.as_bytes().to_vec()),
            home_dir: None,
            state_dir: persisted.then(|| root.join("state")),
            recovery_backoff,
            ..Default::default()
        },
    )
    .await
    .expect("runtime")
}

#[tokio::test]
async fn cancelled_refresh_retains_owned_transaction_and_single_account_singleflight() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let p = pool.clone();
    let first = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    let second = pool.force_refresh("a");
    tokio::pin!(second);
    assert!(futures::poll!(&mut second).is_pending());
    let p = pool.clone();
    let selection =
        tokio::spawn(async move { p.select("launch", "sonnet", &HashSet::new()).await });
    first.abort();
    token.resume();
    assert_eq!(second.await.expect("refresh").expose(), "rotated-access");
    assert_eq!(
        selection
            .await
            .expect("selection task")
            .expect("selection")
            .access_token
            .expose(),
        "rotated-access"
    );
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
    assert_eq!(
        pool.status().accounts[0].credential_health,
        CredentialHealth::Healthy
    );
    assert!(
        std::fs::read_to_string(temp.path().join("profile/.credentials.json"))
            .expect("saved")
            .contains("rotated-refresh")
    );
}

#[tokio::test]
async fn quiesce_stops_oauth_on_accounts_it_has_not_reached_yet() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    credentials(&temp.path().join("profile-b"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    let pool = runtime_with_accounts(
        temp.path(),
        &usage,
        &token,
        false,
        Duration::from_millis(1),
        &[("a", "profile"), ("b", "profile-b")],
    )
    .await;
    let p = pool.clone();
    let refresh = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    // Quiesce now waits on account a; b's gate is still open.
    let quiesce = pool.quiesce_credentials();
    tokio::pin!(quiesce);
    assert!(futures::poll!(&mut quiesce).is_pending());
    let late = tokio::time::timeout(Duration::from_secs(5), pool.force_refresh("b"))
        .await
        .expect("refused without waiting on OAuth");
    assert!(late.is_err(), "no OAuth on b once quiesce has begun");
    token.resume();
    refresh.await.expect("task").expect("a's refresh completes");
    assert!(quiesce.await.is_empty());
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quiesce_waits_for_an_in_flight_refresh_to_persist_and_refuses_new_ones() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let p = pool.clone();
    let refresh = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    // The requester goes away mid-refresh, as on shutdown.
    refresh.abort();
    let quiesce = pool.quiesce_credentials();
    tokio::pin!(quiesce);
    assert!(futures::poll!(&mut quiesce).is_pending());
    // A request still open after the drain queues a refresh behind it.
    let p = pool.clone();
    let late = tokio::spawn(async move { p.force_refresh("a").await });
    tokio::task::yield_now().await;
    token.resume();
    assert!(quiesce.await.is_empty());
    assert!(
        late.await.expect("late task").is_err(),
        "no OAuth after quiesce"
    );
    assert!(
        std::fs::read_to_string(temp.path().join("profile/.credentials.json"))
            .expect("saved")
            .contains("rotated-refresh")
    );
    assert!(pool.force_refresh("a").await.is_err());
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn peer_runtime_adopts_cross_process_owner_grant_without_second_refresh() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    let first = runtime(temp.path(), &usage, &token, false).await;
    let second = runtime(temp.path(), &usage, &token, false).await;
    let a = tokio::spawn(async move { first.force_refresh("a").await });
    token.wait().await;
    let b = tokio::spawn(async move { second.force_refresh("a").await });
    token.resume();
    assert_eq!(
        a.await.expect("owner").expect("grant").expose(),
        "rotated-access"
    );
    assert_eq!(
        b.await.expect("peer").expect("grant").expose(),
        "rotated-access"
    );
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persistence_failure_keeps_rotated_grant_and_retries_save_not_oauth() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    let original = std::fs::read(profile.join(".credentials.json")).expect("original");
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let p = pool.clone();
    let refresh = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    std::fs::remove_file(profile.join(".credentials.json")).expect("remove");
    std::fs::create_dir(profile.join(".credentials.json")).expect("obstruct save");
    token.resume();
    assert_eq!(
        refresh
            .await
            .expect("task")
            .expect("pending grant")
            .expose(),
        "rotated-access"
    );
    assert!(
        pool.status().accounts[0]
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("persistence pending"))
    );
    std::fs::remove_dir(profile.join(".credentials.json")).expect("remove obstruction");
    std::fs::write(profile.join(".credentials.json"), original).expect("restore source");
    tokio::time::sleep(Duration::from_millis(2)).await;
    pool.refresh_all_usage().await;
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
    assert!(
        std::fs::read_to_string(profile.join(".credentials.json"))
            .expect("saved")
            .contains("rotated-refresh")
    );
}

#[tokio::test]
async fn quiesce_reports_a_rotated_grant_it_still_cannot_save() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    // A long backoff: the shutdown save must not wait for it.
    let pool =
        runtime_with_backoff(temp.path(), &usage, &token, false, Duration::from_secs(600)).await;
    let p = pool.clone();
    let refresh = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    std::fs::remove_file(profile.join(".credentials.json")).expect("remove");
    std::fs::create_dir(profile.join(".credentials.json")).expect("obstruct save");
    token.resume();
    refresh.await.expect("task").expect("pending grant");

    // Still obstructed: reported, not lost silently, and no new OAuth call.
    assert_eq!(pool.quiesce_credentials().await, vec!["a".to_owned()]);
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quiesce_persists_a_pending_grant_inside_the_backoff_without_oauth() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    let original = std::fs::read(profile.join(".credentials.json")).expect("original");
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, true).await;
    let pool =
        runtime_with_backoff(temp.path(), &usage, &token, false, Duration::from_secs(600)).await;
    let p = pool.clone();
    let refresh = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    std::fs::remove_file(profile.join(".credentials.json")).expect("remove");
    std::fs::create_dir(profile.join(".credentials.json")).expect("obstruct save");
    token.resume();
    refresh.await.expect("task").expect("pending grant");
    std::fs::remove_dir(profile.join(".credentials.json")).expect("remove obstruction");
    std::fs::write(profile.join(".credentials.json"), original).expect("restore source");

    assert!(pool.quiesce_credentials().await.is_empty());
    assert_eq!(
        token.count.load(Ordering::SeqCst),
        1,
        "no second OAuth call"
    );
    assert!(
        std::fs::read_to_string(profile.join(".credentials.json"))
            .expect("saved")
            .contains("rotated-refresh")
    );
}

#[tokio::test]
async fn delayed_poll_cannot_erase_newer_quota_and_restart_retains_block() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, false).await;
    let pool = runtime(temp.path(), &usage, &token, true).await;
    // Drain startup request signals before establishing the delayed poll barrier.
    usage
        .entered
        .try_acquire()
        .expect("startup request")
        .forget();
    usage.paused.store(true, Ordering::SeqCst);
    let p = pool.clone();
    let poll = tokio::spawn(async move { p.refresh_all_usage().await });
    usage.wait().await;
    let observation = pool.begin_observation("a").expect("observation");
    pool.record_quota_rejection("a", "sonnet", &HeaderSnapshot::default(), observation)
        .await
        .expect("durable rejection");
    usage.resume();
    poll.await.expect("poll");
    assert!(
        pool.select("launch", "sonnet", &HashSet::new())
            .await
            .is_err()
    );
    let restarted = runtime(temp.path(), &usage, &token, true).await;
    assert!(
        restarted
            .select("launch", "sonnet", &HashSet::new())
            .await
            .is_err()
    );
    assert!(
        restarted
            .select("launch", "opus", &HashSet::new())
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn successful_usage_does_not_corroborate_inference_unauthorized() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, false).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let selected = pool
        .select("launch", "sonnet", &HashSet::new())
        .await
        .expect("selection");
    assert!(
        !pool
            .corroborate_credential_rejection("a", &selected.access_token)
            .await
            .expect("evidence")
    );
    assert_eq!(
        pool.status().accounts[0].credential_health,
        CredentialHealth::Healthy
    );
    assert_eq!(token.count.load(Ordering::SeqCst), 0);
}

fn set_expiry(profile: &Path, delta_ms: i64) {
    let path = profile.join(".credentials.json");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).expect("credential bytes")).expect("document");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis() as i64;
    document["claudeAiOauth"]["expiresAt"] = (now + delta_ms).into();
    std::fs::write(path, serde_json::to_vec(&document).expect("json")).expect("expiry update");
}

#[tokio::test]
async fn temporary_early_refresh_uses_only_unexpired_fallback_and_backs_off() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    set_expiry(&profile, 60_000);
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, r#"{"error":"temporarily_unavailable"}"#, false).await;
    let pool = SubscriptionPoolRuntime::start(
        "pool",
        SubscriptionPoolConfig {
            control_config_dir: temp.path().join("control"),
            accounts: vec![SubscriptionAccountConfig {
                id: "a".into(),
                credential_source: CredentialSourceConfig {
                    claude_config_dir: profile,
                },
            }],
            policy: Default::default(),
        },
        SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", usage.port),
            token_url: format!("https://localhost:{}/token", token.port),
            trusted_ca_pem: Some(token.pem.as_bytes().to_vec()),
            home_dir: None,
            recovery_backoff: Duration::from_secs(60),
            ..Default::default()
        },
    )
    .await
    .expect("runtime");
    let selected = pool
        .select("launch", "sonnet", &HashSet::new())
        .await
        .expect("unexpired fallback");
    assert_eq!(selected.access_token.expose(), "initial-access");
    assert_eq!(token.count.load(Ordering::SeqCst), 1);
    assert_eq!(
        pool.status().accounts[0].credential_health,
        CredentialHealth::Healthy
    );
}

#[tokio::test]
async fn temporary_refresh_never_serves_expired_fallback() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    set_expiry(&profile, -1000);
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, r#"{"error":"temporarily_unavailable"}"#, false).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    assert_eq!(
        pool.status().accounts[0].credential_health,
        CredentialHealth::Unavailable
    );
    assert!(
        pool.select("launch", "sonnet", &HashSet::new())
            .await
            .is_err()
    );
    assert_eq!(usage.count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn provisional_selection_is_not_serving_until_accepted_and_drop_rolls_back() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, false).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let selected = pool
        .select("launch", "sonnet", &HashSet::new())
        .await
        .expect("provisional");
    assert_eq!(
        pool.status().placement.expect("placement").accounts[0].primary_assignments,
        0
    );
    drop(selected);
    assert_eq!(
        pool.status().placement.expect("placement").accounts[0].active_launch_leases,
        0
    );
    let mut selected = pool
        .select("launch", "sonnet", &HashSet::new())
        .await
        .expect("selection");
    selected.accept();
    assert_eq!(
        pool.status().placement.expect("placement").accounts[0].primary_assignments,
        1
    );
}

#[tokio::test]
async fn read_only_start_never_polls_refreshes_creates_locks_or_recovery_files() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    set_expiry(&profile, -1000);
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, false).await;
    let pool = SubscriptionPoolRuntime::start(
        "pool",
        SubscriptionPoolConfig {
            control_config_dir: temp.path().join("control"),
            accounts: vec![SubscriptionAccountConfig {
                id: "a".into(),
                credential_source: CredentialSourceConfig {
                    claude_config_dir: profile.clone(),
                },
            }],
            policy: Default::default(),
        },
        SubscriptionRuntimeOptions {
            anthropic_base_url: format!("http://127.0.0.1:{}", usage.port),
            token_url: format!("https://localhost:{}/token", token.port),
            trusted_ca_pem: Some(token.pem.as_bytes().to_vec()),
            home_dir: None,
            read_only: true,
            state_dir: Some(temp.path().join("state")),
            ..Default::default()
        },
    )
    .await
    .expect("read-only runtime");
    assert!(pool.spawn_monitor().is_none());
    pool.refresh_all_usage().await;
    assert!(pool.force_refresh("a").await.is_err());
    assert_eq!(usage.count.load(Ordering::SeqCst), 0);
    assert_eq!(token.count.load(Ordering::SeqCst), 0);
    assert!(!profile.join(".credentials.json.rayline.lock").exists());
    assert!(!temp.path().join("state").exists());
    let status = pool.status();
    assert_eq!(
        status.accounts[0].credential_health,
        CredentialHealth::Unavailable
    );
    assert!(!status.accounts[0].usage_snapshot_fresh);
    assert!(status.accounts[0].claims.is_empty());
}

#[tokio::test]
async fn read_only_usage_poll_measures_capacity_without_writing_or_refreshing() {
    const EXHAUSTED_USAGE: &str = r#"{"five_hour":{"utilization":10},"seven_day":{"utilization":100,"resets_at":"2090-01-01T00:00:00Z"}}"#;
    for (expired, body, exhausted) in [
        (false, USAGE, false),
        (false, EXHAUSTED_USAGE, true),
        (true, USAGE, false),
    ] {
        let temp = tempfile::tempdir().expect("temp");
        let profile = temp.path().join("profile");
        credentials(&profile);
        if expired {
            set_expiry(&profile, -1000);
        }
        let path = profile.join(".credentials.json");
        let original = std::fs::read(&path).expect("credential bytes");
        let modified = std::fs::metadata(&path)
            .expect("metadata")
            .modified()
            .expect("mtime");
        let usage = endpoint(false, body, false).await;
        let token = endpoint(true, GRANT, false).await;
        let pool = SubscriptionPoolRuntime::start(
            "pool",
            SubscriptionPoolConfig {
                control_config_dir: temp.path().join("control"),
                accounts: vec![SubscriptionAccountConfig {
                    id: "a".into(),
                    credential_source: CredentialSourceConfig {
                        claude_config_dir: profile.clone(),
                    },
                }],
                policy: Default::default(),
            },
            SubscriptionRuntimeOptions {
                anthropic_base_url: format!("http://127.0.0.1:{}", usage.port),
                token_url: format!("https://localhost:{}/token", token.port),
                trusted_ca_pem: Some(token.pem.as_bytes().to_vec()),
                home_dir: None,
                read_only: true,
                poll_usage_read_only: true,
                state_dir: Some(temp.path().join("state")),
                ..Default::default()
            },
        )
        .await
        .expect("read-only usage runtime");
        let status = pool.status_without_live_placement();
        let account = &status.accounts[0];
        assert_eq!(account.usage_snapshot_fresh, !expired);
        assert_eq!(account.complete_global_snapshot, !expired);
        if !expired {
            assert_eq!(account.credential_health, CredentialHealth::Healthy);
            let weekly = account
                .claims
                .iter()
                .find(|claim| claim.key == "seven_day")
                .expect("weekly claim");
            if exhausted {
                assert!(weekly.is_hard_exhausted());
                assert!(
                    weekly.resets_at.is_some(),
                    "spent quota must retain its known reset"
                );
            } else {
                assert_eq!(weekly.utilization, Some(0.2));
            }
        } else {
            assert!(account.claims.is_empty());
        }
        assert!(status.placement.is_none());
        assert!(pool.spawn_monitor().is_none());
        assert_eq!(usage.count.load(Ordering::SeqCst), usize::from(!expired));
        assert_eq!(token.count.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(&path).expect("credential after polling"),
            original
        );
        assert_eq!(
            std::fs::metadata(&path)
                .expect("metadata")
                .modified()
                .expect("mtime"),
            modified
        );
        assert!(!profile.join(".credentials.json.rayline.lock").exists());
        assert!(!temp.path().join("control").exists());
        assert!(!temp.path().join("state").exists());
    }
}

#[tokio::test]
async fn cached_healthy_selection_does_not_reload_missing_source_or_send_network_requests() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, false).await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let initial_usage = usage.count.load(Ordering::SeqCst);
    std::fs::remove_file(profile.join(".credentials.json")).expect("remove cached source");
    std::fs::remove_file(profile.join(".credentials.json.rayline.lock")).expect("remove old lock");
    for launch in ["one", "two", "three"] {
        let mut selected = pool
            .select(launch, "sonnet", &HashSet::new())
            .await
            .expect("cached selection");
        assert_eq!(selected.access_token.expose(), "initial-access");
        selected.accept();
    }
    assert_eq!(usage.count.load(Ordering::SeqCst), initial_usage);
    assert_eq!(token.count.load(Ordering::SeqCst), 0);
    assert!(!profile.join(".credentials.json.rayline.lock").exists());
}

#[tokio::test]
async fn expired_pending_grant_is_not_served_and_saved_refresh_grant_is_reused_after_repair() {
    let temp = tempfile::tempdir().expect("temp");
    let profile = temp.path().join("profile");
    credentials(&profile);
    let original = std::fs::read(profile.join(".credentials.json")).expect("original");
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint_with_followup(
        true,
        r#"{"access_token":"short-access","refresh_token":"short-refresh","expires_in":1}"#,
        Some(GRANT),
        true,
    )
    .await;
    let pool = runtime(temp.path(), &usage, &token, false).await;
    let p = pool.clone();
    let refresh = tokio::spawn(async move { p.force_refresh("a").await });
    token.wait().await;
    std::fs::remove_file(profile.join(".credentials.json")).expect("remove");
    std::fs::create_dir(profile.join(".credentials.json")).expect("obstruct");
    token.resume();
    assert_eq!(
        refresh
            .await
            .expect("refresh task")
            .expect("pending short grant")
            .expose(),
        "short-access"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert!(
        pool.select("expired", "sonnet", &HashSet::new())
            .await
            .is_err()
    );
    assert_eq!(
        token.count.load(Ordering::SeqCst),
        1,
        "do not mint against the stale disk grant"
    );
    assert_eq!(
        pool.status().accounts[0].credential_health,
        CredentialHealth::Unavailable
    );
    std::fs::remove_dir(profile.join(".credentials.json")).expect("repair directory");
    std::fs::write(profile.join(".credentials.json"), original).expect("restore source");
    tokio::time::sleep(Duration::from_millis(2)).await;
    while let Ok(permit) = token.release.try_acquire() {
        permit.forget();
    }
    token.paused.store(true, Ordering::SeqCst);
    let p = pool.clone();
    let selection =
        tokio::spawn(async move { p.select("recovered", "sonnet", &HashSet::new()).await });
    token.wait().await;
    let saved: serde_json::Value = serde_json::from_slice(
        &std::fs::read(profile.join(".credentials.json")).expect("saved retained grant"),
    )
    .expect("json");
    assert_eq!(saved["claudeAiOauth"]["refreshToken"], "short-refresh");
    token.resume();
    assert_eq!(
        selection
            .await
            .expect("selection task")
            .expect("fresh admission")
            .access_token
            .expose(),
        "rotated-access"
    );
    assert_eq!(token.count.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn healthy_observations_do_not_create_or_rewrite_recovery_snapshots() {
    let temp = tempfile::tempdir().expect("temp");
    credentials(&temp.path().join("profile"));
    let root = std::fs::canonicalize(temp.path()).expect("physical root");
    let usage = endpoint(false, USAGE, false).await;
    let token = endpoint(true, GRANT, false).await;
    let pool = runtime(&root, &usage, &token, true).await;
    let snapshot = root.join("state/routing-blocks.json");
    assert!(!snapshot.exists());
    for _ in 0..3 {
        let observation = pool.begin_observation("a").expect("observation");
        pool.observe_response_headers("a", "sonnet", &HeaderSnapshot::default(), observation)
            .await
            .expect("healthy observation");
        pool.refresh_all_usage().await;
        pool.flush_recovery_state().await.expect("no-change flush");
        assert!(
            !snapshot.exists(),
            "healthy traffic must not write a recovery snapshot"
        );
    }
    let observation = pool.begin_observation("a").expect("rejection observation");
    pool.record_quota_rejection("a", "sonnet", &HeaderSnapshot::default(), observation)
        .await
        .expect("durable block");
    assert!(
        snapshot.exists(),
        "changed recovery state must be durable before returning"
    );
}
