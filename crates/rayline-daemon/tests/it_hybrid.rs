//! Real supervisor + subprocess + HTTP connections, with an external core contract fixture.
use std::fs;
use std::path::{Path, PathBuf};
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
            let _ = request_stop(&self.child);
        }
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

/// Ask rld to stop the way its launcher does: SIGTERM, or the Windows stop event.
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
            "http://127.0.0.1:9/decisions",
            "",
            None,
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
            "rayline-hybrid-{}-{:x}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&dir).unwrap();
        let script = install_core(&dir);
        let mut config = json!({"events":dir.join("events.jsonl"),
            "cancelled":dir.join("cancelled"), "contract_version":version,
            "handshake_delay":delay, "decisions": {"url": decisions_url},
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
        let mut command = Command::new(env!("CARGO_BIN_EXE_rld"));
        command.env_remove("RAYLINE_ROUTER_API_KEY");
        if let Some(key) = router_key {
            command.env("RAYLINE_ROUTER_API_KEY", key);
        }
        let child = command
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

#[tokio::test]
async fn all_protocols_preserve_body_and_identity_then_stop_owned_core() {
    let mut runtime = Runtime::start(1);
    let (url, token) = runtime.ready().await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let unauthorized = client.get(format!("{url}/healthz")).send().await.unwrap();
    assert_eq!(unauthorized.status(), 401);
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
        let mut runtime = Runtime::start_with_decisions(1, 0, 120, None, url, "", None);
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
        let mut runtime = Runtime::start_with_decisions(1, 0, 120, None, url, raw, None);
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

/// A one-request router stand-in: records the request line, headers and body,
/// then answers with a fixed Messages response.
/// The recorded request line, lower-cased headers and JSON body.
type Recorded = (String, Vec<(String, String)>, Value);

fn fake_router() -> (String, std::thread::JoinHandle<Recorded>) {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
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
        let answer = json!({"id": "msg_cloud", "type": "message", "role": "assistant",
            "content": [{"type": "text", "text": "<block>no</block>"}]})
        .to_string();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nx-rayline-selected-model: anthropic/claude-sonnet-5.5\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{answer}",
            answer.len()
        )
        .unwrap();
        (line, headers, serde_json::from_slice(&body).unwrap())
    });
    (url, handle)
}

#[tokio::test]
async fn auto_mode_classifier_goes_to_haiku_on_the_cloud_router_and_other_turns_to_the_core() {
    let (router, recorded) = fake_router();
    let deployments = format!(
        r#","deployments":[{{"id":"local","kind":"local","url":"http://127.0.0.1:9/v1/chat/completions","protocol":"openai.chat.v1","wire_model":"glm"}},{{"id":"cloud-opus","kind":"cloud","cloud_transport":"model_pin","url":"{router}/v1/messages","protocol":"anthropic.messages.v1","wire_model":"anthropic/claude-opus-5.5","auth_env":"RAYLINE_ROUTER_API_KEY"}},{{"id":"cloud-sonnet","kind":"cloud","cloud_transport":"model_pin","url":"{router}/v1/messages","protocol":"anthropic.messages.v1","wire_model":"anthropic/claude-sonnet-5.5","auth_env":"RAYLINE_ROUTER_API_KEY"}}]"#
    );
    let mut runtime = Runtime::start_with_decisions(
        1,
        0,
        120,
        None,
        &format!("{router}/v1/routes"),
        &deployments,
        Some("rlk-test-key"),
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
    let (line, headers, body) = recorded.join().unwrap();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    };
    assert!(line.starts_with("POST /v1/messages "), "{line}");
    assert_eq!(header("authorization"), Some("Bearer rlk-test-key"));
    // Claude Haiku 5.5 by default, through the cloud pins' router and key.
    assert_eq!(
        header("x-workshop-required-model"),
        Some("anthropic/claude-haiku-5.5")
    );
    assert_eq!(header("anthropic-beta"), Some("claude-code-20250219"));
    assert_eq!(header("x-conversation-id"), Some("session-one"));
    let mut expected = classifier.clone();
    expected["model"] = json!("anthropic/claude-haiku-5.5");
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
    let events = fs::read_to_string(runtime.dir.join("events.jsonl")).unwrap();
    assert_eq!(
        events.lines().count(),
        1,
        "only the ordinary turn reaches the core"
    );
}
