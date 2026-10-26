#![cfg(unix)]
//! Real supervisor + subprocess + HTTP connections, with an external core contract fixture.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

struct Runtime {
    dir: PathBuf,
    child: Child,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = Command::new("kill")
                .args(["-TERM", &self.child.id().to_string()])
                .status();
        }
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}
impl Runtime {
    fn start(version: u32) -> Self {
        Self::start_with_deadline(version, 0, 120)
    }
    fn start_with_deadline(version: u32, delay: u64, deadline: u64) -> Self {
        Self::start_with_capabilities(version, delay, deadline, None)
    }
    fn start_with_capabilities(
        version: u32,
        delay: u64,
        deadline: u64,
        capabilities: Option<Value>,
    ) -> Self {
        Self::start_with_decisions(
            version,
            delay,
            deadline,
            capabilities,
            "http://127.0.0.1:9/decisions",
            "",
        )
    }
    fn start_with_decisions(
        version: u32,
        delay: u64,
        deadline: u64,
        capabilities: Option<Value>,
        decisions_url: &str,
        raw_members: &str,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rayline-hybrid-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&dir).unwrap();
        let script = dir.join("core");
        fs::write(&script, include_str!("fixtures/execution-contract.py")).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        let mut config = json!({"events":dir.join("events.jsonl"),
            "cancelled":dir.join("cancelled"), "contract_version":version,
            "handshake_delay":delay, "decisions": {"url": decisions_url}});
        if let Some(capabilities) = capabilities {
            config["capabilities"] = capabilities;
        }
        // Extra members as raw JSON text, for keys serde_json cannot build.
        let mut text = config.to_string();
        if !raw_members.is_empty() {
            text.pop();
            text.push_str(raw_members);
            text.push('}');
        }
        fs::write(dir.join("config.json"), text).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_rld"))
            .arg("hybrid")
            .arg("--core")
            .arg(script)
            .arg("--config")
            .arg(dir.join("config.json"))
            .args(["--port", "0"])
            .arg("--startup-timeout-secs")
            .arg(deadline.to_string())
            .arg("--token-file")
            .arg(dir.join("token"))
            .arg("--ready-file")
            .arg(dir.join("ready.json"))
            .env_remove("RAYLINE_ROUTER_API_KEY")
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        Self { dir, child }
    }
    async fn ready(&mut self) -> (String, String) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.child.try_wait().unwrap().is_none(),
                "runtime exited before readiness"
            );
            if let Ok(bytes) = fs::read(self.dir.join("ready.json")) {
                let ready: Value = serde_json::from_slice(&bytes).unwrap();
                let token = fs::read_to_string(self.dir.join("token")).unwrap();
                assert_eq!(
                    fs::metadata(self.dir.join("token"))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
                return (ready["listen_url"].as_str().unwrap().to_owned(), token);
            }
            assert!(Instant::now() < deadline, "startup timed out");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
    async fn exited(&mut self) -> std::process::ExitStatus {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                return status;
            }
            assert!(Instant::now() < deadline, "runtime did not exit");
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }

    async fn core_exited(&self) {
        let pid = fs::read_to_string(self.dir.join("config.pid")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = Command::new("ps")
                .args(["-o", "stat=", "-p", &pid])
                .output()
                .unwrap();
            if !status.status.success() || String::from_utf8_lossy(&status.stdout).contains('Z') {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "owned core process survived supervisor exit"
            );
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
    }
}

