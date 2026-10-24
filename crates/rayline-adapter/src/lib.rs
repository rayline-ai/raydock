//! Pass-through proxy for Anthropic-native local-model traffic.
//!
//! llama-server (`--jinja`) already serves the Anthropic Messages API at
//! `/v1/messages`, including native `tool_use` blocks. We just forward the
//! request body as-is, then snoop usage tokens off the response (or SSE
//! stream) to fire the cloud `/v1/usage/update` callback.
//!
//! We intentionally do NOT translate to/from OpenAI chat-completions — that
//! round-trip loses tool-call structure for templated models like Qwen3.

use std::collections::HashMap;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

pub use rayline_authcache::AuthCache;

use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_metrics::{MetricsUpdate, REQUEST_ID_HEADER, SharedMetricsSink, new_request_id};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

pub const DEFAULT_PORT: u16 = 20808;
pub const DEFAULT_TARGET: &str = "http://127.0.0.1:8081";
pub const DEFAULT_UPSTREAM_MODEL: &str = "mlx-community/Qwen3.6-35B-A3B-4bit";
// Library-level fallback only. The shipping daemon (`rayline-daemon`) normally
// overrides this from CLI/runtime config. Mirrors `rayline-proxy`'s
// `DEFAULT_ROUTER_URL`.
pub const DEFAULT_ROUTER_URL: &str = "https://api.rayline.ai";

// AuthCache is re-exported from rayline_authcache above.

#[derive(Clone)]
pub struct AdapterOptions {
    pub port: u16,
    pub target: String,
    /// Reported in `/v1/usage/update.selectedModel` when the upstream doesn't
    /// echo a model name. llama-server typically ignores the client-supplied
    /// `model` field and returns whatever GGUF it loaded.
    pub upstream_model: String,
    pub router_url: String,
    pub auth_cache: Option<AuthCache>,
    pub metrics: Option<SharedMetricsSink>,
    /// Enable llama.cpp-only progress frames (`return_progress`) so we can
    /// compute prompt cache reuse. Leave disabled for arbitrary custom
    /// Anthropic-compatible endpoints, which may reject unknown request fields.
    pub collect_llama_progress: bool,
}

impl Default for AdapterOptions {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            target: DEFAULT_TARGET.to_string(),
            upstream_model: DEFAULT_UPSTREAM_MODEL.to_string(),
            router_url: DEFAULT_ROUTER_URL.trim_end_matches('/').to_string(),
            auth_cache: None,
            metrics: None,
            collect_llama_progress: false,
        }
    }
}

#[derive(Clone)]
struct AppState {
    opts: Arc<AdapterOptions>,
    http: reqwest::Client,
    started_at: String,
}

/// Bind on 127.0.0.1:`opts.port` and serve until the process exits.
pub async fn serve(opts: AdapterOptions) -> Result<()> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), opts.port);
    let listener = TcpListener::bind(addr).await?;
    let started_at = chrono_like_now();
    info!(
        "adapter listening on 127.0.0.1:{} → {} (Anthropic-native passthrough)",
        opts.port, opts.target
    );
    let state = AppState {
        opts: Arc::new(opts),
        http: reqwest::Client::builder().build()?,
        started_at,
    };
    loop {
        let (stream, _) = listener.accept().await?;
        let io = TokioIo::new(stream);
        let state = state.clone();
        tokio::spawn(async move {
            let svc = service_fn(move |req| {
                let state = state.clone();
                async move { Ok::<_, Infallible>(handle(state, req).await) }
            });
            if let Err(e) = auto::Builder::new(TokioExecutor::new())
                .serve_connection(io, svc)
                .await
            {
                warn!("connection error: {e}");
            }
        });
    }
}

type BoxBody = http_body_util::combinators::BoxBody<Bytes, std::io::Error>;

fn full_body(s: impl Into<Bytes>) -> BoxBody {
    Full::new(s.into()).map_err(|never| match never {}).boxed()
}

fn json_response(status: StatusCode, value: Value) -> Response<BoxBody> {
    let body = serde_json::to_vec(&value).unwrap_or_else(|_| b"{}".to_vec());
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(full_body(body))
        .unwrap()
}

async fn handle(state: AppState, req: Request<Incoming>) -> Response<BoxBody> {
    let path = req.uri().path().to_string();
    let method = req.method().clone();
    match (method.clone(), path.as_str()) {
        (Method::GET, "/healthz") => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "target": state.opts.target,
                "port": state.opts.port,
            }),
        ),
        (Method::GET, "/api/v1/ping") => json_response(
            StatusCode::OK,
            json!({
                "ok": true,
                "runtime": "rayline-adapter",
                "port": state.opts.port,
                "startedAt": state.started_at,
            }),
        ),
        // `/api/v1/messages` is the path the cloud router 307s to (with
        // ?usage_doc_id=…). `/v1/messages` is the path a client points at us
        // directly when bypassing the router (e.g., ANTHROPIC_BASE_URL=:20808).
        // Both map to the same handler — usage callback is gated on the query
        // param so direct calls just skip it.
        (Method::POST, "/api/v1/messages" | "/v1/messages") => {
            match handle_messages(state, req).await {
                Ok(r) => r,
                Err(e) => {
                    error!("handler error: {e}");
                    json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"type": "error", "error": {"type": "api_error", "message": e.to_string()}}),
                    )
                }
            }
        }
        // `/api/v1/responses` is the path the cloud router 307s eligible Codex
        // subagent turns to (with ?usage_doc_id=…), sending a native OpenAI
        // Responses body. `/v1/responses` is the direct-client path. Both map to
        // `handle_responses`, a pure Responses passthrough (no Anthropic
        // thinking/tool-schema processing) — see its docs.
        (Method::POST, "/api/v1/responses" | "/v1/responses") => {
            match handle_responses(state, req).await {
                Ok(r) => r,
                Err(e) => {
                    error!("handler error: {e}");
                    json_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({"error": {"type": "api_error", "message": e.to_string()}}),
                    )
                }
            }
        }
        _ => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(full_body("not found"))
            .unwrap(),
    }
}

