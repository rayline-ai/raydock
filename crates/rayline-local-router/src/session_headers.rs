//! The client session headers the hosted Rayline router keys episodes
//! (conversations) on, and the one rule for carrying them upstream: copy them
//! verbatim, and only to the hosted router. A session id the client sent only
//! in its body (`session_id`, `prompt_cache_key`) is carried in
//! `x-session-affinity` where the local router rebuilds that body for another
//! protocol, under the same rule.

use hyper::HeaderMap;
use serde_json::Value;

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

/// The header a body-borne session id travels in once the local router has
/// rebuilt the body. The hosted router reads it on every protocol, at the same
/// rank as a body `session_id`, and it is one of `SESSION_HEADERS`.
const BODY_SESSION_ID_HEADER: &str = "x-session-affinity";

/// The longest body `session_id` carried: the hosted router reads no longer
/// one either.
const BODY_SESSION_ID_LIMIT: usize = 1024;

/// For a builder that rebuilds the client's body for another protocol (Chat to
/// Messages, Responses to Messages or Chat, Messages to Chat): copy the
/// client's session headers ([`copy_session_headers`]) and, when the client
/// named its session only in the body, carry that id in `x-session-affinity`.
/// The rebuilt body drops `session_id` and `prompt_cache_key`, so without this
/// the hosted router would key the turn on an id it derives from the
/// messages and separate sessions would merge. A builder that forwards the
/// client's body as is keeps those fields and must use
/// [`copy_session_headers`] alone.
pub(crate) fn copy_session_ids_for_rebuilt_body(
    outbound: reqwest::RequestBuilder,
    inbound_headers: &HeaderMap,
    inbound_body: &Value,
    endpoint: &EndpointConfig,
) -> reqwest::RequestBuilder {
    let outbound = copy_session_headers(outbound, inbound_headers, endpoint);
    if !endpoint_receives_session_headers(endpoint) {
        return outbound;
    }
    // Any session header already on the request, from the client or the
    // endpoint config, names the session: a body id never overrides it.
    let header_names_session = SESSION_HEADERS.iter().any(|name| {
        inbound_headers.contains_key(*name)
            || endpoint
                .headers
                .keys()
                .any(|configured| configured.eq_ignore_ascii_case(name))
    });
    if header_names_session {
        return outbound;
    }
    match body_session_id(inbound_headers, inbound_body) {
        Some(id) => outbound.header(BODY_SESSION_ID_HEADER, id),
        None => outbound,
    }
}

