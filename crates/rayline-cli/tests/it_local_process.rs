//! Real supervisor + subprocess + HTTP connections, with an external core contract fixture.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The router key `rayline local start` hands the local process; a synthetic
/// stand-in.
const ROUTER_KEY: &str = "rlk-test-key";

struct Runtime {
    dir: PathBuf,
    child: Child,
}
impl Drop for Runtime {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = request_stop(&self.child);
        }
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Ask the local process to stop the way its launcher does: SIGTERM, or the
/// Windows stop event.
fn request_stop(child: &Child) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let status = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()?;
        if !status.success() {
            return Err(std::io::Error::other("kill -TERM failed"));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        rayline_local_router::windows_state::request_stop(child.id())
    }
}

/// Install the fixture as an executable core. Windows cannot run the script
/// itself, so a .cmd launcher runs it with the Python on PATH.
fn install_core(dir: &Path) -> PathBuf {
    let source = include_str!("fixtures/execution-contract.py");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = dir.join("core");
        fs::write(&script, source).unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
        script
    }
    #[cfg(windows)]
    {
        static PYTHON: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let python = PYTHON.get_or_init(|| {
            let output = Command::new("python")
                .args(["-c", "import sys; print(sys.executable)"])
                .output()
                .expect("the hybrid fixture needs Python on PATH");
            assert!(output.status.success(), "python on PATH does not run");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        });
        fs::write(dir.join("core.py"), source).unwrap();
        let launcher = dir.join("core.cmd");
        fs::write(
            &launcher,
            format!(
                "@\"{python}\" \"%~dp0core.py\" %*
"
            ),
        )
        .unwrap();
        launcher
    }
}

/// Whether only the current user can open `path`.
fn owner_only(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777 == 0o600
    }
    #[cfg(windows)]
    {
        rayline_local_router::windows_state::is_owner_only(path).unwrap()
    }
}