async fn handle_messages(state: AppState, req: Request<Incoming>) -> Result<Response<BoxBody>> {
    let t_start = Instant::now();
    let query = req.uri().query().unwrap_or("").to_string();
    let query_params = parse_query(&query);
    let usage_doc_id = query_params.get("usage_doc_id").cloned();
    let query_request_id = query_params.get("rayline_request_id").cloned();
    let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or(query_request_id)
        .or_else(|| usage_doc_id.clone())
        .unwrap_or_else(new_request_id);
    // Auth for the /v1/usage/update callback. The cloud router only accepts a
    // Rayline router key or a Firebase ID token. After the cross-origin 307
    // to 127.0.0.1, Claude Code re-attaches its Max-plan OAuth JWT in
    // `Authorization` but drops the `x-api-key` router key, so the inbound
    // headers that land here carry a credential the router rejects ("Decoding
    // Firebase ID token failed"). The injector stashed the original
    // pre-redirect headers keyed by usage_doc_id, so PREFER the stash whenever
    // we have a doc id; fall back to inbound only when the stash missed (cache
    // evicted, or a direct call with no doc id).
    let mut auth_headers = collect_auth_headers(req.headers());
    if let (Some(doc_id), Some(cache)) = (usage_doc_id.as_ref(), state.opts.auth_cache.as_ref()) {
        if let Ok(mut guard) = cache.lock() {
            if let Some(stashed) = guard.remove(doc_id) {
                auth_headers = stashed;
            }
        }
    }

    let body_bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                json!({"type":"error","error":{"type":"invalid_request_error","message":format!("read body: {e}")}}),
            ));
        }
    };

    // Peek at request shape for logging + stream detection, and to sanitize
    // tool schemas below. Parse failure is non-fatal — we forward raw bytes.
    let mut parsed: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let want_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let client_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let msg_count = parsed
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let estimated_input_tokens = approximate_input_tokens(&parsed);

    let upstream_url = format!("{}/v1/messages", state.opts.target);
    info!(
        "/api/v1/messages → {} (client-model={} stream={} msgs={})",
        upstream_url, client_model, want_stream, msg_count
    );
    if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: request_id.clone(),
            source: "adapter".to_owned(),
            requested_model: Some(client_model.clone()),
            agent_id: None,
            agent_type: None,
            // The adapter serves the local model behind the router, which
            // already attributes the conversation on the inbound request.
            session_id: None,
        });
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: request_id.clone(),
            route_id: usage_doc_id.clone(),
            target: "local".to_owned(),
            endpoint_id: Some("local".to_owned()),
            selected_model: Some(state.opts.upstream_model.clone()),
            requested_model: Some(client_model.clone()),
            policy: Some("local-adapter".to_owned()),
            task_class: None,
            agent_id: None,
            agent_type: None,
            session_id: None,
        });
        metrics.record(MetricsUpdate::TokenUsage {
            request_id: request_id.clone(),
            input_tokens: Some(estimated_input_tokens),
            output_tokens: None,
            selected_model: Some(state.opts.upstream_model.clone()),
        });
    }

    // Substitute the configured upstream model into the request body before
    // forwarding. A normal `claude` launch sends the cloud/router model name;
    // custom endpoints (Ollama / LM Studio) reject a body whose `model` isn't a
    // model they serve, while the bundled llama-server ignores the field — so
    // rewriting it to `upstream_model` is correct for custom and safe for both.
    let substituted_model = if let Some(obj) = parsed.as_object_mut() {
        if obj.get("model").and_then(Value::as_str) == Some(state.opts.upstream_model.as_str()) {
            false
        } else {
            obj.insert(
                "model".to_owned(),
                Value::String(state.opts.upstream_model.clone()),
            );
            true
        }
    } else {
        false
    };

    // Map Claude Code's Anthropic `thinking` field onto the local template's
    // `chat_template_kwargs.enable_thinking` (see fn docs) so per-turn thinking
    // intent reaches the local model. Toggle off with `RAYLINE_MAP_THINKING=0`.
    let mapped_thinking =
        thinking_mapping_enabled() && map_thinking_to_template_kwargs(&mut parsed);
    let added_llama_progress =
        state.opts.collect_llama_progress && enable_llama_progress(&mut parsed);

    // Sanitize tool input_schemas before forwarding. llama-server's `--jinja`
    // grammar builder 400s the whole turn ("Unrecognized schema") on any schema
    // node with no recognized keyword — e.g. a typeless `{"description": ...}`
    // "any" parameter like Claude Code's `Workflow.args`. Coerce such nodes to
    // `{}` (an empty schema = "any"), which llama-server accepts. Re-serialize
    // when we substituted the model or changed a schema; else forward untouched.
    let sanitized = sanitize_tool_schemas(&mut parsed);
    let stripped_hosted = strip_local_unsupported_tools(&mut parsed);
    let forward_body = if substituted_model
        || sanitized > 0
        || stripped_hosted > 0
        || mapped_thinking
        || added_llama_progress
    {
        match serde_json::to_vec(&parsed) {
            Ok(b) => {
                if sanitized > 0 {
                    info!(
                        "sanitized {sanitized} tool-schema constraint(s) (typeless→{{}}, stripped pattern/format/bounds) for local model"
                    );
                }
                if stripped_hosted > 0 {
                    info!(
                        "stripped {stripped_hosted} local-unsupported hosted tool(s) (web_search) before local forward"
                    );
                }
                if mapped_thinking {
                    info!("mapped Anthropic thinking → chat_template_kwargs.enable_thinking");
                }
                if added_llama_progress {
                    info!("enabled llama.cpp prompt progress for local metrics");
                }
                Bytes::from(b)
            }
            Err(e) => {
                warn!("re-serialize before forward failed; forwarding raw: {e}");
                body_bytes.clone()
            }
        }
    } else {
        body_bytes.clone()
    };

    let upstream = match state
        .http
        .post(&upstream_url)
        .header("content-type", "application/json")
        .header("anthropic-version", "2023-06-01")
        .body(forward_body)
        .send()
        .await
    {
        Ok(u) => u,
        Err(e) => {
            // Local inference died before sending headers (server down/
            // restarting, process OOM/crash). Close the placeholder as an error
            // instead of letting `?` bypass the callback and leak the row.
            warn!("upstream request failed: {e}");
            if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: None,
                    error: format!("upstream request failed: {e}"),
                });
            }
            spawn_error_close(
                &state,
                &usage_doc_id,
                &auth_headers,
                t_start.elapsed().as_millis() as u64,
            );
            return Ok(json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "type": "error",
                    "error": {"type": "api_error", "message": format!("upstream request failed: {e}")}
                }),
            ));
        }
    };

    if !upstream.status().is_success() {
        let status = upstream.status();
        let text = upstream.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(500).collect();
        warn!("upstream {}: {}", status, snippet);
        if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
            metrics.record(MetricsUpdate::RequestErrored {
                request_id: request_id.clone(),
                status_code: Some(status.as_u16()),
                error: format!("upstream {status}: {snippet}"),
            });
        }
        // Close the placeholder as an error so a failed local turn (the common
        // llama-server 400 on an unsupported tool-schema grammar) doesn't leak
        // forever as a 0-token `local_redirect` row.
        spawn_error_close(
            &state,
            &usage_doc_id,
            &auth_headers,
            t_start.elapsed().as_millis() as u64,
        );
        return Ok(json_response(
            StatusCode::BAD_GATEWAY,
            json!({
                "type": "error",
                "error": {"type": "api_error", "message": format!("upstream {}: {}", status, snippet)}
            }),
        ));
    }

    if !want_stream {
        let resp_bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                // Body never fully arrived (connection dropped mid-read). Same
                // leak class as a pre-headers failure — close as error.
                warn!("upstream body read failed: {e}");
                if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
                    metrics.record(MetricsUpdate::RequestErrored {
                        request_id: request_id.clone(),
                        status_code: None,
                        error: format!("upstream body read failed: {e}"),
                    });
                }
                spawn_error_close(
                    &state,
                    &usage_doc_id,
                    &auth_headers,
                    t_start.elapsed().as_millis() as u64,
                );
                return Ok(json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "type": "error",
                        "error": {"type": "api_error", "message": format!("upstream body read failed: {e}")}
                    }),
                ));
            }
        };
        if let Some(doc_id) = usage_doc_id.clone() {
            let v: Value = serde_json::from_slice(&resp_bytes).unwrap_or(Value::Null);
            let input_tokens = v
                .pointer("/usage/input_tokens")
                .and_then(|x| x.as_u64())
                .filter(|value| *value > 0)
                .unwrap_or(estimated_input_tokens);
            let output_tokens = v
                .pointer("/usage/output_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let selected_model = v
                .get("model")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(state.opts.upstream_model.as_str())
                .to_string();
            if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
                record_prompt_cache_metrics(metrics, &request_id, &v);
                metrics.record(MetricsUpdate::FirstToken {
                    request_id: request_id.clone(),
                });
                metrics.record(MetricsUpdate::RequestCompleted {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(output_tokens),
                    selected_model: Some(selected_model.clone()),
                });
            }
            spawn_usage_callback(
                &state,
                UsageCallback {
                    doc_id,
                    input_tokens,
                    output_tokens,
                    duration_ms: t_start.elapsed().as_millis() as u64,
                    selected_model,
                    status: "success",
                    auth_headers,
                },
            );
        }
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap());
    }

    // Streaming path: pipe bytes through unchanged, observe usage off the SSE
    // events as they flow.
    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();

    let state_for_task = state.clone();
    let usage_doc_id_task = usage_doc_id.clone();
    let auth_headers_task = auth_headers.clone();
    let request_id_task = request_id.clone();

    tokio::spawn(async move {
        if let Err(e) = pump_stream(
            upstream,
            tx,
            t_start,
            usage_doc_id_task,
            request_id_task,
            estimated_input_tokens,
            auth_headers_task,
            state_for_task,
        )
        .await
        {
            warn!("stream pump error: {e}");
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body_out)
        .unwrap())
}