#[tokio::test]
async fn all_protocols_preserve_body_and_identity_then_stop_owned_core() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let unauthorized = client.get(format!("{url}/healthz")).send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);
    let body = "{ \"model\":\"rayline-router\", \"thinking\":{\"type\":\"adaptive\"},\"messages\":[],\"input\":[],\"private\":\"unaltered\" }";
    for path in ["/v1/messages", "/v1/chat/completions", "/v1/responses"] {
        let response = client
            .post(format!("{url}{path}"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .header("x-session-affinity", "session-one")
            .header("x-claude-code-agent-id", "branch-one")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.headers()["x-rayline-selected-model"],
            "fixture-model"
        );
        assert_eq!(response.text().await.unwrap(), body);
    }
    let events: Vec<Value> = fs::read_to_string(runtime.dir.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 3);
    for event in events {
        assert_eq!(event["body"], body);
        assert_eq!(event["session"], "session-one");
        assert_eq!(event["branch"], "branch-one");
        assert_eq!(event["core_authenticated"], true);
    }
    client
        .post(format!("{url}/_rayline/shutdown"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(runtime.exited().await.success());
    runtime.core_exited().await;
    assert!(!runtime.dir.join("token").exists());
    assert!(!runtime.dir.join("ready.json").exists());
}

#[tokio::test]
async fn client_cancellation_reaches_core_and_core_crash_stops_gateway() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut stream = client
        .post(format!("{url}/stream"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(stream.chunk().await.unwrap().is_some());
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.dir.join("cancelled").exists() {
        assert!(
            Instant::now() < deadline,
            "core did not observe downstream cancellation"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let _ = client
        .post(format!("{url}/crash"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await;
    assert!(!runtime.exited().await.success());
    assert!(!runtime.dir.join("ready.json").exists());
    assert!(!runtime.dir.join("token").exists());
}

#[tokio::test]
async fn incompatible_core_never_becomes_ready_and_is_reaped() {
    let mut runtime = Runtime::start(99);
    assert!(!runtime.exited().await.success());
    runtime.core_exited().await;
    assert!(!runtime.dir.join("ready.json").exists());
    assert!(!runtime.dir.join("token").exists());
}

#[tokio::test]
async fn non_router_decisions_url_never_starts_the_core() {
    // A hand-edited core configuration cannot send decisions (and the router
    // key) to a non-router host: rld refuses before spawning the core.
    for url in [
        "https://rayline--decisions.modal.run/v1/routes",
        "https://api.rayline.ai:8443/v1/routes",
        "https://evil.test@api.rayline.ai/v1/routes",
        "http://api.rayline.ai/v1/routes",
    ] {
        let mut runtime = Runtime::start_with_decisions(1, 0, 120, None, url, "");
        assert!(!runtime.exited().await.success(), "{url}");
        assert!(!runtime.dir.join("config.pid").exists(), "{url}");
        assert!(!runtime.dir.join("ready.json").exists());
        assert!(!runtime.dir.join("token").exists());
    }
    // A key Go folds onto `decisions`, or a duplicate key Go would merge,
    // cannot replace the checked URL.
    for (url, raw) in [
        (
            "https://api.rayline.ai/v1/routes",
            r#","deci\u017fions":{"url":"https://evil.test/v1/routes"}"#,
        ),
        (
            "https://evil.test/v1/routes",
            r#","decisions":{"url":"https://api.rayline.ai/v1/routes"}"#,
        ),
    ] {
        let mut runtime = Runtime::start_with_decisions(1, 0, 120, None, url, raw);
        assert!(!runtime.exited().await.success(), "{raw}");
        assert!(!runtime.dir.join("config.pid").exists(), "{raw}");
    }
}

#[tokio::test]
async fn invalid_capabilities_never_publish_readiness_and_reap_core() {
    for capabilities in [
        Value::Null,
        json!({"context_window": 16384, "max_output_tokens": 32000,
            "input_modalities": ["text"]}),
        json!({"context_window": 16384, "max_output_tokens": 8192,
            "input_modalities": ["audio"]}),
    ] {
        let mut runtime = Runtime::start_with_capabilities(1, 0, 120, Some(capabilities));
        assert!(!runtime.exited().await.success());
        runtime.core_exited().await;
        assert!(!runtime.dir.join("ready.json").exists());
        assert!(!runtime.dir.join("token").exists());
    }
}

#[tokio::test]
async fn startup_deadline_reaps_core_without_publishing_readiness() {
    let mut runtime = Runtime::start_with_deadline(1, 5, 1);
    assert!(!runtime.exited().await.success());
    runtime.core_exited().await;
    assert!(!runtime.dir.join("ready.json").exists());
    assert!(!runtime.dir.join("token").exists());
}

#[tokio::test]
async fn shutdown_during_startup_reaps_owned_core() {
    let mut runtime = Runtime::start_with_deadline(1, 5, 10);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.dir.join("config.pid").exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &runtime.child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(runtime.exited().await.success());
    runtime.core_exited().await;
    assert!(!runtime.dir.join("ready.json").exists());
    assert!(!runtime.dir.join("token").exists());
}

#[tokio::test]
async fn abrupt_supervisor_exit_closes_parent_pipe_and_stops_owned_core() {
    let mut runtime = Runtime::start(1);
    runtime.ready().await;
    runtime.child.kill().unwrap();
    runtime.exited().await;
    runtime.core_exited().await;
}

#[tokio::test]
async fn cancellation_before_headers_reaches_core() {
    use tokio::io::AsyncWriteExt;
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let endpoint = reqwest::Url::parse(&url).unwrap();
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", endpoint.port().unwrap()))
        .await
        .unwrap();
    socket.write_all(format!("POST /pending HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{{}}", endpoint.port().unwrap(), token).as_bytes()).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.dir.join("events.jsonl").exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    drop(socket);
    while !runtime.dir.join("cancelled-pending").exists() {
        assert!(
            Instant::now() < deadline,
            "core did not observe cancellation before response headers"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn session_and_branch_precedence_preserves_native_fork_identity() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (headers, session, branch) in [
        (
            vec![
                ("x-rayline-session-id", "explicit-session"),
                ("x-rayline-branch-id", "explicit-branch"),
                ("thread-id", "codex-child"),
            ],
            Some("explicit-session"),
            "explicit-branch",
        ),
        (
            vec![
                ("thread-id", "codex-child"),
                ("x-claude-code-agent-id", "lower-priority"),
            ],
            None,
            "codex-child",
        ),
        (
            vec![
                ("x-parent-session-id", "opencode-root"),
                ("x-session-id", "opencode-child"),
            ],
            Some("opencode-root"),
            "opencode-child",
        ),
    ] {
        let mut request = client
            .post(format!("{url}/v1/responses"))
            .bearer_auth(&token)
            .json(&json!({"prompt_cache_key":"body-session"}));
        for (name, value) in headers {
            request = request.header(name, value);
        }
        request.send().await.unwrap().error_for_status().unwrap();
        let lines = fs::read_to_string(runtime.dir.join("events.jsonl")).unwrap();
        let event: Value = serde_json::from_str(lines.lines().last().unwrap()).unwrap();
        assert_eq!(event["session"], json!(session));
        assert_eq!(event["branch"], branch);
    }
}
