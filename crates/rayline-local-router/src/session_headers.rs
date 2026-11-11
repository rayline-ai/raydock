//! The client session headers the hosted Rayline router keys episodes
//! (conversations) on, and the one rule for carrying them upstream: copy them
//! verbatim, and only to the hosted router.

use hyper::HeaderMap;

use crate::{EndpointConfig, endpoint_is_hosted_rcr};

/// Client session headers the hosted router keys episodes (conversations) on.
/// A client that names its session only in a header would otherwise be keyed
/// on a derived id once its request passes through a local gateway. The local
/// router and the hybrid gateway's cloud classifier pin both forward exactly
/// this set.
pub const SESSION_HEADERS: [&str; 4] = [
    "x-session-affinity",
    "x-claude-code-session-id",
    "x-session-id",
    "x-conversation-id",
];

/// Whether an upstream may receive the client's session headers: only the
/// hosted Rayline router, which is what keys on them. A provider such as
/// api.anthropic.com, OpenRouter or a custom upstream never sees them.
pub(crate) fn endpoint_receives_session_headers(endpoint: &EndpointConfig) -> bool {
    if endpoint_is_hosted_rcr(&endpoint.base_url) {
        return true;
    }
    #[cfg(test)]
    if tests::stands_in_for_hosted_router(&endpoint.base_url) {
        return true;
    }
    false
}

/// Copy exactly the client's session headers (`SESSION_HEADERS`) onto a request
/// bound for the hosted router. Nothing else is copied: auth and every other
/// client header keep their per-builder policy. A header the endpoint config
/// sets itself is left to the config.
pub(crate) fn copy_session_headers(
    mut outbound: reqwest::RequestBuilder,
    inbound_headers: &HeaderMap,
    endpoint: &EndpointConfig,
) -> reqwest::RequestBuilder {
    if !endpoint_receives_session_headers(endpoint) {
        return outbound;
    }
    for name in SESSION_HEADERS {
        if endpoint
            .headers
            .keys()
            .any(|configured| configured.eq_ignore_ascii_case(name))
        {
            continue;
        }
        if let Some(value) = inbound_headers.get(name) {
            outbound = outbound.header(name, value.clone());
        }
    }
    outbound
}

#[cfg(test)]
mod tests {
    //! A mock upstream on loopback stands in for the hosted router by
    //! registering its own `host:port`, so the hosted and non-hosted cases run
    //! side by side without any switch in the shipped router.

    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;

    use super::*;
    use crate::{EndpointProtocol, LocalRouterOptions, default_endpoint_kind, serve};

    /// `host:port` of mock upstreams that stand in for the hosted router.
    static HOSTED_STAND_INS: Mutex<Vec<String>> = Mutex::new(Vec::new());

    fn authority(base_url: &str) -> Option<String> {
        let url = reqwest::Url::parse(base_url).ok()?;
        Some(format!(
            "{}:{}",
            url.host_str()?,
            url.port_or_known_default()?
        ))
    }

    pub(super) fn stands_in_for_hosted_router(base_url: &str) -> bool {
        authority(base_url).is_some_and(|authority| {
            HOSTED_STAND_INS
                .lock()
                .expect("stand-in list lock")
                .contains(&authority)
        })
    }

    fn endpoint(base_url: &str) -> EndpointConfig {
        EndpointConfig {
            id: "up".to_owned(),
            kind: default_endpoint_kind(),
            protocol: EndpointProtocol::AnthropicMessages,
            base_url: base_url.to_owned(),
            api_key_env: None,
            models: Vec::new(),
            headers: HashMap::new(),
            auth: None,
        }
    }