#[allow(clippy::too_many_arguments)]
async fn pump_stream(
    upstream: reqwest::Response,
    tx: mpsc::Sender<std::io::Result<Frame<Bytes>>>,
    t_start: Instant,
    usage_doc_id: Option<String>,
    request_id: String,
    estimated_input_tokens: u64,
    auth_headers: HashMap<String, String>,
    state: AppState,
) -> Result<()> {
    use futures::StreamExt;

    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    let mut input_tokens = estimated_input_tokens;
    let mut output_tokens: u64 = 0;
    let mut selected_model: Option<String> = None;
    let mut saw_first_token = false;
    // The turn is a success ONLY if it reached a terminal SSE event
    // (`message_stop` / `[DONE]`) and never emitted an `error` event. Every
    // other exit — a transport drop, a clean end before the terminal, an empty
    // stream, or an `event: error` payload — is a failed local turn. Token
    // counts can't tell these apart (a truncation can carry partial output),
    // so the terminal marker is the completion signal, not output_tokens.
    let mut saw_terminal = false;
    let mut saw_error = false;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                // Transport drop mid-stream: stop draining and fall through to
                // the callback below. `saw_terminal` stays false unless we
                // already passed the terminal event, so this closes as an error
                // instead of leaking — don't `?`-return past the callback.
                warn!("upstream stream error: {e}");
                break;
            }
        };
        // Forward the raw SSE bytes downstream first — the client only sees
        // what llama-server emits, byte-for-byte.
        if tx.send(Ok(Frame::data(chunk.clone()))).await.is_err() {
            // Client hung up; keep draining upstream so the model finishes
            // and we can still fire the usage callback.
        }
        // Best-effort usage extraction. SSE events are `\n\n`-separated; each
        // event has `event: <type>` and `data: <json>` lines.
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(idx) = buffer.find("\n\n") {
            let event_str = buffer[..idx].to_string();
            buffer.drain(..idx + 2);
            for line in event_str.lines() {
                if let Some(payload) = line.strip_prefix("data:") {
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    // OpenAI-style terminal marker (some llama-server builds
                    // emit it): a clean turn end, not a truncation.
                    if payload == "[DONE]" {
                        saw_terminal = true;
                        continue;
                    }
                    let v: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    match v.get("type").and_then(|x| x.as_str()) {
                        // Anthropic terminal event — the turn completed normally.
                        Some("message_stop") => saw_terminal = true,
                        // A 200 `text/event-stream` can still carry an
                        // application failure as an `event: error` payload and
                        // then close cleanly. That's a failed turn, not a
                        // success — mark it so the close reports an error.
                        Some("error") => saw_error = true,
                        _ => {}
                    }
                    if !saw_first_token && is_first_output_event(&v) {
                        saw_first_token = true;
                        if let Some(metrics) =
                            metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id)
                        {
                            metrics.record(MetricsUpdate::FirstToken {
                                request_id: request_id.clone(),
                            });
                        }
                    }
                    if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id)
                    {
                        record_prompt_cache_metrics(metrics, &request_id, &v);
                    }
                    // Anthropic SSE shapes:
                    //   message_start.message.usage.input_tokens / output_tokens
                    //   message_delta.usage.output_tokens
                    if let Some(in_tok) = v
                        .pointer("/message/usage/input_tokens")
                        .and_then(|x| x.as_u64())
                        .filter(|value| *value > 0)
                    {
                        input_tokens = in_tok;
                        if let Some(metrics) =
                            metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id)
                        {
                            metrics.record(MetricsUpdate::TokenUsage {
                                request_id: request_id.clone(),
                                input_tokens: Some(input_tokens),
                                output_tokens: Some(output_tokens),
                                selected_model: selected_model.clone(),
                            });
                        }
                    }
                    if let Some(out_tok) = v
                        .pointer("/message/usage/output_tokens")
                        .and_then(|x| x.as_u64())
                    {
                        output_tokens = out_tok;
                    }
                    if let Some(out_tok) =
                        v.pointer("/usage/output_tokens").and_then(|x| x.as_u64())
                    {
                        output_tokens = out_tok;
                        if let Some(metrics) =
                            metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id)
                        {
                            metrics.record(MetricsUpdate::TokenUsage {
                                request_id: request_id.clone(),
                                input_tokens: Some(input_tokens),
                                output_tokens: Some(output_tokens),
                                selected_model: selected_model.clone(),
                            });
                        }
                    }
                    if let Some(in_tok) = v
                        .pointer("/usage/input_tokens")
                        .and_then(|x| x.as_u64())
                        .filter(|value| *value > 0)
                    {
                        input_tokens = in_tok;
                        if let Some(metrics) =
                            metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id)
                        {
                            metrics.record(MetricsUpdate::TokenUsage {
                                request_id: request_id.clone(),
                                input_tokens: Some(input_tokens),
                                output_tokens: Some(output_tokens),
                                selected_model: selected_model.clone(),
                            });
                        }
                    }
                    if selected_model.is_none() {
                        if let Some(m) = v
                            .pointer("/message/model")
                            .and_then(|x| x.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            selected_model = Some(m.to_string());
                        }
                    }
                }
            }
        }
    }

    if let Some(doc_id) = usage_doc_id.as_ref() {
        // Success requires a clean terminal event and no error event. Every
        // other exit (transport drop, empty/aborted stream, `event: error`) is
        // a failed local turn that closes the placeholder as an error.
        let status = if saw_terminal && !saw_error {
            "success"
        } else {
            "error"
        };
        if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
            if status == "success" {
                metrics.record(MetricsUpdate::RequestCompleted {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(output_tokens),
                    selected_model: selected_model.clone(),
                });
            } else {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    error: "local stream ended without terminal success".to_owned(),
                });
            }
        }
        spawn_usage_callback(
            &state,
            UsageCallback {
                doc_id: doc_id.clone(),
                input_tokens,
                output_tokens,
                duration_ms: t_start.elapsed().as_millis() as u64,
                selected_model: selected_model.unwrap_or_else(|| state.opts.upstream_model.clone()),
                status,
                auth_headers,
            },
        );
    }
    Ok(())
}

