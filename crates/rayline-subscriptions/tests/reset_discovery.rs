use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use rayline_subscriptions::{
    CredentialHealth, CredentialSourceConfig, SubscriptionAccountConfig, SubscriptionPoolConfig,
    SubscriptionPoolRuntime, SubscriptionRuntimeOptions,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

struct Provider {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<[Value; 3]>>,
}

fn quota() -> Value {
    json!({"five_hour":{"utilization":10},"seven_day":{"utilization":20},"cedar_ember":null,"juniper_tide":null})
}
fn cedar() -> Value {
    json!({"eligible":true,"next_grant_id":"saved","grants":[{
        "id":"saved","label":"Saved weekly reset","resets_left":2,"clears":["seven_day"],
        "blocking":[],"paused":false,"usable_now":true,"use_requires_limit":true,
        "ends_at":"2099-01-01T00:00:00Z"
    },{
        "id":"paused","resets_left":3,"clears":["five_hour"],"blocking":[],"paused":true,"usable_now":true
    }]})
}
async fn provider() -> Provider {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("listener");
    let url = format!("http://{}", listener.local_addr().expect("address"));
    let requests = Arc::new(Mutex::new(Vec::new()));
    let bodies = Arc::new(Mutex::new([
        quota(),
        json!({"cedar_ember":cedar()}),
        json!({"juniper_tide":{"eligible":false,"ineligible_reason":"surface"}}),
    ]));
    let recorded = Arc::clone(&requests);
    let responses = Arc::clone(&bodies);
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let recorded = Arc::clone(&recorded);
            let responses = Arc::clone(&responses);
            tokio::spawn(async move {
                let mut bytes = Vec::new();
                let mut chunk = [0; 4096];
                loop {
                    let n = stream.read(&mut chunk).await.expect("request");
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&chunk[..n]);
                    if bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8(bytes).expect("HTTP request");
                let line = request.lines().next().expect("request line");
                assert!(line.starts_with("GET /api/oauth/usage"));
                assert!(
                    request
                        .to_ascii_lowercase()
                        .contains("authorization: bearer initial-access")
                        || request
                            .to_ascii_lowercase()
                            .contains("authorization: bearer replacement-access")
                );
                let index = if line.contains("cedar_ember=1") {
                    1
                } else if line.contains("at_wall=1") {
                    2
                } else {
                    0
                };
                if index != 0 {
                    assert!(line.contains("skip_spend=1"));
                }
                recorded.lock().expect("requests").push(line.to_owned());
                let (status, body) = {
                    let bodies = responses.lock().expect("bodies");
                    (
                        bodies[index]
                            .get("_http_status")
                            .and_then(Value::as_u64)
                            .unwrap_or(200),
                        bodies[index].to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} Response\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            });
        }
    });
    Provider {
        url,
        requests,
        bodies,
    }
}
fn credential(path: &Path, token: &str) {
    std::fs::create_dir_all(path).expect("profile");
    std::fs::write(path.join(".credentials.json"), json!({"claudeAiOauth":{
        "accessToken":token,"refreshToken":format!("refresh-{token}"),"expiresAt":4000000000000_i64,
        "scopes":["user:inference","user:profile"]
    }}).to_string()).expect("credentials");
}
async fn runtime(
    root: &Path,
    provider: &Provider,
    read_only: bool,
) -> Arc<SubscriptionPoolRuntime> {
    credential(&root.join("profile"), "initial-access");
    SubscriptionPoolRuntime::start(
        "pool",
        SubscriptionPoolConfig {
            control_config_dir: root.join("control"),
            accounts: vec![SubscriptionAccountConfig {
                id: "a".to_owned(),
                credential_source: CredentialSourceConfig {
                    claude_config_dir: root.join("profile"),
                },
            }],
            policy: Default::default(),
        },
        SubscriptionRuntimeOptions {
            anthropic_base_url: provider.url.clone(),
            request_timeout: Duration::from_secs(2),
            read_only,
            ..Default::default()
        },
    )
    .await
    .expect("runtime")
}