/// Whether `pid` still runs (a zombie has exited).
fn process_running(pid: &str) -> bool {
    #[cfg(unix)]
    {
        let status = Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        status.status.success() && !String::from_utf8_lossy(&status.stdout).contains('Z')
    }
    #[cfg(windows)]
    {
        let output = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .unwrap();
        assert!(output.status.success(), "tasklist failed");
        String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
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
            "http://127.0.0.1:9/v1/routes",
            "",
            Some(ROUTER_KEY),
        )
    }
    fn start_with_decisions(
        version: u32,
        delay: u64,
        deadline: u64,
        capabilities: Option<Value>,
        decisions_url: &str,
        raw_members: &str,
        router_key: Option<&str>,
    ) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "rayline-local-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&dir).unwrap();
        let script = install_core(&dir);
        // The fixture writes its requests to events.jsonl beside this config.
        let mut config = json!({"cancelled":dir.join("cancelled"), "contract_version":version,
            "handshake_delay":delay, "decisions": {"url": decisions_url, "auth_env": "RAYLINE_ROUTER_API_KEY"},
            "launcher": cfg!(windows)});
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_rayline"));
        command.env_remove("RAYLINE_ROUTER_API_KEY");
        if let Some(key) = router_key {
            command.env("RAYLINE_ROUTER_API_KEY", key);
        }
        let child = command
            .arg("__local-process")
            .arg("--core")
            .arg(script)
            .arg("--config")
            .arg(dir.join("config.json"))
            .arg("--startup-timeout-secs")
            .arg(deadline.to_string())
            .arg("--token-file")
            .arg(dir.join("token"))
            .arg("--ready-file")
            .arg(dir.join("ready.json"))
            .args(["--router-url", "https://api.rayline.ai"])
            .args(["--classifier-model", "claude-haiku-5-5"])
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
                assert!(owner_only(&self.dir.join("token")));
                assert!(owner_only(&self.dir.join("ready.json")));
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
            if !process_running(pid.trim()) {
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

async fn top_snapshot(client: &reqwest::Client, url: &str, token: &str) -> Value {
    let response = client
        .get(format!("{url}/v1/router/top/snapshot"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let body = response.text().await.unwrap();
    assert!(
        !body.contains(token),
        "snapshot exposed its gateway credential"
    );
    let snapshot: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(snapshot["runtime"], "rayline-local");
    snapshot
}

async fn assert_cancelled_snapshot(client: &reqwest::Client, url: &str, token: &str, id: &Value) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let snapshot = top_snapshot(client, url, token).await;
        if snapshot["active"].as_array().unwrap().is_empty() {
            assert_eq!(snapshot["totals"]["active_requests"], 0);
            assert_eq!(snapshot["totals"]["completed_requests"], 1);
            assert_eq!(snapshot["totals"]["errored_requests"], 1);
            let recent = snapshot["recent"].as_array().unwrap();
            assert_eq!(
                recent.len(),
                1,
                "disconnect produced duplicate terminal rows"
            );
            assert_eq!(&recent[0]["request_id"], id);
            assert_eq!(recent[0]["state"], "error");
            assert!(
                recent[0]["error"]
                    .as_str()
                    .is_some_and(|error| !error.is_empty())
            );
            assert!(recent[0]["completed_at_unix_ms"].is_number());
            assert!(
                recent[0]["target"].is_null(),
                "fixture has no execution receipt"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled request stayed active: {snapshot}"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

#[tokio::test]
async fn all_protocols_preserve_body_and_identity_then_stop_owned_core() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let unauthorized = client.get(format!("{url}/healthz")).send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);
    let unauthorized_top = client
        .get(format!("{url}/v1/router/top/snapshot"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthorized_top.status(), 401);
    let initial = top_snapshot(&client, &url, &token).await;
    assert_eq!(initial["pid"], runtime.child.id());
    assert_eq!(initial["totals"]["active_requests"], 0);
    assert_eq!(initial["totals"]["completed_requests"], 0);
    assert!(initial["recent"].as_array().unwrap().is_empty());
    let body = "{ \"model\":\"rayline-auto\", \"thinking\":{\"type\":\"adaptive\"},\"messages\":[],\"input\":[],\"private\":\"unaltered\" }";
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
    let snapshot = top_snapshot(&client, &url, &token).await;
    assert_eq!(snapshot["totals"]["local_requests"], 0);
    assert_eq!(
        snapshot["totals"]["remote_requests"], 0,
        "missing receipts must not imply remote routing"
    );
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
async fn encoded_response_preserves_wire_bytes_and_completes_without_body_telemetry() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let expected = [
        31, 139, 8, 0, 0, 0, 0, 0, 2, 255, 171, 86, 42, 45, 78, 76, 79, 85, 178, 170, 86, 202, 204,
        43, 40, 45, 137, 47, 201, 207, 78, 205, 43, 86, 178, 50, 215, 81, 202, 47, 45, 65, 22, 49,
        174, 173, 5, 0, 174, 189, 157, 210, 46, 0, 0, 0,
    ];
    for eof in [false, true] {
        let response = client
            .post(format!("{url}/v1/messages"))
            .bearer_auth(&token)
            .json(&json!({"fixture_gzip": true, "fixture_eof": eof}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["content-encoding"], "gzip");
        assert_eq!(response.bytes().await.unwrap().as_ref(), &expected);
        let snapshot = top_snapshot(&client, &url, &token).await;
        assert!(snapshot["active"].as_array().unwrap().is_empty());
        let row = &snapshot["recent"][0];
        assert_eq!(row["state"], "completed");
        assert!(row["input_tokens"].is_null());
        assert!(row["output_tokens"].is_null());
        assert!(row["first_token_at_unix_ms"].is_null());
        assert_eq!(snapshot["totals"]["errored_requests"], 0);
    }
}

#[tokio::test]
async fn valid_deeply_nested_core_output_is_a_completed_request() {
    // The core is trusted to be well formed: JSON the observer cannot parse (here,
    // valid but nested past serde_json's limit) must not fail a successful turn.
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .json(&json!({"fixture_deep": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body = response.text().await.unwrap();
    assert_eq!(body.matches('[').count(), 201);
    let snapshot = top_snapshot(&client, &url, &token).await;
    assert!(snapshot["active"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["recent"][0]["state"], "completed");
    assert_eq!(snapshot["recent"][0]["status_code"], 200);
    assert_eq!(snapshot["totals"]["errored_requests"], 0);
}

#[tokio::test]
async fn empty_responses_close_the_row_with_the_real_status() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    // The same policy as the Content-Length and EOF paths: a refusal is an error
    // with its real status; an empty JSON body from the trusted core completes;
    // an empty event-stream never reached a terminal, so it is an error.
    let cases = [
        (429, "application/json", "error"),
        (200, "application/json", "completed"),
        (200, "text/event-stream", "error"),
    ];
    for (index, (status, content_type, state)) in cases.into_iter().enumerate() {
        let response = client
            .post(format!("{url}/v1/messages"))
            .bearer_auth(&token)
            .json(&json!({"fixture_empty_status": status, "fixture_empty_content_type": content_type}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        assert!(response.bytes().await.unwrap().is_empty());
        let snapshot = top_snapshot(&client, &url, &token).await;
        assert!(snapshot["active"].as_array().unwrap().is_empty());
        let row = &snapshot["recent"][0];
        assert_eq!(snapshot["recent"].as_array().unwrap().len(), index + 1);
        assert_eq!(row["state"], state);
        assert_eq!(row["status_code"], status);
        let error = row["error"].as_str().unwrap_or_default();
        assert!(!error.contains("cancelled"), "{error}");
    }
}

#[tokio::test]
async fn derived_models_sharing_a_worker_attribute_the_executed_deployment() {
    // Derived action IDs are produced by the core, not enumerated in daemon config.
    // The shared worker alone is ambiguous; the executed model disambiguates it.
    let members = r#","deployments":[{"id":"local","kind":"local","wire_model":"local-wire"},{"id":"cloud","kind":"cloud","wire_model":"cloud-wire"}],"derived_actions":[{"worker":"shared","model":"local-model","deployment_id":"local"},{"worker":"shared","model":"cloud-model","deployment_id":"cloud"}]"#;
    let mut runtime = Runtime::start_with_decisions(
        1,
        0,
        120,
        None,
        "http://127.0.0.1:9/v1/routes",
        members,
        Some(ROUTER_KEY),
    );
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    for (index, (model, deployment, target, wire)) in [
        ("local-model", "local", "local", "local-wire"),
        ("cloud-model", "cloud", "remote", "cloud-wire"),
        ("local-model", "local", "local", "local-wire"),
    ]
    .into_iter()
    .enumerate()
    {
        client
            .post(format!("{url}/v1/messages"))
            .bearer_auth(&token)
            .json(&json!({"model": model, "messages": []}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let snapshot = top_snapshot(&client, &url, &token).await;
        assert!(snapshot["active"].as_array().unwrap().is_empty());
        assert_eq!(snapshot["recent"].as_array().unwrap().len(), index + 1);
        let row = &snapshot["recent"][0];
        assert_eq!(row["state"], "completed");
        assert_eq!(row["decision_model"], "fixture-model");
        assert_eq!(row["executed_model"], model);
        assert_eq!(row["executed_worker"], "shared");
        assert_eq!(row["endpoint_id"], deployment);
        assert_eq!(row["target"], target);
        assert_eq!(row["wire_model"], wire);
        assert_eq!(snapshot["totals"]["completed_requests"], index + 1);
        assert_eq!(snapshot["totals"]["errored_requests"], 0);
    }
    let snapshot = top_snapshot(&client, &url, &token).await;
    assert_eq!(snapshot["totals"]["local_requests"], 2);
    assert_eq!(snapshot["totals"]["remote_requests"], 1);
}

#[tokio::test]
async fn client_cancellation_reaches_core_and_core_crash_stops_gateway() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut stream = client
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .json(&json!({"model": "rayline-auto", "stream": true}))
        .send()
        .await
        .unwrap();
    assert!(stream.chunk().await.unwrap().is_some());
    let active = top_snapshot(&client, &url, &token).await;
    assert_eq!(active["totals"]["active_requests"], 1);
    assert_eq!(active["active"].as_array().unwrap().len(), 1);
    assert!(active["recent"].as_array().unwrap().is_empty());
    let request_id = active["active"][0]["request_id"].clone();
    drop(stream);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.dir.join("cancelled").exists() {
        assert!(
            Instant::now() < deadline,
            "core did not observe downstream cancellation"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_cancelled_snapshot(&client, &url, &token, &request_id).await;
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
    // key) to a non-router host: the local process refuses before spawning the core.
    for url in [
        "https://decisions.example.com/v1/routes",
        "https://api.rayline.ai:8443/v1/routes",
        "https://evil.test@api.rayline.ai/v1/routes",
        "http://api.rayline.ai/v1/routes",
    ] {
        let mut runtime = Runtime::start_with_decisions(1, 0, 120, None, url, "", Some(ROUTER_KEY));
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
        let mut runtime =
            Runtime::start_with_decisions(1, 0, 120, None, url, raw, Some(ROUTER_KEY));
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
    request_stop(&runtime.child).unwrap();
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
    let body = r#"{"model":"rayline-auto","fixture_pending":true}"#;
    socket.write_all(format!("POST /v1/messages HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}", endpoint.port().unwrap(), token, body.len(), body).as_bytes()).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !runtime.dir.join("events.jsonl").exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let active = top_snapshot(&client, &url, &token).await;
    assert_eq!(active["totals"]["active_requests"], 1);
    assert_eq!(active["active"].as_array().unwrap().len(), 1);
    assert!(active["recent"].as_array().unwrap().is_empty());
    let request_id = active["active"][0]["request_id"].clone();
    drop(socket);
    while !runtime.dir.join("cancelled-pending").exists() {
        assert!(
            Instant::now() < deadline,
            "core did not observe cancellation before response headers"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_cancelled_snapshot(&client, &url, &token, &request_id).await;
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

/// A two-request router stand-in: records requests and answers the malformed
/// response probe with HTTP 200 plus invalid JSON.
/// The recorded request line, lower-cased headers and JSON body.
type Recorded = (String, Vec<(String, String)>, Value);

fn fake_router() -> (String, std::thread::JoinHandle<Vec<Recorded>>) {
    fake_router_for(2)
}

/// A router stand-in that answers exactly `count` requests.
fn fake_router_for(count: usize) -> (String, std::thread::JoinHandle<Vec<Recorded>>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let mut recorded = Vec::new();
        for _ in 0..count {
            let (stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut headers = Vec::new();
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (name, value) = header.split_once(':').unwrap();
                let (name, value) = (name.to_ascii_lowercase(), value.trim().to_owned());
                if name == "content-length" {
                    length = value.parse().unwrap();
                }
                headers.push((name, value));
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            let answer = if body["messages"][0]["content"] == "fixture-malformed-response" {
                "{private-provider-response-do-not-log".to_owned()
            } else if body["messages"][0]["content"] == "fixture-empty-response" {
                String::new()
            } else {
                json!({"id": "msg_cloud", "type": "message", "role": "assistant",
                "content": [{"type": "text", "text": "<block>no</block>"}]})
                .to_string()
            };
            let mut stream = stream;
            write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-rayline-selected-model: claude-sonnet-5-5\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
            answer.len()
        )
        .unwrap();
            recorded.push((line, headers, body));
        }
        recorded
    });
    (url, handle)
}

/// An auto-mode classifier request body whose first message is `content`.
fn classifier_request(content: &str) -> Value {
    json!({"model": "claude-sonnet-5", "max_tokens": 64,
        "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.319"},
                   {"type": "text", "text": "You are a security monitor for autonomous AI coding agents.\n"}],
        "messages": [{"role": "user", "content": content}]})
}

#[tokio::test]
async fn an_empty_classifier_response_is_an_error_not_a_completed_request() {
    // The cloud router is not trusted: an HTTP 200 with Content-Length 0 is not a
    // JSON answer, and must not be recorded as a completed request just because
    // hyper never polls a known-empty body.
    let (router, recorded) = fake_router_for(1);
    let mut runtime = Runtime::start_with_decisions(
        1,
        0,
        120,
        None,
        &format!("{router}/v1/routes"),
        "",
        Some(ROUTER_KEY),
    );
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let response = client
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .header("anthropic-beta", "claude-code-20250219")
        .json(&classifier_request("fixture-empty-response"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.bytes().await.unwrap().is_empty());
    let snapshot = top_snapshot(&client, &url, &token).await;
    assert!(snapshot["active"].as_array().unwrap().is_empty());
    let row = &snapshot["recent"][0];
    assert_eq!(row["state"], "error");
    assert_eq!(row["status_code"], 200);
    assert!(
        !row["error"].as_str().unwrap_or_default().contains("cancel"),
        "{row}"
    );
    assert_eq!(snapshot["totals"]["errored_requests"], 1);
    recorded.join().unwrap();
}

#[tokio::test]
async fn without_the_router_key_the_local_process_does_not_start() {
    let mut runtime =
        Runtime::start_with_decisions(1, 0, 120, None, "http://127.0.0.1:9/v1/routes", "", None);
    assert!(!runtime.exited().await.success());
    assert!(!runtime.dir.join("config.pid").exists());
    assert!(!runtime.dir.join("ready.json").exists());
}

#[tokio::test]
async fn auto_mode_classifier_goes_to_haiku_on_the_cloud_router_and_other_turns_to_the_core() {
    let (router, recorded) = fake_router();
    let mut runtime = Runtime::start_with_decisions(
        1,
        0,
        120,
        None,
        &format!("{router}/v1/routes"),
        "",
        Some(ROUTER_KEY),
    );
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let classifier = json!({"model": "claude-sonnet-5", "max_tokens": 64,
        "system": [{"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.319"},
                   {"type": "text", "text": "You are a security monitor for autonomous AI coding agents.\n"}],
        "messages": [{"role": "user", "content": "<transcript>"}]});
    let response = client
        .post(format!("{url}/v1/messages?beta=true"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .header("anthropic-beta", "claude-code-20250219")
        .header("x-claude-code-session-id", "session-one")
        .json(&classifier)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap()["id"], "msg_cloud");
    let initial = top_snapshot(&client, &url, &token).await;
    assert_eq!(initial["recent"][0]["state"], "completed");
    let mut malformed = classifier.clone();
    malformed["messages"][0]["content"] = json!("fixture-malformed-response");
    let response = client
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .json(&malformed)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        "{private-provider-response-do-not-log"
    );
    let snapshot = top_snapshot(&client, &url, &token).await;
    assert!(snapshot["active"].as_array().unwrap().is_empty());
    assert_eq!(snapshot["recent"].as_array().unwrap().len(), 2);
    assert_eq!(snapshot["recent"][0]["state"], "error");
    assert_eq!(snapshot["recent"][0]["status_code"], 200);
    assert_eq!(snapshot["totals"]["errored_requests"], 1);
    assert!(
        !snapshot
            .to_string()
            .contains("private-provider-response-do-not-log")
    );
    assert!(!snapshot.to_string().contains("rlk-test-key"));
    let mut recorded = recorded.join().unwrap().into_iter();
    let (line, headers, body) = recorded.next().unwrap();
    let (_, _, malformed_body) = recorded.next().unwrap();
    assert_eq!(malformed_body["messages"], malformed["messages"]);
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    assert!(line.starts_with("POST /v1/messages "), "{line}");
    assert_eq!(
        header("authorization"),
        Some(format!("Bearer {ROUTER_KEY}").as_str())
    );
    // Claude Haiku 5.5 by default, through the cloud pins' router and key.
    assert_eq!(
        header("x-workshop-required-model"),
        Some("claude-haiku-5-5")
    );
    assert_eq!(header("anthropic-beta"), Some("claude-code-20250219"));
    // Forwarded as sent: the router keys on Claude Code's own header, so the
    // gateway does not invent an `x-conversation-id` from it.
    assert_eq!(header("x-claude-code-session-id"), Some("session-one"));
    assert_eq!(header("x-conversation-id"), None);
    let mut expected = classifier.clone();
    expected["model"] = json!("claude-haiku-5-5");
    assert_eq!(body, expected);

    // A turn with tools is an ordinary turn: the core gets it unchanged.
    let turn = "{\"model\":\"rayline-auto\",\"tools\":[{\"name\":\"Bash\"}],\"messages\":[]}";
    let response = client
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .header("content-type", "application/json")
        .body(turn)
        .send()
        .await
        .unwrap();
    assert_eq!(response.text().await.unwrap(), turn);
    let snapshot = top_snapshot(&client, &url, &token).await;
    assert!(snapshot["recent"][0]["target"].is_null());
    assert_eq!(snapshot["totals"]["local_requests"], 0);
    assert_eq!(
        snapshot["totals"]["remote_requests"], 2,
        "receipt-less ordinary turn counted as remote"
    );
    let events = fs::read_to_string(runtime.dir.join("events.jsonl")).unwrap();
    assert_eq!(
        events.lines().count(),
        1,
        "only the ordinary turn reaches the core"
    );
}

/// The classifier pin is the Rayline router, so it is keyed like the turn it
/// classifies: every session header the client sent reaches it unchanged (the
/// local process's shared `SESSION_HEADERS`, so both gateways forward the same
/// set), each exactly once, and the client's own credentials never do.
#[tokio::test]
async fn classifier_pin_carries_every_client_session_header_and_no_client_auth() {
    let (router, recorded) = fake_router_for(1);
    let mut runtime = Runtime::start_with_decisions(
        1,
        0,
        120,
        None,
        &format!("{router}/v1/routes"),
        "",
        Some(ROUTER_KEY),
    );
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let mut request = client
        .post(format!("{url}/v1/messages"))
        .bearer_auth(&token)
        .header("x-api-key", "client-key-must-not-leave");
    for name in rayline_local_router::SESSION_HEADERS {
        request = request.header(name, format!("{name}-one"));
    }
    let response = request
        .json(&classifier_request("<transcript>"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let (_, headers, _) = recorded.join().unwrap().into_iter().next().unwrap();
    let values = |name: &str| {
        headers
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
    };
    for name in rayline_local_router::SESSION_HEADERS {
        assert_eq!(values(name), [format!("{name}-one")], "{name}");
    }
    assert_eq!(values("authorization"), ["Bearer rlk-test-key"]);
    assert!(values("x-api-key").is_empty(), "{headers:?}");
}

/// One harness turn whose Decisions call fails is sent to the core at most
/// twice (the gateway's one resend), and the harness is told not to resend:
/// `x-should-retry: false`, which Claude Code's SDK honours.
#[tokio::test]
async fn a_failed_decision_reaches_the_core_at_most_twice_and_stops_the_harness_retrying() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    // (query, core calls, harness status, harness retry-after)
    let cases = [
        // Refused: final, never resent.
        ("refused=400", 1, 400, None),
        ("refused=422&should_retry=true", 1, 400, None),
        // Unavailable: one resend after the service's retry-after.
        ("refused=503&retry_after=1", 2, 503, Some("1")),
        ("refused=failed&retry_after=1", 2, 503, Some("1")),
        // Busy: the same.
        ("refused=429&retry_after=1", 2, 429, Some("1")),
        // The service says not to resend.
        (
            "refused=503&retry_after=1&should_retry=false",
            1,
            503,
            Some("1"),
        ),
        // A wait longer than the gateway holds a turn goes to the harness instead.
        ("refused=503&retry_after=120", 1, 503, Some("120")),
        // A timeout, a conflict or another 5xx: one resend, then final.
        ("refused=408&retry_after=1", 2, 503, Some("1")),
        ("refused=500&retry_after=0", 2, 503, Some("1")),
    ];
    let events = runtime.dir.join("events.jsonl");
    for (query, calls, status, retry_after) in cases {
        let before = fs::read_to_string(&events)
            .unwrap_or_default()
            .lines()
            .count();
        let response = client
            .post(format!("{url}/decide-fail?{query}"))
            .bearer_auth(&token)
            .header("content-type", "application/json")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{query}");
        assert_eq!(response.headers()["x-should-retry"], "false", "{query}");
        assert_eq!(
            response
                .headers()
                .get("retry-after")
                .map(|value| value.to_str().unwrap()),
            retry_after,
            "{query}"
        );
        assert_eq!(
            response.headers().get_all("retry-after").iter().count(),
            usize::from(retry_after.is_some()),
            "{query}"
        );
        let after = fs::read_to_string(&events).unwrap().lines().count();
        assert_eq!(after - before, calls, "{query}");
    }
    client
        .post(format!("{url}/_rayline/shutdown"))
        .bearer_auth(&token)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert!(runtime.exited().await.success());
}