/// Pure OpenAI Responses passthrough to the on-device model's `/v1/responses`.
///
/// The on-device llama-server (`--jinja`) serves the OpenAI Responses API
/// natively at `/v1/responses`, returning a well-formed reply with a
/// `usage:{input_tokens,output_tokens}` block and a `model` field. The hosted
/// router 307-redirects eligible Codex subagent turns here with a native
/// Responses body; we forward it UNCHANGED (except model substitution) and
/// return the reply UNCHANGED, while closing the placeholder usage row.
///
/// This is a parallel, simpler sibling of `handle_messages`: it reuses the exact
/// same auth-header resolution, metrics records, error-close discipline, and
/// usage callback, but deliberately skips all Anthropic-specific processing
/// (thinking mapping, `anthropic-version` header, tool-schema sanitize) — a
/// Responses body has a different shape. Tool-schema sanitize is NOT applied to
/// Responses tools; if the local grammar builder ever chokes on a Responses tool
/// schema, add a Responses-shaped sanitize here (the Anthropic one keys off
/// `tools[].input_schema`, which doesn't match Responses tools).
async fn handle_responses(state: AppState, req: Request<Incoming>) -> Result<Response<BoxBody>> {
    let t_start = Instant::now();
    let query = req.uri().query().unwrap_or("").to_string();
    let query_params = parse_query(&query);
    let usage_doc_id = query_params.get("usage_doc_id").cloned();
    let query_request_id = query_params.get("rayline_request_id").cloned();
    let request_id = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or(query_request_id)
        .or_else(|| usage_doc_id.clone())
        .unwrap_or_else(new_request_id);
    // Auth for the /v1/usage/update callback — identical resolution to
    // handle_messages: prefer the injector's pre-redirect stash keyed by
    // usage_doc_id, fall back to inbound headers only when the stash missed.
    let mut auth_headers = collect_auth_headers(req.headers());
    if let (Some(doc_id), Some(cache)) = (usage_doc_id.as_ref(), state.opts.auth_cache.as_ref()) {
        if let Ok(mut guard) = cache.lock() {
            if let Some(stashed) = guard.remove(doc_id) {
                auth_headers = stashed;
            }
        }
    }

    let body_bytes = match req.into_body().collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => {
            return Ok(json_response(
                StatusCode::BAD_REQUEST,
                json!({"error":{"type":"invalid_request_error","message":format!("read body: {e}")}}),
            ));
        }
    };

    // Peek at request shape for logging + stream detection. Parse failure is
    // non-fatal — we forward raw bytes.
    let mut parsed: Value = serde_json::from_slice(&body_bytes).unwrap_or(Value::Null);
    let want_stream = parsed
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let client_model = parsed
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let estimated_input_tokens = approximate_input_tokens(&parsed);

    let upstream_url = format!("{}/v1/responses", state.opts.target);
    info!(
        "/api/v1/responses → {} (client-model={} stream={})",
        upstream_url, client_model, want_stream
    );
    if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
        metrics.record(MetricsUpdate::RequestStarted {
            request_id: request_id.clone(),
            source: "adapter".to_owned(),
            requested_model: Some(client_model.clone()),
            agent_id: None,
            agent_type: None,
            session_id: None,
        });
        metrics.record(MetricsUpdate::RouteDecided {
            request_id: request_id.clone(),
            route_id: usage_doc_id.clone(),
            target: "local".to_owned(),
            endpoint_id: Some("local".to_owned()),
            selected_model: Some(state.opts.upstream_model.clone()),
            requested_model: Some(client_model.clone()),
            policy: Some("local-adapter".to_owned()),
            task_class: None,
            agent_id: None,
            agent_type: None,
            session_id: None,
        });
        metrics.record(MetricsUpdate::TokenUsage {
            request_id: request_id.clone(),
            input_tokens: Some(estimated_input_tokens),
            output_tokens: None,
            selected_model: Some(state.opts.upstream_model.clone()),
        });
    }

    // Model substitution — the field is still top-level `.model` in a Responses
    // body. Same rationale as handle_messages: custom endpoints reject a body
    // whose `model` isn't one they serve; the bundled llama-server ignores it. No
    // Anthropic thinking/tool-schema/llama-progress processing.
    let substituted_model = if let Some(obj) = parsed.as_object_mut() {
        if obj.get("model").and_then(Value::as_str) == Some(state.opts.upstream_model.as_str()) {
            false
        } else {
            obj.insert(
                "model".to_owned(),
                Value::String(state.opts.upstream_model.clone()),
            );
            true
        }
    } else {
        false
    };
    // Drop hosted tools a local model can't run (web_search). See
    // strip_local_unsupported_tools: llama-server skips it silently but LM Studio
    // misroutes the model's search intent onto another tool, so make the drop
    // explicit and backend-uniform here — the 307 lands eligible Codex subagent
    // turns on this same handler, so it covers both may-local and pure-local.
    let stripped_hosted = strip_local_unsupported_tools(&mut parsed);
    if stripped_hosted > 0 {
        info!(
            "stripped {stripped_hosted} local-unsupported hosted tool(s) (web_search) before local forward"
        );
    }
    let forward_body = if substituted_model || stripped_hosted > 0 {
        match serde_json::to_vec(&parsed) {
            Ok(b) => Bytes::from(b),
            Err(e) => {
                warn!("re-serialize before forward failed; forwarding raw: {e}");
                body_bytes.clone()
            }
        }
    } else {
        body_bytes.clone()
    };

    let upstream = match state
        .http
        .post(&upstream_url)
        .header("content-type", "application/json")
        .body(forward_body)
        .send()
        .await
    {
        Ok(u) => u,
        Err(e) => {
            // Local inference died before headers — close the placeholder as an
            // error instead of letting `?` leak the row.
            warn!("upstream request failed: {e}");
            if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: None,
                    error: format!("upstream request failed: {e}"),
                });
            }
            spawn_error_close(
                &state,
                &usage_doc_id,
                &auth_headers,
                t_start.elapsed().as_millis() as u64,
            );
            return Ok(json_response(
                StatusCode::BAD_GATEWAY,
                json!({
                    "error": {"type": "api_error", "message": format!("upstream request failed: {e}")}
                }),
            ));
        }
    };

    if !upstream.status().is_success() {
        let status = upstream.status();
        let text = upstream.text().await.unwrap_or_default();
        let snippet: String = text.chars().take(500).collect();
        warn!("upstream {}: {}", status, snippet);
        if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
            metrics.record(MetricsUpdate::RequestErrored {
                request_id: request_id.clone(),
                status_code: Some(status.as_u16()),
                error: format!("upstream {status}: {snippet}"),
            });
        }
        spawn_error_close(
            &state,
            &usage_doc_id,
            &auth_headers,
            t_start.elapsed().as_millis() as u64,
        );
        return Ok(json_response(
            StatusCode::BAD_GATEWAY,
            json!({
                "error": {"type": "api_error", "message": format!("upstream {}: {}", status, snippet)}
            }),
        ));
    }

    if !want_stream {
        let resp_bytes = match upstream.bytes().await {
            Ok(b) => b,
            Err(e) => {
                warn!("upstream body read failed: {e}");
                if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
                    metrics.record(MetricsUpdate::RequestErrored {
                        request_id: request_id.clone(),
                        status_code: None,
                        error: format!("upstream body read failed: {e}"),
                    });
                }
                spawn_error_close(
                    &state,
                    &usage_doc_id,
                    &auth_headers,
                    t_start.elapsed().as_millis() as u64,
                );
                return Ok(json_response(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": {"type": "api_error", "message": format!("upstream body read failed: {e}")}
                    }),
                ));
            }
        };
        if let Some(doc_id) = usage_doc_id.clone() {
            // llama-server's Responses body carries usage.input_tokens /
            // usage.output_tokens / model — same pointers as Anthropic here.
            let v: Value = serde_json::from_slice(&resp_bytes).unwrap_or(Value::Null);
            let input_tokens = v
                .pointer("/usage/input_tokens")
                .and_then(|x| x.as_u64())
                .filter(|value| *value > 0)
                .unwrap_or(estimated_input_tokens);
            let output_tokens = v
                .pointer("/usage/output_tokens")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            let selected_model = v
                .get("model")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(state.opts.upstream_model.as_str())
                .to_string();
            if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
                metrics.record(MetricsUpdate::FirstToken {
                    request_id: request_id.clone(),
                });
                metrics.record(MetricsUpdate::RequestCompleted {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(output_tokens),
                    selected_model: Some(selected_model.clone()),
                });
            }
            spawn_usage_callback(
                &state,
                UsageCallback {
                    doc_id,
                    input_tokens,
                    output_tokens,
                    duration_ms: t_start.elapsed().as_millis() as u64,
                    selected_model,
                    status: "success",
                    auth_headers,
                },
            );
        }
        return Ok(Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(full_body(resp_bytes))
            .unwrap());
    }

    // Streaming path: pass the Responses SSE through byte-for-byte and fire the
    // usage callback at end-of-stream. See pump_responses_stream for how the
    // terminal event / usage extraction differs from the Anthropic pump.
    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    let stream_body = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx));
    let body_out: BoxBody = stream_body.boxed();

    let state_for_task = state.clone();
    let usage_doc_id_task = usage_doc_id.clone();
    let auth_headers_task = auth_headers.clone();
    let request_id_task = request_id.clone();

    tokio::spawn(async move {
        if let Err(e) = pump_responses_stream(
            upstream,
            tx,
            t_start,
            usage_doc_id_task,
            request_id_task,
            estimated_input_tokens,
            auth_headers_task,
            state_for_task,
        )
        .await
        {
            warn!("stream pump error: {e}");
        }
    });

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(body_out)
        .unwrap())
}