/// The session id the client's body names, picked the way the hosted router
/// would pick it from that body on a direct connection, so going through the
/// local router never changes which id keys the conversation:
///
/// - a client session header outranks every body id (the caller checks that
///   first). On a direct connection the one body id ranked above those headers
///   is Codex's `prompt_cache_key`, and Codex sends none of them;
/// - Codex (an `originator` header starting with `codex`) on a body without
///   `messages`: `prompt_cache_key`, which the router reads ahead of any
///   `session_id` for Codex;
/// - otherwise the body `session_id` (OpenRouter's field; Cline's OpenRouter
///   provider sends its task id there), up to 1024 characters;
/// - else `prompt_cache_key`, but only on a body without `messages` (a
///   Responses body): the router ignores it on Chat and Messages bodies.
///
/// A value is trimmed and must be non-empty printable ASCII; anything else is
/// skipped, as a header could not carry it unchanged.
fn body_session_id(inbound_headers: &HeaderMap, body: &Value) -> Option<String> {
    let field = |name: &str, limit: usize| {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| {
                !id.is_empty() && id.len() <= limit && id.bytes().all(|b| (0x20..0x7f).contains(&b))
            })
            .map(str::to_owned)
    };
    let has_messages = body.get("messages").is_some_and(Value::is_array);
    // The router reads a `prompt_cache_key` of any length.
    let prompt_cache_key = || {
        if has_messages {
            None
        } else {
            field("prompt_cache_key", usize::MAX)
        }
    };
    let codex = inbound_headers
        .get("originator")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().starts_with("codex"));
    if codex && let Some(id) = prompt_cache_key() {
        return Some(id);
    }
    field("session_id", BODY_SESSION_ID_LIMIT).or_else(prompt_cache_key)
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
            // Read the head and the whole body: tests assert on body fields.
            let mut raw = Vec::new();
            let mut buf = [0u8; 16384];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw);
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(key, _)| key.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            let _ = tx.send(String::from_utf8_lossy(&raw).to_string());
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
        session_case_with(
            inbound_path,
            protocol,
            hosted,
            endpoint_headers,
            &SESSION_HEADER_VALUES,
            json!({}),
        )
        .await
    }

    /// `session_case` with the client's own headers and extra top-level body
    /// fields.
    async fn session_case_with(
        inbound_path: &str,
        protocol: &str,
        hosted: bool,
        endpoint_headers: Value,
        client_headers: &[(&str, &str)],
        body_fields: Value,
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
        let mut body = match inbound_path {
            "/v1/messages" => json!({"model": "rayline-auto", "max_tokens": 16,
                                     "messages": [{"role": "user", "content": "hi"}]}),
            "/v1/chat/completions" => json!({"model": "rayline-auto",
                                             "messages": [{"role": "user", "content": "hi"}]}),
            "/v1/memories/trace_summarize" => json!({"model": "rayline-auto",
                                                     "traces": [{"id": "t1", "text": "hi"}]}),
            _ => json!({"model": "rayline-auto", "stream": false, "input": "hi"}),
        };
        if let (Some(body), Some(fields)) = (body.as_object_mut(), body_fields.as_object()) {
            body.extend(fields.clone());
        }
        let mut request = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}{inbound_path}"))
            .header("anthropic-version", "2023-06-01")
            .header("authorization", "Bearer client-secret-must-not-leave")
            .header("x-api-key", "client-secret-must-not-leave")
            .json(&body);
        for (name, value) in client_headers {
            request = request.header(*name, *value);
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

    // -----------------------------------------------------------------------
    // Body-borne session ids, where the router rebuilds the body.
    // -----------------------------------------------------------------------

    /// Surfaces where the router rebuilds the client's body for another
    /// protocol, dropping its `session_id` and `prompt_cache_key`.
    const REBUILT_BODY_SURFACES: [(&str, &str); 8] = [
        ("/v1/chat/completions", "anthropic_messages"),
        ("/v1/responses", "anthropic_messages"),
        ("/v1/responses", "openai_chat"),
        ("/v1/responses/compact", "anthropic_messages"),
        ("/v1/responses/compact", "openai_chat"),
        ("/v1/memories/trace_summarize", "anthropic_messages"),
        ("/v1/memories/trace_summarize", "openai_chat"),
        ("/v1/messages", "openai_chat"),
    ];

    /// Surfaces where the router forwards the client's body as is.
    const PASSTHROUGH_SURFACES: [(&str, &str); 3] = [
        ("/v1/chat/completions", "openai_chat"),
        ("/v1/responses", "openai_responses"),
        ("/v1/messages", "anthropic_messages"),
    ];

    /// Whether the client's body on `inbound_path` has no `messages` array, so
    /// the router reads its `prompt_cache_key`.
    fn is_responses(inbound_path: &str) -> bool {
        inbound_path.starts_with("/v1/responses") || inbound_path.starts_with("/v1/memories")
    }

    async fn body_id_case(
        inbound_path: &str,
        protocol: &str,
        hosted: bool,
        client_headers: &[(&str, &str)],
        body_fields: Value,
    ) -> String {
        session_case_with(
            inbound_path,
            protocol,
            hosted,
            json!({}),
            client_headers,
            body_fields,
        )
        .await
    }

    /// A client that names its session only in the body `session_id` (Cline's
    /// OpenRouter provider) keeps it where the body is rebuilt: the hosted
    /// router receives it as `x-session-affinity`, on every such surface.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn body_session_id_reaches_the_hosted_router_where_the_body_is_rebuilt() {
        for (inbound_path, protocol) in REBUILT_BODY_SURFACES {
            let raw = body_id_case(
                inbound_path,
                protocol,
                true,
                &[],
                json!({"session_id": "task-1"}),
            )
            .await;
            assert_eq!(
                captured_header_values(&raw, "x-session-affinity"),
                ["task-1"],
                "{inbound_path} -> {protocol}"
            );
            assert!(
                !raw.contains("client-secret-must-not-leave"),
                "{inbound_path} -> {protocol} forwarded client credentials"
            );
        }
    }

    /// A Responses `prompt_cache_key` (Codex, Hermes, pi) is carried the same
    /// way. On a Chat or Messages body the router does not read it, so it is
    /// not carried from those either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn prompt_cache_key_is_carried_from_a_responses_body_only() {
        for (inbound_path, protocol) in REBUILT_BODY_SURFACES {
            let raw = body_id_case(
                inbound_path,
                protocol,
                true,
                &[],
                json!({"prompt_cache_key": "pck-1"}),
            )
            .await;
            let expected: &[&str] = if is_responses(inbound_path) {
                &["pck-1"]
            } else {
                &[]
            };
            assert_eq!(
                captured_header_values(&raw, "x-session-affinity"),
                expected,
                "{inbound_path} -> {protocol}"
            );
        }
    }

    /// No body id, no header: the router derives its own id as before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_body_id_adds_no_session_header() {
        for (inbound_path, protocol) in REBUILT_BODY_SURFACES {
            let raw = body_id_case(inbound_path, protocol, true, &[], json!({})).await;
            for name in SESSION_HEADERS {
                assert!(
                    captured_header_values(&raw, name).is_empty(),
                    "{inbound_path} -> {protocol}: {name}"
                );
            }
        }
    }

    /// Codex names its session in `prompt_cache_key`, which the router reads
    /// ahead of a body `session_id` for Codex; any other client's
    /// `session_id` wins over its `prompt_cache_key`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn body_ids_keep_the_hosted_router_precedence() {
        let both = json!({"session_id": "task-1", "prompt_cache_key": "pck-1"});
        let raw = body_id_case(
            "/v1/responses",
            "anthropic_messages",
            true,
            &[("originator", "codex_cli_rs")],
            both.clone(),
        )
        .await;
        assert_eq!(
            captured_header_values(&raw, "x-session-affinity"),
            ["pck-1"]
        );
        let raw = body_id_case("/v1/responses", "anthropic_messages", true, &[], both).await;
        assert_eq!(
            captured_header_values(&raw, "x-session-affinity"),
            ["task-1"]
        );
    }

    /// A session header the client sent keeps priority: the body id is not
    /// added next to it, and the client's header reaches the router unchanged.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_session_header_wins_over_a_body_id() {
        let body = json!({"session_id": "task-1", "prompt_cache_key": "pck-1"});
        for (inbound_path, protocol) in REBUILT_BODY_SURFACES {
            for (name, value) in SESSION_HEADER_VALUES {
                let raw =
                    body_id_case(inbound_path, protocol, true, &[(name, value)], body.clone())
                        .await;
                assert_eq!(
                    captured_header_values(&raw, name),
                    [value],
                    "{inbound_path} -> {protocol}: {name}"
                );
                if name != BODY_SESSION_ID_HEADER {
                    assert!(
                        captured_header_values(&raw, BODY_SESSION_ID_HEADER).is_empty(),
                        "{inbound_path} -> {protocol}: body id added next to {name}"
                    );
                }
            }
        }
    }

    /// A session header the endpoint config sets wins over a body id too.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn endpoint_configured_session_header_wins_over_a_body_id() {
        let raw = session_case_with(
            "/v1/chat/completions",
            "anthropic_messages",
            true,
            json!({"X-Session-Affinity": "configured"}),
            &[],
            json!({"session_id": "task-1"}),
        )
        .await;
        assert_eq!(
            captured_header_values(&raw, "x-session-affinity"),
            ["configured"]
        );
    }

    /// Any upstream but the hosted router never receives a body id as a header.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn body_ids_never_reach_other_upstreams_as_headers() {
        for (inbound_path, protocol) in REBUILT_BODY_SURFACES {
            let raw = body_id_case(
                inbound_path,
                protocol,
                false,
                &[("originator", "codex_cli_rs")],
                json!({"session_id": "task-1", "prompt_cache_key": "pck-1"}),
            )
            .await;
            for name in SESSION_HEADERS {
                assert!(
                    captured_header_values(&raw, name).is_empty(),
                    "{inbound_path} -> {protocol} leaked {name}"
                );
            }
        }
    }

    /// Where the body is forwarded as is, it still carries the id and no
    /// header is added: the router reads the body itself.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn passthrough_body_keeps_its_id_and_gets_no_header() {
        for (inbound_path, protocol) in PASSTHROUGH_SURFACES {
            let raw = body_id_case(
                inbound_path,
                protocol,
                true,
                &[],
                json!({"session_id": "task-1"}),
            )
            .await;
            assert!(
                captured_header_values(&raw, "x-session-affinity").is_empty(),
                "{inbound_path} -> {protocol}"
            );
            let body = raw.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
            let body: Value = serde_json::from_str(body).expect("upstream body is JSON");
            assert_eq!(body["session_id"], "task-1", "{inbound_path} -> {protocol}");
        }
    }

    fn picked(headers: &[(&'static str, &'static str)], body: Value) -> Option<String> {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(
                hyper::header::HeaderName::from_static(name),
                hyper::header::HeaderValue::from_static(value),
            );
        }
        body_session_id(&map, &body)
    }

    #[test]
    fn body_session_id_follows_the_hosted_ladder() {
        let codex = [("originator", "codex_exec")];
        // Responses body: Codex prefers prompt_cache_key, anyone else session_id.
        let both = json!({"input": "hi", "session_id": "s", "prompt_cache_key": "p"});
        assert_eq!(picked(&codex, both.clone()).as_deref(), Some("p"));
        assert_eq!(picked(&[], both).as_deref(), Some("s"));
        assert_eq!(
            picked(&[], json!({"input": "hi", "prompt_cache_key": "p"})).as_deref(),
            Some("p")
        );
        // A body with messages (Chat, Messages): prompt_cache_key is not read,
        // for Codex either.
        let chat = json!({"messages": [], "prompt_cache_key": "p"});
        assert_eq!(picked(&[], chat.clone()), None);
        assert_eq!(picked(&codex, chat), None);
        assert_eq!(
            picked(
                &codex,
                json!({"messages": [], "session_id": "s", "prompt_cache_key": "p"})
            )
            .as_deref(),
            Some("s")
        );
        assert_eq!(picked(&[], json!({"input": "hi"})), None);
    }

    #[test]
    fn body_session_id_skips_values_a_header_cannot_carry_unchanged() {
        // Trimmed; a blank value falls through to the next id.
        assert_eq!(
            picked(&[], json!({"session_id": "  s  "})).as_deref(),
            Some("s")
        );
        assert_eq!(
            picked(&[], json!({"session_id": "  ", "prompt_cache_key": "p"})).as_deref(),
            Some("p")
        );
        // Longer than the router reads from a body, non-ASCII, control
        // characters, or not a string: skipped.
        let long = "a".repeat(BODY_SESSION_ID_LIMIT + 1);
        assert_eq!(picked(&[], json!({"session_id": long})), None);
        let at_limit = "a".repeat(BODY_SESSION_ID_LIMIT);
        assert_eq!(
            picked(&[], json!({"session_id": at_limit.clone()})),
            Some(at_limit)
        );
        assert_eq!(picked(&[], json!({"session_id": "s\u{e9}"})), None);
        // The limit is the router's for session_id only: a longer
        // prompt_cache_key is still read, and still wins for Codex.
        let long_key = "k".repeat(BODY_SESSION_ID_LIMIT + 1);
        assert_eq!(
            picked(
                &[],
                json!({"input": "hi", "prompt_cache_key": long_key.clone()})
            ),
            Some(long_key.clone())
        );
        assert_eq!(
            picked(
                &[("originator", "codex_exec")],
                json!({"input": "hi", "session_id": "s", "prompt_cache_key": long_key.clone()})
            ),
            Some(long_key)
        );
        assert_eq!(picked(&[], json!({"session_id": "a\nb"})), None);
        assert_eq!(picked(&[], json!({"session_id": 7})), None);
    }
}