#[tokio::test]
async fn null_blocks_probe_both_programs_and_preserve_banked_vs_usable() {
    let provider = provider().await;
    let root = tempfile::tempdir().expect("root");
    let runtime = runtime(root.path(), &provider, false).await;
    let status = runtime.status().accounts.remove(0);
    assert!(status.usage_snapshot_fresh);
    assert_eq!(status.credential_health, CredentialHealth::Healthy);
    let resets = status.limit_resets.expect("inventory");
    assert!(resets.fresh);
    assert_eq!(resets.programs.len(), 2);
    let saved = &resets.programs[0].grants;
    assert_eq!(saved.iter().map(|g| g.remaining_count).sum::<u64>(), 5);
    assert!(saved[0].usable_now);
    assert!(!saved[1].usable_now);
    assert_eq!(resets.programs[1].reason.as_deref(), Some("surface"));
    assert_eq!(provider.requests.lock().expect("requests").len(), 3);
}

#[tokio::test]
async fn malformed_discovery_keeps_quota_and_previous_inventory_stale() {
    let provider = provider().await;
    let root = tempfile::tempdir().expect("root");
    let runtime = runtime(root.path(), &provider, false).await;
    provider.bodies.lock().expect("bodies")[1] =
        json!({"cedar_ember":{"eligible":true,"grants":"malformed"}});
    runtime.refresh_all_usage().await;
    let status = runtime.status().accounts.remove(0);
    assert!(status.usage_snapshot_fresh);
    assert!(status.complete_global_snapshot);
    assert!(status.last_error.is_none());
    let resets = status.limit_resets.expect("retained inventory");
    assert!(!resets.fresh);
    assert_eq!(resets.programs[0].grants[0].remaining_count, 2);
    credential(&root.path().join("profile"), "replacement-access");
    runtime.reload_credentials().await;
    let resets = runtime
        .status()
        .accounts
        .remove(0)
        .limit_resets
        .expect("known surface program");
    assert!(!resets.fresh);
    assert!(resets.programs.iter().all(|p| p.program != "cedar_ember"));
}

#[tokio::test]
async fn inline_answers_avoid_probes_and_unsupported_inventory_stays_unknown() {
    let provider = provider().await;
    let root = tempfile::tempdir().expect("root");
    {
        let mut bodies = provider.bodies.lock().expect("bodies");
        bodies[0]["cedar_ember"] = cedar();
        bodies[0]["juniper_tide"] = json!({"eligible":true,"arm":"reset","available":true,"weekly_resets_at":"2099-01-01T00:00:00Z"});
    }
    let pool = runtime(root.path(), &provider, false).await;
    let resets = pool
        .status()
        .accounts
        .remove(0)
        .limit_resets
        .expect("inline inventory");
    assert!(resets.fresh);
    assert_eq!(resets.programs[1].grants[0].clears, vec!["five_hour"]);
    assert!(resets.programs[1].grants[0].usable_now);
    assert_eq!(provider.requests.lock().expect("requests").len(), 1);
    let other_root = tempfile::tempdir().expect("root");
    *provider.bodies.lock().expect("bodies") = [quota(), quota(), quota()];
    let unknown = runtime(other_root.path(), &provider, false).await;
    assert!(unknown.status().accounts[0].limit_resets.is_none());
    assert!(unknown.status().accounts[0].usage_snapshot_fresh);
}

#[tokio::test]
async fn probe_auth_failure_does_not_refresh_or_reject_valid_quota() {
    let provider = provider().await;
    let root = tempfile::tempdir().expect("root");
    provider.bodies.lock().expect("bodies")[1] =
        json!({"_http_status":401,"error":"private provider diagnostic"});
    let pool = runtime(root.path(), &provider, false).await;
    let status = pool.status().accounts.remove(0);
    assert_eq!(status.credential_health, CredentialHealth::Healthy);
    assert!(status.usage_snapshot_fresh);
    assert!(status.last_error.is_none());
    assert!(
        !serde_json::to_string(&status.last_error)
            .expect("JSON")
            .contains("private provider diagnostic")
    );
    let inventory = status.limit_resets.expect("partial known answer");
    assert!(!inventory.fresh);
    assert_eq!(inventory.programs.len(), 1);
    assert_eq!(provider.requests.lock().expect("requests").len(), 3);
}

#[tokio::test]
async fn standalone_read_only_never_discovers_or_refreshes() {
    let provider = provider().await;
    let root = tempfile::tempdir().expect("root");
    let runtime = runtime(root.path(), &provider, true).await;
    runtime.refresh_all_usage().await;
    runtime.reload_credentials().await;
    assert!(runtime.status().accounts[0].limit_resets.is_none());
    assert!(provider.requests.lock().expect("requests").is_empty());
}