/// Passthrough pump for an OpenAI Responses SSE stream. Mirrors `pump_stream`'s
/// error-close discipline — bytes are forwarded downstream unchanged and usage
/// is snooped off the events as they flow — but keys off the Responses SSE shape
/// rather than Anthropic's. The terminal marker is an event whose JSON `.type`
/// is `response.completed` (or an OpenAI-style `[DONE]` data line), with usage
/// on `.response.usage.{input_tokens,output_tokens}` and model on
/// `.response.model`; `.type == "error"` / `response.failed` marks a failed turn.
///
/// SIMPLIFIED vs. `pump_stream`: we don't record per-delta TokenUsage/PromptCache
/// (Responses SSE has no llama prompt-progress frames and only carries usage on
/// the terminal event); FirstToken fires on the first `response.output_*` delta.
/// Any non-terminal or error exit closes the placeholder as an error.
#[allow(clippy::too_many_arguments)]
async fn pump_responses_stream(
    upstream: reqwest::Response,
    tx: mpsc::Sender<std::io::Result<Frame<Bytes>>>,
    t_start: Instant,
    usage_doc_id: Option<String>,
    request_id: String,
    estimated_input_tokens: u64,
    auth_headers: HashMap<String, String>,
    state: AppState,
) -> Result<()> {
    use futures::StreamExt;

    let mut stream = upstream.bytes_stream();
    let mut buffer = String::new();
    let mut input_tokens = estimated_input_tokens;
    let mut output_tokens: u64 = 0;
    let mut selected_model: Option<String> = None;
    let mut saw_first_token = false;
    let mut saw_terminal = false;
    let mut saw_error = false;

    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                // Transport drop mid-stream: stop draining and fall through to
                // the callback. saw_terminal stays false unless we already
                // passed the terminal event, so this closes as an error.
                warn!("upstream stream error: {e}");
                break;
            }
        };
        // Forward raw SSE bytes downstream first — the client sees exactly what
        // llama-server emits.
        if tx.send(Ok(Frame::data(chunk.clone()))).await.is_err() {
            // Client hung up; keep draining so the model finishes and we can
            // still fire the usage callback.
        }
        // Normalize CRLF → LF before scanning: a standards-compliant SSE upstream
        // may separate frames with `\r\n\r\n`, which `find("\n\n")` would never
        // match — leaving `response.completed`/usage unseen and a successful turn
        // wrongly closed as an error. Normalize the whole BUFFER after appending
        // (not the chunk in isolation) so a `\r\n` split across a TCP chunk boundary
        // is still collapsed. The RAW bytes were already forwarded to the client
        // above, so this only affects our parse buffer.
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        if buffer.contains('\r') {
            buffer = buffer.replace("\r\n", "\n");
        }
        while let Some(idx) = buffer.find("\n\n") {
            let event_str = buffer[..idx].to_string();
            buffer.drain(..idx + 2);
            for line in event_str.lines() {
                if let Some(payload) = line.strip_prefix("data:") {
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    if payload == "[DONE]" {
                        saw_terminal = true;
                        continue;
                    }
                    let v: Value = match serde_json::from_str(payload) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    match v.get("type").and_then(|x| x.as_str()) {
                        // Responses terminal event — the turn completed normally.
                        Some("response.completed") => saw_terminal = true,
                        // A 200 text/event-stream can still carry an application
                        // failure and then close cleanly. Treat it as a failed turn.
                        Some("response.failed") | Some("error") => saw_error = true,
                        _ => {}
                    }
                    if !saw_first_token && is_first_responses_output_event(&v) {
                        saw_first_token = true;
                        if let Some(metrics) =
                            metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id)
                        {
                            metrics.record(MetricsUpdate::FirstToken {
                                request_id: request_id.clone(),
                            });
                        }
                    }
                    // Responses usage lives on the terminal event under
                    // `response.usage`; some builds also emit a top-level `usage`.
                    if let Some(in_tok) = v
                        .pointer("/response/usage/input_tokens")
                        .or_else(|| v.pointer("/usage/input_tokens"))
                        .and_then(|x| x.as_u64())
                        .filter(|value| *value > 0)
                    {
                        input_tokens = in_tok;
                    }
                    if let Some(out_tok) = v
                        .pointer("/response/usage/output_tokens")
                        .or_else(|| v.pointer("/usage/output_tokens"))
                        .and_then(|x| x.as_u64())
                    {
                        output_tokens = out_tok;
                    }
                    if selected_model.is_none() {
                        if let Some(m) = v
                            .pointer("/response/model")
                            .or_else(|| v.get("model"))
                            .and_then(|x| x.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            selected_model = Some(m.to_string());
                        }
                    }
                }
            }
        }
    }

    if let Some(doc_id) = usage_doc_id.as_ref() {
        let status = if saw_terminal && !saw_error {
            "success"
        } else {
            "error"
        };
        if let Some(metrics) = metrics_for_usage_doc(&state.opts.metrics, &usage_doc_id) {
            if status == "success" {
                metrics.record(MetricsUpdate::RequestCompleted {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    input_tokens: Some(input_tokens),
                    output_tokens: Some(output_tokens),
                    selected_model: selected_model.clone(),
                });
            } else {
                metrics.record(MetricsUpdate::RequestErrored {
                    request_id: request_id.clone(),
                    status_code: Some(StatusCode::OK.as_u16()),
                    error: "local stream ended without terminal success".to_owned(),
                });
            }
        }
        spawn_usage_callback(
            &state,
            UsageCallback {
                doc_id: doc_id.clone(),
                input_tokens,
                output_tokens,
                duration_ms: t_start.elapsed().as_millis() as u64,
                selected_model: selected_model.unwrap_or_else(|| state.opts.upstream_model.clone()),
                status,
                auth_headers,
            },
        );
    }
    Ok(())
}