    #[test]
    fn only_the_hosted_router_receives_client_session_headers() {
        assert!(endpoint_receives_session_headers(&endpoint(
            "https://api.rayline.ai"
        )));
        assert!(endpoint_receives_session_headers(&endpoint(
            "https://api-dev.rayline.ai/v1"
        )));
        for other in [
            "https://api.anthropic.com",
            "https://openrouter.ai/api/v1",
            "https://api.openai.com/v1",
            "https://chatgpt.com/backend-api/codex",
            "http://127.0.0.1:8080",
            "http://localhost:8080/v1",
            "http://[::1]:8080",
            "https://api.rayline.ai.example.com",
        ] {
            assert!(
                !endpoint_receives_session_headers(&endpoint(other)),
                "{other}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // Through the full HTTP path, on every surface the router forwards on.
    // -----------------------------------------------------------------------

    /// The session headers a client names its session with, and the value each
    /// test request sends.
    const SESSION_HEADER_VALUES: [(&str, &str); 4] = [
        ("x-session-affinity", "affinity-1"),
        ("x-claude-code-session-id", "claude-session-1"),
        ("x-session-id", "session-1"),
        ("x-conversation-id", "conversation-1"),
    ];

    #[test]
    fn test_values_cover_every_session_header() {
        let names: Vec<&str> = SESSION_HEADER_VALUES.iter().map(|(n, _)| *n).collect();
        assert_eq!(names, SESSION_HEADERS);
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    async fn serve_once_capture(
        listener: TcpListener,
        response: String,
        tx: oneshot::Sender<String>,
    ) {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 16384];
            let n = sock.read(&mut buf).await.unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&buf[..n]).to_string());
            let _ = sock.write_all(response.as_bytes()).await;
            let _ = sock.flush().await;
            let _ = sock.shutdown().await;
        }
    }