/// First "real output" event for a Responses SSE stream — used to fire the
/// FirstToken metric. Responses emits `response.output_text.delta` /
/// `response.output_item.added` (and similar `response.output_*` events) as the
/// model produces tokens; the lifecycle events (`response.created`,
/// `response.in_progress`, `response.completed`, `response.failed`) are not
/// output.
fn is_first_responses_output_event(value: &Value) -> bool {
    match value.get("type").and_then(Value::as_str) {
        Some(
            "response.created"
            | "response.in_progress"
            | "response.completed"
            | "response.failed"
            | "error",
        ) => false,
        Some(t) => t.starts_with("response.output"),
        None => false,
    }
}

fn enable_llama_progress(value: &mut Value) -> bool {
    let Some(obj) = value.as_object_mut() else {
        return false;
    };
    if obj.get("return_progress").and_then(Value::as_bool) == Some(true) {
        return false;
    }
    obj.insert("return_progress".to_owned(), Value::Bool(true));
    true
}

fn is_first_output_event(value: &Value) -> bool {
    if value.get("prompt_progress").is_some() {
        return false;
    }
    match value.get("type").and_then(Value::as_str) {
        Some("message_start") | Some("content_block_start") | Some("content_block_delta") => true,
        Some("message_delta") | Some("message_stop") | Some("error") => false,
        _ => {
            value
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .is_some()
                || value
                    .pointer("/choices/0/delta/tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|value| !value.is_empty())
                || value
                    .get("content")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .is_some()
        }
    }
}

fn record_prompt_cache_metrics(metrics: &SharedMetricsSink, request_id: &str, value: &Value) {
    if let Some(progress) = value.get("prompt_progress") {
        let total = progress.get("total").and_then(Value::as_u64);
        let cache = progress.get("cache").and_then(Value::as_u64);
        let processed = progress.get("processed").and_then(Value::as_u64);
        let prompt_ms = progress.get("time_ms").and_then(Value::as_f64);
        if total.is_some() || cache.is_some() || processed.is_some() {
            metrics.record(MetricsUpdate::PromptCache {
                request_id: request_id.to_owned(),
                prompt_tokens: total,
                cache_tokens: cache,
                processed_tokens: processed,
                prompt_ms,
                prompt_tps: None,
            });
        }
    }

    if let Some(timings) = value.get("timings") {
        let cache = timings.get("cache_n").and_then(Value::as_u64);
        let prompt = timings.get("prompt_n").and_then(Value::as_u64);
        let prompt_ms = timings.get("prompt_ms").and_then(Value::as_f64);
        let prompt_tps = timings.get("prompt_per_second").and_then(Value::as_f64);
        if cache.is_some() || prompt.is_some() || prompt_ms.is_some() || prompt_tps.is_some() {
            metrics.record(MetricsUpdate::PromptCache {
                request_id: request_id.to_owned(),
                prompt_tokens: cache.zip(prompt).map(|(cache, prompt)| cache + prompt),
                cache_tokens: cache,
                processed_tokens: cache.zip(prompt).map(|(cache, prompt)| cache + prompt),
                prompt_ms,
                prompt_tps,
            });
        }
    }

    if let Some(cached) = value
        .pointer("/usage/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
    {
        let prompt = value
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64);
        metrics.record(MetricsUpdate::PromptCache {
            request_id: request_id.to_owned(),
            prompt_tokens: prompt,
            cache_tokens: Some(cached),
            processed_tokens: prompt,
            prompt_ms: None,
            prompt_tps: None,
        });
    }
}

fn metrics_for_usage_doc<'a>(
    metrics: &'a Option<SharedMetricsSink>,
    usage_doc_id: &Option<String>,
) -> Option<&'a SharedMetricsSink> {
    usage_doc_id.as_ref()?;
    metrics.as_ref()
}

#[derive(Debug, Clone)]
struct UsageCallback {
    doc_id: String,
    input_tokens: u64,
    output_tokens: u64,
    duration_ms: u64,
    selected_model: String,
    /// Terminal state for the placeholder row: "success" when local inference
    /// completed, "error" when it failed (upstream non-2xx or an aborted
    /// stream). Without this, a failed turn leaks forever as a 0-token
    /// `local_redirect` placeholder because the cloud row never gets closed.
    status: &'static str,
    auth_headers: HashMap<String, String>,
}

/// Fire a `status: "error"` close for a placeholder whose local turn failed
/// before producing usable output: an upstream transport error (server down,
/// process OOM/crash), a non-2xx response, or an unreadable body. No-op when
/// there's no `usage_doc_id` (a direct call that bypassed the router). Takes
/// references and clones internally so the caller's `usage_doc_id` /
/// `auth_headers` stay usable on the success path.
fn spawn_error_close(
    state: &AppState,
    usage_doc_id: &Option<String>,
    auth_headers: &HashMap<String, String>,
    duration_ms: u64,
) {
    if let Some(doc_id) = usage_doc_id.clone() {
        spawn_usage_callback(
            state,
            UsageCallback {
                doc_id,
                input_tokens: 0,
                output_tokens: 0,
                duration_ms,
                selected_model: state.opts.upstream_model.clone(),
                status: "error",
                auth_headers: auth_headers.clone(),
            },
        );
    }
}

fn spawn_usage_callback(state: &AppState, cb: UsageCallback) {
    let http = state.http.clone();
    let router_url = state.opts.router_url.clone();
    tokio::spawn(async move {
        let url = format!("{router_url}/v1/usage/update");
        let mut req = http
            .post(&url)
            .header("content-type", "application/json")
            .json(&json!({
                "routeId": cb.doc_id,
                "inputTokens": cb.input_tokens,
                "outputTokens": cb.output_tokens,
                "durationMs": cb.duration_ms,
                "selectedModel": cb.selected_model,
                "status": cb.status,
            }));
        for (k, v) in &cb.auth_headers {
            req = req.header(k, v);
        }
        match req.send().await {
            Ok(resp) if resp.status().is_success() => {
                info!(
                    "usage/update ok docId={} in={} out={}",
                    cb.doc_id, cb.input_tokens, cb.output_tokens
                );
            }
            Ok(resp) => {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                let snippet: String = text.chars().take(300).collect();
                error!("usage/update failed: {} {}", status, snippet);
            }
            Err(e) => error!("usage/update threw: {e}"),
        }
    });
}

/// Structural JSON-Schema keywords that define a node's *shape*. A node
/// carrying none of these (after stripping the constraint keywords below) is a
/// metadata-only "any" node (e.g. `{"description": ...}`) that llama-server's
/// `--jinja` grammar builder rejects with `Unrecognized schema`, 400ing the
/// whole turn. We coerce such nodes to `{}` (= "any"), which it accepts.
const RECOGNIZED_SCHEMA_KEYS: &[&str] = &[
    "type",
    "enum",
    "const",
    "$ref",
    "oneOf",
    "anyOf",
    "allOf",
    "not",
    "if",
    "properties",
    "additionalProperties",
    "patternProperties",
    "required",
    "items",
    "prefixItems",
    "contains",
];

/// Value-constraint keywords that llama-server's grammar builder turns into
/// regex/repetition grammars. `pattern`/`format` flow through
/// `common_schema_converter::_visit_pattern`, which SIGSEGVs on some real
/// inputs (observed crashing llama-server on Claude Code's tool set); the
/// numeric/length/size bounds are likewise non-structural. We strip all of
/// them before forwarding — they constrain values, not the shape the model
/// needs, so tool calls still work and the grammar builder can't choke on them.
const STRIPPED_SCHEMA_KEYS: &[&str] = &[
    "pattern",
    "format",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "minLength",
    "maxLength",
    "minItems",
    "maxItems",
    "uniqueItems",
    "minProperties",
    "maxProperties",
];

/// Whether to translate the Anthropic `thinking` request field into the local
/// model's `chat_template_kwargs.enable_thinking`. On by default; set
/// `RAYLINE_MAP_THINKING=0` (or `false`/`off`) to forward `thinking` untouched.
fn thinking_mapping_enabled() -> bool {
    !matches!(
        std::env::var("RAYLINE_MAP_THINKING").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Map Claude Code's Anthropic-native `thinking` field onto the local model's
/// `chat_template_kwargs.enable_thinking`, which is what Qwen-style chat
/// templates (and oMLX / llama-server `--jinja`) actually key off.
///
/// Claude Code sends `thinking: {"type": "adaptive" | "enabled" | "disabled"}`
/// per turn (for sonnet/opus, including subagents); an absent field means
/// thinking *off* under Anthropic semantics. Local templates instead default to
/// thinking *on* and don't understand `adaptive`, so without this a subagent
/// turn either over-thinks or ignores the client's intent. We therefore set
/// `chat_template_kwargs.enable_thinking` = (thinking present && type !=
/// "disabled") unless the caller pinned it (then we respect it). The original
/// Anthropic `thinking` object is preserved for stricter Anthropic-compatible
/// local endpoints. Returns true if it modified the body.
fn map_thinking_to_template_kwargs(body: &mut Value) -> bool {
    let Some(obj) = body.as_object_mut() else {
        return false;
    };
    // Intent: absent or `{"type":"disabled"}` => off; anything else present => on.
    let enable = matches!(
        obj.get("thinking"),
        Some(Value::Object(t)) if t.get("type").and_then(Value::as_str) != Some("disabled")
    );

    let mut changed = false;

    // Set chat_template_kwargs.enable_thinking unless the caller already pinned it.
    let ctk = obj
        .entry("chat_template_kwargs".to_owned())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if let Some(map) = ctk.as_object_mut() {
        if !map.contains_key("enable_thinking") {
            map.insert("enable_thinking".to_owned(), Value::Bool(enable));
            changed = true;
        }
    }
    changed
}

/// Remove hosted tools no local backend can execute from the top-level `tools`
/// array, returning the number dropped. `web_search` is a server-side hosted tool
/// the *cloud* provider fulfils; a local model has no way to run it. Left in the
/// body it degrades silently and inconsistently per backend — llama-server logs
/// `unsupported Responses tool type 'web_search' skipped` and drops it cleanly,
/// but LM Studio misroutes the model's search intent onto whatever other tool is
/// present (emitting e.g. a `function_call` to an unrelated tool with
/// `{"queries":[…]}` args). Stripping it here — the single choke point before any
/// local server, on both the Responses and Anthropic paths — makes the drop
/// explicit and identical across backends. Matches both the Responses shape
/// (`{"type":"web_search"}`) and the Anthropic server-tool shape
/// (`{"type":"web_search_20250305",…}`) by `type` prefix, so it is model- and
/// backend-agnostic.
fn strip_local_unsupported_tools(body: &mut Value) -> usize {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return 0;
    };
    let before = tools.len();
    tools.retain(|tool| {
        !tool
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("web_search"))
    });
    before - tools.len()
}

/// Coerce every tool `input_schema` node that llama-server can't grammar-convert
/// into an empty schema `{}`. Returns the number of nodes coerced.
fn sanitize_tool_schemas(body: &mut Value) -> usize {
    let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) else {
        return 0;
    };
    let mut coerced = 0;
    for tool in tools.iter_mut() {
        if let Some(schema) = tool.get_mut("input_schema") {
            coerced += sanitize_schema_node(schema);
        }
    }
    coerced
}

/// Recursively sanitize one JSON-Schema node in place: descend into every
/// subschema position, then — if THIS node is an object carrying no recognized
/// schema keyword — replace it with `{}`. Returns the count coerced.
fn sanitize_schema_node(node: &mut Value) -> usize {
    let Value::Object(map) = node else {
        return 0;
    };
    let mut coerced = 0;

    // Maps of subschemas.
    for key in ["properties", "patternProperties", "$defs", "definitions"] {
        if let Some(Value::Object(sub)) = map.get_mut(key) {
            for v in sub.values_mut() {
                coerced += sanitize_schema_node(v);
            }
        }
    }
    // Single subschemas (`items` may also be a bool/array; handled elsewhere).
    for key in [
        "additionalProperties",
        "items",
        "additionalItems",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
    ] {
        if let Some(v) = map.get_mut(key) {
            if v.is_object() {
                coerced += sanitize_schema_node(v);
            }
        }
    }
    // Arrays of subschemas (incl. tuple-style `items`).
    for key in ["oneOf", "anyOf", "allOf", "prefixItems", "items"] {
        if let Some(Value::Array(arr)) = map.get_mut(key) {
            for v in arr.iter_mut() {
                coerced += sanitize_schema_node(v);
            }
        }
    }

    // Strip value-constraint keywords the grammar builder mishandles/crashes on.
    for key in STRIPPED_SCHEMA_KEYS {
        if map.remove(*key).is_some() {
            coerced += 1;
        }
    }
    // Coerce a node with no structural keyword left to `{}` (= "any").
    let recognized = map
        .keys()
        .any(|k| RECOGNIZED_SCHEMA_KEYS.contains(&k.as_str()));
    if !recognized {
        map.clear();
        coerced += 1;
    }
    coerced
}

fn collect_auth_headers(headers: &hyper::HeaderMap) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        out.insert("Authorization".to_string(), v.to_string());
    }
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        out.insert("x-api-key".to_string(), v.to_string());
    }
    out
}

fn parse_query(q: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in q.split('&').filter(|s| !s.is_empty()) {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("").to_string();
        let v = it.next().unwrap_or("").to_string();
        if !k.is_empty() {
            out.insert(k, urldecode(&v));
        }
    }
    out
}

fn approximate_input_tokens(value: &Value) -> u64 {
    let system = value.get("system").map(content_to_text).unwrap_or_default();
    let messages = value
        .get("messages")
        .map(content_to_text)
        .unwrap_or_default();
    ((system.len() + messages.len()) as u64 / 4).max(1)
}