    fn http_json(body: &Value) -> String {
        let body = body.to_string();
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        )
    }

    fn write_config(config: &Value) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "rayline-session-headers-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, serde_json::to_vec_pretty(config).unwrap()).unwrap();
        path
    }

    /// Start a router on `port` and wait until healthy.
    async fn start_router(port: u16, config_path: PathBuf) {
        let opts = LocalRouterOptions {
            port,
            config_path: Some(config_path),
            ..LocalRouterOptions::default()
        };
        tokio::spawn(async move {
            let _ = serve(opts).await;
        });
        let client = reqwest::Client::new();
        for _ in 0..100 {
            if client
                .get(format!("http://127.0.0.1:{port}/healthz"))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("router on port {port} did not become healthy");
    }

    /// Every value of `name` in a captured raw request's head.
    fn captured_header_values(raw: &str, name: &str) -> Vec<String> {
        let head = raw.split("\r\n\r\n").next().unwrap_or_default();
        head.lines()
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .filter(|(key, _)| key.trim().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.trim().to_owned())
            .collect()
    }

    /// An upstream reply in `protocol`'s non-streaming shape.
    fn session_case_reply(protocol: &str) -> String {
        match protocol {
            "anthropic_messages" => http_json(&json!({
                "id": "msg_mock", "type": "message", "role": "assistant", "model": "m",
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn", "usage": {"input_tokens": 3, "output_tokens": 1}
            })),
            "openai_chat" => http_json(&json!({
                "id": "chatcmpl-mock", "object": "chat.completion", "created": 1, "model": "m",
                "choices": [{"index": 0, "finish_reason": "stop",
                             "message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            })),
            _ => http_json(&json!({
                "id": "resp_mock", "object": "response", "status": "completed", "model": "m",
                "output": [{"type": "message", "role": "assistant",
                            "content": [{"type": "output_text", "text": "ok"}]}],
                "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
            })),
        }
    }

    /// Send one request carrying every session header plus client credentials
    /// to `inbound_path`, routed to a mock `protocol` endpoint that stands in
    /// for the hosted router (`hosted`) or for any other provider, and return
    /// the raw request the upstream received.
    async fn session_case(
        inbound_path: &str,
        protocol: &str,
        hosted: bool,
        endpoint_headers: Value,
    ) -> String {
        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_port = upstream.local_addr().unwrap().port();
        if hosted {
            HOSTED_STAND_INS
                .lock()
                .expect("stand-in list lock")
                .push(format!("127.0.0.1:{up_port}"));
        }
        let (tx, rx) = oneshot::channel();
        tokio::spawn(serve_once_capture(
            upstream,
            session_case_reply(protocol),
            tx,
        ));
        let base_url = if protocol == "anthropic_messages" {
            format!("http://127.0.0.1:{up_port}")
        } else {
            format!("http://127.0.0.1:{up_port}/v1")
        };
        let path = write_config(&json!({
            "endpoints": [{"id": "up", "protocol": protocol, "base_url": base_url,
                           "models": ["m"], "headers": endpoint_headers}],
            "routes": {"main": {"endpoint": "up", "model": "m"},
                       "default": {"endpoint": "up", "model": "m"}}
        }));
        let port = free_port();
        start_router(port, path.clone()).await;
        let body = match inbound_path {
            "/v1/messages" => json!({"model": "rayline-auto", "max_tokens": 16,
                                     "messages": [{"role": "user", "content": "hi"}]}),
            "/v1/chat/completions" => json!({"model": "rayline-auto",
                                             "messages": [{"role": "user", "content": "hi"}]}),
            _ => json!({"model": "rayline-auto", "stream": false, "input": "hi"}),
        };
        let mut request = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}{inbound_path}"))
            .header("anthropic-version", "2023-06-01")
            .header("authorization", "Bearer client-secret-must-not-leave")
            .header("x-api-key", "client-secret-must-not-leave")
            .json(&body);
        for (name, value) in SESSION_HEADER_VALUES {
            request = request.header(name, value);
        }
        let _ = request.send().await.expect("router request");
        let _ = std::fs::remove_file(path);
        rx.await.expect("upstream saw the request")
    }

    /// Every inbound surface and upstream protocol the router forwards on.
    const SESSION_HEADER_SURFACES: [(&str, &str); 9] = [
        ("/v1/messages", "anthropic_messages"),
        ("/v1/messages", "openai_chat"),
        ("/v1/responses", "anthropic_messages"),
        ("/v1/responses", "openai_chat"),
        ("/v1/responses", "openai_responses"),
        ("/v1/responses/compact", "anthropic_messages"),
        ("/v1/responses/compact", "openai_chat"),
        ("/v1/chat/completions", "anthropic_messages"),
        ("/v1/chat/completions", "openai_chat"),
    ];

    /// A client behind this router that names its session only in a header
    /// keeps that session on the hosted router: each session header reaches
    /// the upstream once, unchanged, on every surface. The client's own
    /// credentials never do.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_headers_reach_the_hosted_router_on_every_surface() {
        for (inbound_path, protocol) in SESSION_HEADER_SURFACES {
            let raw = session_case(inbound_path, protocol, true, json!({})).await;
            for (name, value) in SESSION_HEADER_VALUES {
                assert_eq!(
                    captured_header_values(&raw, name),
                    [value],
                    "{inbound_path} -> {protocol}: {name}"
                );
            }
            assert!(
                !raw.contains("client-secret-must-not-leave"),
                "{inbound_path} -> {protocol} forwarded client credentials"
            );
        }
    }

    /// Any upstream but the hosted router (a provider, OpenRouter, a custom
    /// endpoint) never sees the client's session ids.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_headers_never_reach_other_upstreams() {
        for (inbound_path, protocol) in SESSION_HEADER_SURFACES {
            let raw = session_case(inbound_path, protocol, false, json!({})).await;
            for (name, _) in SESSION_HEADER_VALUES {
                assert!(
                    captured_header_values(&raw, name).is_empty(),
                    "{inbound_path} -> {protocol} leaked {name}"
                );
            }
            assert!(
                !raw.contains("client-secret-must-not-leave"),
                "{inbound_path} -> {protocol} forwarded client credentials"
            );
        }
    }

    /// A session header the endpoint config sets itself is the config's: the
    /// client's value is not appended next to it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_configured_session_header_wins_over_the_client() {
        let raw = session_case(
            "/v1/messages",
            "anthropic_messages",
            true,
            json!({"X-Session-Id": "configured"}),
        )
        .await;
        assert_eq!(captured_header_values(&raw, "x-session-id"), ["configured"]);
        assert_eq!(
            captured_header_values(&raw, "x-session-affinity"),
            ["affinity-1"]
        );
    }
}