fn content_to_text(value: &Value) -> String {
    if let Some(text) = value.as_str() {
        return text.to_owned();
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    if let Some(obj) = value.as_object() {
        return obj
            .values()
            .map(content_to_text)
            .collect::<Vec<_>>()
            .join("\n");
    }
    String::new()
}

fn urldecode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let decoded = if bytes[i] == b'%' && i + 2 < bytes.len() {
            u8::from_str_radix(
                std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("00"),
                16,
            )
            .ok()
        } else {
            None
        };
        if let Some(b) = decoded {
            out.push(b);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn chrono_like_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("epoch:{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_disabled_maps_to_enable_false() {
        let mut v = json!({"thinking":{"type":"disabled"},"messages":[]});
        assert!(map_thinking_to_template_kwargs(&mut v));
        assert_eq!(v["chat_template_kwargs"]["enable_thinking"], json!(false));
    }

    #[test]
    fn thinking_adaptive_enables_and_preserves_type() {
        let mut v = json!({"thinking":{"type":"adaptive","budget_tokens":2048}});
        assert!(map_thinking_to_template_kwargs(&mut v));
        assert_eq!(v["chat_template_kwargs"]["enable_thinking"], json!(true));
        // `thinking` is forwarded unchanged for strict Anthropic-compatible endpoints.
        assert_eq!(v["thinking"]["type"], json!("adaptive"));
        assert_eq!(v["thinking"]["budget_tokens"], json!(2048));
    }

    #[test]
    fn thinking_enabled_left_intact_and_enables() {
        let mut v = json!({"thinking":{"type":"enabled"}});
        assert!(map_thinking_to_template_kwargs(&mut v));
        assert_eq!(v["chat_template_kwargs"]["enable_thinking"], json!(true));
        assert_eq!(v["thinking"]["type"], json!("enabled"));
    }

    #[test]
    fn absent_thinking_maps_to_enable_false() {
        // Anthropic semantics: no `thinking` field == thinking off.
        let mut v = json!({"messages":[]});
        assert!(map_thinking_to_template_kwargs(&mut v));
        assert_eq!(v["chat_template_kwargs"]["enable_thinking"], json!(false));
    }

    #[test]
    fn caller_supplied_enable_thinking_is_respected() {
        let mut v = json!({
            "thinking":{"type":"adaptive"},
            "chat_template_kwargs":{"enable_thinking":false,"foo":"bar"}
        });
        assert!(!map_thinking_to_template_kwargs(&mut v));
        // Explicit chat_template_kwargs.enable_thinking wins; other keys kept.
        assert_eq!(v["chat_template_kwargs"]["enable_thinking"], json!(false));
        assert_eq!(v["chat_template_kwargs"]["foo"], json!("bar"));
        assert_eq!(v["thinking"]["type"], json!("adaptive"));
    }

    #[test]
    fn parse_query_basic() {
        let q = parse_query("usage_doc_id=abc&foo=bar");
        assert_eq!(q.get("usage_doc_id"), Some(&"abc".to_string()));
        assert_eq!(q.get("foo"), Some(&"bar".to_string()));
    }

    #[test]
    fn approximate_input_tokens_counts_messages_and_system() {
        let value = json!({
            "system": "system prompt",
            "messages": [
                {"role": "user", "content": "hello there"},
                {"role": "assistant", "content": [{"type": "text", "text": "response"}]}
            ]
        });
        assert!(approximate_input_tokens(&value) > 1);
    }

    #[test]
    fn collect_auth_headers_filters() {
        let mut h = hyper::HeaderMap::new();
        h.insert("authorization", "Bearer x".parse().unwrap());
        h.insert("x-api-key", "k".parse().unwrap());
        h.insert("x-stainless-os", "macos".parse().unwrap());
        let out = collect_auth_headers(&h);
        assert_eq!(out.get("Authorization"), Some(&"Bearer x".to_string()));
        assert_eq!(out.get("x-api-key"), Some(&"k".to_string()));
        assert!(!out.contains_key("x-stainless-os"));
    }

    #[test]
    fn urldecode_basic() {
        assert_eq!(urldecode("a%20b+c"), "a b c");
        assert_eq!(urldecode("hello"), "hello");
    }

    #[test]
    fn sanitize_coerces_typeless_property_to_empty_schema() {
        // The exact shape that 400s llama-server: a `Workflow.args` param with
        // a description but no `type`.
        let mut v = json!({
            "tools": [{
                "name": "Workflow",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "args": {"description": "Optional input value exposed to the script as the global `args`."},
                        "script": {"type": "string", "description": "the script"}
                    }
                }
            }]
        });
        assert_eq!(sanitize_tool_schemas(&mut v), 1);
        let props = &v["tools"][0]["input_schema"]["properties"];
        assert_eq!(props["args"], json!({}), "typeless node coerced to {{}}");
        // Typed sibling is untouched.
        assert_eq!(props["script"]["type"], "string");
        assert_eq!(props["script"]["description"], "the script");
    }

    #[test]
    fn sanitize_leaves_well_typed_schema_untouched() {
        let mut v = json!({
            "tools": [{"input_schema": {"type": "object", "properties": {"x": {"type": "number"}}}}]
        });
        assert_eq!(sanitize_tool_schemas(&mut v), 0);
    }

    #[test]
    fn sanitize_recurses_into_nested_objects_and_arrays() {
        let mut v = json!({
            "tools": [{"input_schema": {
                "type": "object",
                "properties": {
                    "nested": {"type": "object", "properties": {"deep": {"description": "any"}}},
                    "list": {"type": "array", "items": {"description": "any"}}
                }
            }}]
        });
        assert_eq!(sanitize_tool_schemas(&mut v), 2);
        let props = &v["tools"][0]["input_schema"]["properties"];
        assert_eq!(props["nested"]["properties"]["deep"], json!({}));
        assert_eq!(props["list"]["items"], json!({}));
    }

    #[test]
    fn strip_removes_web_search_hosted_tool_keeps_the_rest() {
        // Responses shape: the exact tool set a real Codex explorer subagent
        // ships. web_search is dropped; exec_command/function tools survive.
        let mut v = json!({
            "tools": [
                {"type": "web_search"},
                {"type": "function", "name": "exec_command"},
                {"type": "function", "name": "noop"}
            ]
        });
        assert_eq!(strip_local_unsupported_tools(&mut v), 1);
        let names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        assert_eq!(names, ["exec_command", "noop"]);
        assert!(
            !v["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|t| t["type"] == "web_search")
        );
    }

    #[test]
    fn strip_matches_anthropic_server_tool_shape_by_prefix() {
        // Anthropic server-tool form carries a versioned type; match by prefix.
        let mut v = json!({
            "tools": [
                {"type": "web_search_20250305", "name": "web_search"},
                {"name": "Read", "input_schema": {"type": "object"}}
            ]
        });
        assert_eq!(strip_local_unsupported_tools(&mut v), 1);
        assert_eq!(v["tools"].as_array().unwrap().len(), 1);
        assert_eq!(v["tools"][0]["name"], "Read");
    }

    #[test]
    fn strip_is_noop_without_tools_or_web_search() {
        let mut none = json!({"input": []});
        assert_eq!(strip_local_unsupported_tools(&mut none), 0);
        let mut only_fns = json!({"tools": [{"type": "function", "name": "exec_command"}]});
        assert_eq!(strip_local_unsupported_tools(&mut only_fns), 0);
        assert_eq!(only_fns["tools"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn sanitize_is_noop_without_tools() {
        let mut v = json!({"model": "x", "messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(sanitize_tool_schemas(&mut v), 0);
    }

    #[test]
    fn sanitize_strips_value_constraints_but_keeps_structure() {
        // `pattern`/`format` crash llama-server's grammar builder; numeric/length
        // bounds are non-structural. Strip them; keep type/required/enum/etc.
        let mut v = json!({
            "tools": [{"input_schema": {
                "type": "object",
                "properties": {
                    "q": {"type": "string", "pattern": "^x$", "format": "uri",
                          "minLength": 3, "description": "d"},
                    "n": {"type": "integer", "minimum": 0, "maximum": 9}
                },
                "required": ["q"]
            }}]
        });
        // 3 strips on `q` (pattern, format, minLength) + 2 on `n` (minimum, maximum).
        assert_eq!(sanitize_tool_schemas(&mut v), 5);
        let schema = &v["tools"][0]["input_schema"];
        assert_eq!(
            schema["properties"]["q"],
            json!({"type": "string", "description": "d"})
        );
        assert_eq!(schema["properties"]["n"], json!({"type": "integer"}));
        // Structural keywords preserved.
        assert_eq!(schema["required"], json!(["q"]));
    }
}
