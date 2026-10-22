//! Inbound OpenAI Chat Completions (`POST /v1/chat/completions`).
//!
//! OpenAI-compatible-only harnesses (Roomote and many others) discover models
//! with `GET /v1/models` and then speak Chat Completions. The router routes the
//! request like every other listener and:
//!
//! - passes it through to an `openai_chat` endpoint, with the routed model and
//!   the endpoint's auth (the caller's own auth for `client_bearer`);
//! - refuses a route to an `openai_responses` endpoint, which has no Chat
//!   Completions surface;
//! - translates it to Anthropic Messages for an `anthropic_messages` endpoint
//!   or the on-device adapter, and translates the answer (JSON or SSE) back to
//!   Chat Completions, including tool calls.

use super::*;

/// `POST /v1/chat/completions`.
pub(crate) async fn handle_chat_completions(
    state: AppState,
    req: Request<Incoming>,
) -> Result<Response<BoxBody>> {
    let t_start = Instant::now();
    let headers = req.headers().clone();
    let body = req.into_body().collect().await?.to_bytes();
    let parsed = serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null);
    if !parsed.is_object() {
        return Ok(json_response(
            StatusCode::BAD_REQUEST,
            json!({"error": {"type": "invalid_request_error",
                             "message": "the request body must be a JSON object"}}),
        ));
    }
    let decision = select_route_with_warn(&state, &headers, &parsed, ApiSurface::Anthropic);
    let request_id = request_id_from_headers(&headers);
    info!(
        "chat route {} requested={} selected={} policy={} task={} elapsed_ms={}",
        route_target_label(&decision.target),
        decision.requested_model,
        decision.selected_model,
        decision.policy,
        decision.task_class,
        t_start.elapsed().as_millis()
    );
    let want_stream = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let include_usage = parsed
        .pointer("/stream_options/include_usage")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let response = match &decision.target {
        RouteSelection::Local => {
            if let Some(refusal) = untranslatable_chat_tools_response(&parsed) {
                return Ok(refusal);
            }
            let (anthropic, tool_names) =
                chat_to_anthropic_request(&parsed, &decision.selected_model);
            let url = format!(
                "http://127.0.0.1:{}/api/v1/messages?usage_doc_id={}&rayline_request_id={}",
                state.opts.local_adapter_port,
                query_escape(&decision.route_id),
                query_escape(&request_id)
            );
            let resp = state
                .http
                .post(url)
                .header("content-type", "application/json")
                .json(&anthropic)
                .send()
                .await?;
            anthropic_response_to_chat(
                resp,
                &decision,
                &request_id,
                want_stream,
                include_usage,
                tool_names,
            )
            .await
        }
        RouteSelection::Endpoint(endpoint_id) => {
            let endpoint = state
                .config
                .endpoints
                .iter()
                .find(|endpoint| endpoint.id == *endpoint_id)
                .ok_or_else(|| anyhow!("endpoint {endpoint_id:?} not found"))?;
            match endpoint.protocol {
                EndpointProtocol::AnthropicMessages => {
                    if let Some(refusal) = untranslatable_chat_tools_response(&parsed) {
                        return Ok(refusal);
                    }
                    let (anthropic, tool_names) =
                        chat_to_anthropic_request(&parsed, &decision.selected_model);
                    let url = format!("{}/v1/messages", endpoint.base_url.trim_end_matches('/'));
                    let mut outbound = state
                        .http
                        .post(url)
                        .header("content-type", "application/json")
                        .header("anthropic-version", "2023-06-01")
                        .json(&anthropic);
                    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Anthropic)?;
                    let resp = outbound.send().await?;
                    anthropic_response_to_chat(
                        resp,
                        &decision,
                        &request_id,
                        want_stream,
                        include_usage,
                        tool_names,
                    )
                    .await
                }
                EndpointProtocol::OpenAIResponses => {
                    // A Responses-only backend has no Chat Completions surface,
                    // and this listener does not translate Chat to Responses.
                    return Ok(chat_invalid_request(
                        "unsupported_route",
                        "model",
                        format!(
                            "the route for model {:?} targets endpoint {endpoint_id:?}, which \
                             speaks openai_responses; Chat Completions requests can only be \
                             routed to openai_chat or anthropic_messages endpoints",
                            decision.requested_model
                        ),
                    ));
                }
                EndpointProtocol::OpenAIChat => {
                    let mut body = parsed.clone();
                    body["model"] = json!(decision.selected_model);
                    let url = format!(
                        "{}/chat/completions",
                        endpoint.base_url.trim_end_matches('/')
                    );
                    let client_bearer = endpoint.auth == Some(AuthMode::ClientBearer);
                    let http = if client_bearer {
                        &state.http_ipv4
                    } else {
                        &state.http
                    };
                    let mut outbound = http.post(url);
                    // Same header policy as the Responses passthrough: a
                    // `client_bearer` endpoint authenticates with the caller's
                    // own Authorization (and account headers).
                    copy_openai_passthrough_headers(&mut outbound, &headers, true, client_bearer);
                    outbound = outbound.json(&body);
                    outbound = apply_endpoint_headers(outbound, endpoint, AuthStyle::Bearer)?;
                    let resp = outbound.send().await?;
                    passthrough_response(resp, &decision).await
                }
            }
        }
    };
    if let Err(error) = response.as_ref() {
        record_request_error(state.opts.metrics.as_ref(), &request_id, None, error);
    }
    response
}

/// An OpenAI-protocol upstream's answer, unchanged (streamed as it arrives).
async fn passthrough_response(
    resp: reqwest::Response,
    decision: &RouteDecision,
) -> Result<Response<BoxBody>> {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
    tokio::spawn(async move {
        use futures::StreamExt;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let frame = chunk.map(Frame::data).map_err(std::io::Error::other);
            if tx.send(frame).await.is_err() {
                break;
            }
        }
    });
    let body: BoxBody = StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed();
    let mut response = Response::builder()
        .status(status)
        .header("content-type", content_type)
        .body(body)?;
    add_decision_headers(response.headers_mut(), decision);
    Ok(response)
}

/// OpenAI-shaped 400 for a request the translated route cannot carry.
fn chat_invalid_request(code: &str, param: &str, message: String) -> Response<BoxBody> {
    json_response(
        StatusCode::BAD_REQUEST,
        json!({"error": {"type": "invalid_request_error", "code": code,
                         "param": param, "message": message}}),
    )
}

/// The declared tool types Anthropic Messages has no equivalent for (Chat
/// `custom` freeform tools and anything else that is not a `function`), in
/// declaration order. Translating without them would silently drop them.
pub(crate) fn untranslatable_chat_tool_types(body: &Value) -> Vec<String> {
    let mut kinds: Vec<String> = Vec::new();
    for tool in body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let kind = tool
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if kind != "function" && !kinds.iter().any(|seen| seen == kind) {
            kinds.push(kind.to_owned());
        }
    }
    kinds
}

fn untranslatable_chat_tools_response(body: &Value) -> Option<Response<BoxBody>> {
    let kinds = untranslatable_chat_tool_types(body);
    if kinds.is_empty() {
        return None;
    }
    warn!(
        "local router: refusing Chat Completions request with tool type(s) {} the route cannot carry",
        kinds.join(",")
    );
    Some(chat_invalid_request(
        "unsupported_tool_type",
        "tools",
        format!(
            "Rayline Local cannot send tool type(s) {} on this route: it translates Chat Completions to \
             Anthropic Messages, which supports only function tools. Declare them as function tools, or \
             route to an endpoint that speaks Chat Completions natively.",
            kinds.join(", ")
        ),
    ))
}

// --- request: Chat Completions -> Anthropic Messages ---------------------------------

fn chat_content_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn chat_content_to_anthropic(content: &Value) -> Vec<Value> {
    match content {
        Value::String(text) if !text.is_empty() => vec![json!({"type": "text", "text": text})],
        Value::Array(parts) => parts
            .iter()
            .filter_map(|part| match part.get("type").and_then(Value::as_str) {
                Some("text") => part
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| json!({"type": "text", "text": text})),
                Some("image_url") => part
                    .pointer("/image_url/url")
                    .or_else(|| part.get("image_url"))
                    .and_then(Value::as_str)
                    .map(openai_image_url_to_anthropic),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Append blocks for `role`, merging into the previous message when it has
/// the same role (Anthropic expects alternating turns; Chat allows runs, such
/// as several `tool` messages after one assistant turn).
fn push_blocks(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut()
        && last.get("role").and_then(Value::as_str) == Some(role)
        && let Some(content) = last.get_mut("content").and_then(Value::as_array_mut)
    {
        content.extend(blocks);
        return;
    }
    messages.push(json!({"role": role, "content": blocks}));
}

/// The Chat Completions request as an Anthropic Messages request for `model`,
/// plus the tool-name rewrites to undo on the way back.
pub(crate) fn chat_to_anthropic_request(body: &Value, model: &str) -> (Value, ToolNameRewrites) {
    let mut rewrites = ToolNameRewrites::default();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        rewrites.record_tools(tools);
    }
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in body
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let content = message.get("content").cloned().unwrap_or(Value::Null);
        match message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user")
        {
            "system" | "developer" => {
                let text = chat_content_text(&content);
                if !text.is_empty() {
                    system.push(text);
                }
            }
            "assistant" => {
                let mut blocks = chat_content_to_anthropic(&content);
                for call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("tool");
                    let input = call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                        .filter(Value::is_object)
                        .unwrap_or_else(|| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.get("id").and_then(Value::as_str).unwrap_or("call_rayline"),
                        "name": rewrites.upstream(name),
                        "input": input,
                    }));
                }
                push_blocks(&mut messages, "assistant", blocks);
            }
            "tool" => {
                push_blocks(
                    &mut messages,
                    "user",
                    vec![json!({
                        "type": "tool_result",
                        "tool_use_id": message.get("tool_call_id").and_then(Value::as_str).unwrap_or("call_rayline"),
                        "content": chat_content_text(&content),
                    })],
                );
            }
            _ => push_blocks(&mut messages, "user", chat_content_to_anthropic(&content)),
        }
    }
    if messages.is_empty() {
        messages.push(
            json!({"role": "user", "content": [{"type": "text", "text": EMPTY_TURN_PLACEHOLDER}]}),
        );
    }
    let mut out = Map::new();
    out.insert("model".to_owned(), json!(model));
    out.insert("messages".to_owned(), Value::Array(messages));
    out.insert(
        "max_tokens".to_owned(),
        body.get("max_completion_tokens")
            .or_else(|| body.get("max_tokens"))
            .cloned()
            .unwrap_or_else(|| json!(4096)),
    );
    out.insert(
        "stream".to_owned(),
        json!(body.get("stream").and_then(Value::as_bool).unwrap_or(false)),
    );
    if !system.is_empty() {
        out.insert("system".to_owned(), json!(system.join("\n\n")));
    }
    for key in ["temperature", "top_p"] {
        if let Some(value) = body.get(key).filter(|value| value.is_number()) {
            out.insert(key.to_owned(), value.clone());
        }
    }
    match body.get("stop") {
        Some(Value::String(stop)) => {
            out.insert("stop_sequences".to_owned(), json!([stop]));
        }
        Some(Value::Array(stops)) if !stops.is_empty() => {
            out.insert("stop_sequences".to_owned(), Value::Array(stops.clone()));
        }
        _ => {}
    }
    let tools: Vec<Value> = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|tool| tool.get("type").and_then(Value::as_str) == Some("function"))
        .filter_map(|tool| {
            let function = tool.get("function")?;
            let name = function.get("name")?.as_str()?;
            Some(json!({
                "name": rewrites.upstream(name),
                "description": function.get("description").and_then(Value::as_str).unwrap_or(""),
                "input_schema": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            }))
        })
        .collect();
    if !tools.is_empty() {
        out.insert("tools".to_owned(), Value::Array(tools));
        let mut choice = match body.get("tool_choice") {
            Some(Value::String(choice)) if choice == "none" => Some(json!({"type": "none"})),
            Some(Value::String(choice)) if choice == "required" => Some(json!({"type": "any"})),
            Some(Value::Object(choice)) => choice
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .map(|name| json!({"type": "tool", "name": rewrites.upstream(name)})),
            _ => None,
        };
        // `parallel_tool_calls: false` is Anthropic's
        // `tool_choice.disable_parallel_tool_use` (not valid with `none`).
        if body.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false) {
            let mut base = choice.unwrap_or_else(|| json!({"type": "auto"}));
            if base["type"] != "none" {
                base["disable_parallel_tool_use"] = json!(true);
            }
            choice = Some(base);
        }
        if let Some(choice) = choice {
            out.insert("tool_choice".to_owned(), choice);
        }
    }
    (Value::Object(out), rewrites)
}

// --- response: Anthropic Messages -> Chat Completions ----------------------------------

fn finish_reason(stop_reason: Option<&str>) -> &'static str {
    match stop_reason {
        Some("tool_use") => "tool_calls",
        Some("max_tokens") => "length",
        _ => "stop",
    }
}

fn chat_completion_id(decision: &RouteDecision) -> String {
    format!("chatcmpl_{}", decision.route_id.replace('-', "_"))
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

/// A complete Anthropic message as a `chat.completion`.
pub(crate) fn anthropic_message_to_chat(
    message: &Value,
    id: &str,
    model: &str,
    tool_names: &ToolNameRewrites,
) -> Value {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => text.push_str(block.get("text").and_then(Value::as_str).unwrap_or("")),
            Some("tool_use") => tool_calls.push(json!({
                "id": block.get("id").and_then(Value::as_str).unwrap_or("call_rayline"),
                "type": "function",
                "function": {
                    "name": tool_names.client(block.get("name").and_then(Value::as_str).unwrap_or("tool")),
                    "arguments": block.get("input").cloned().unwrap_or_else(|| json!({})).to_string(),
                },
            })),
            _ => {}
        }
    }
    let mut assistant = json!({
        "role": "assistant",
        "content": if text.is_empty() { Value::Null } else { json!(text) },
    });
    if !tool_calls.is_empty() {
        assistant["tool_calls"] = Value::Array(tool_calls);
    }
    let input = message
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output = message
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "id": id,
        "object": "chat.completion",
        "created": unix_now(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": assistant,
            "finish_reason": finish_reason(message.get("stop_reason").and_then(Value::as_str)),
        }],
        "usage": {"prompt_tokens": input, "completion_tokens": output, "total_tokens": input + output},
    })
}

async fn anthropic_response_to_chat(
    resp: reqwest::Response,
    decision: &RouteDecision,
    request_id: &str,
    want_stream: bool,
    include_usage: bool,
    tool_names: ToolNameRewrites,
) -> Result<Response<BoxBody>> {
    let status = resp.status();
    if !status.is_success() {
        return anthropic_error_to_chat(resp, status, decision, request_id).await;
    }
    let id = chat_completion_id(decision);
    let model = decision.requested_model.clone();
    let is_sse = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.to_ascii_lowercase().contains("text/event-stream"));
    if want_stream && is_sse {
        let (tx, rx) = mpsc::channel::<std::io::Result<Frame<Bytes>>>(16);
        tokio::spawn(async move {
            use futures::StreamExt;
            let mut translator =
                AnthropicToChatTranslator::new(id, model, include_usage, tool_names);
            let mut stream = resp.bytes_stream();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(bytes) => {
                        let out = translator.push_bytes(&bytes);
                        if !out.is_empty()
                            && tx.send(Ok(Frame::data(Bytes::from(out)))).await.is_err()
                        {
                            return;
                        }
                    }
                    Err(error) => {
                        // Like the Responses stream: no finish chunk and no
                        // `[DONE]`, and the body is aborted, so the client
                        // sees the answer was cut off.
                        let message = format!("upstream stream failed: {error}");
                        let out = translator.fail(&message);
                        if !out.is_empty() {
                            let _ = tx.send(Ok(Frame::data(Bytes::from(out)))).await;
                        }
                        let _ = tx.send(Err(std::io::Error::other(message))).await;
                        return;
                    }
                }
            }
            let tail = translator.end_of_stream();
            if !tail.is_empty() {
                let _ = tx.send(Ok(Frame::data(Bytes::from(tail)))).await;
            }
        });
        let body: BoxBody =
            StreamBody::new(tokio_stream::wrappers::ReceiverStream::new(rx)).boxed();
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .body(body)?;
        add_decision_headers(response.headers_mut(), decision);
        return Ok(response);
    }
    let message = if is_sse {
        anthropic_sse_text_to_message(&resp.text().await?, 0)
    } else {
        resp.json::<Value>().await?
    };
    let completion = anthropic_message_to_chat(&message, &id, &model, &tool_names);
    if want_stream {
        // The upstream answered in one piece: replay it as chunks.
        let mut translator = AnthropicToChatTranslator::new(id, model, include_usage, tool_names);
        let out = translator.replay_completion(&completion);
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/event-stream")
            .body(full_body(out))?;
        add_decision_headers(response.headers_mut(), decision);
        return Ok(response);
    }
    let mut response = json_response(StatusCode::OK, completion);
    add_decision_headers(response.headers_mut(), decision);
    Ok(response)
}

/// An Anthropic-shaped error body (`{"type":"error","error":{"type","message"}}`)
/// in OpenAI's shape (`{"error":{"message","type","param","code"}}`). Any other
/// body is wrapped as the message, so a Chat client always gets OpenAI's shape.
pub(crate) fn anthropic_error_body_to_openai(body: &[u8]) -> Value {
    let parsed = serde_json::from_slice::<Value>(body).ok();
    let error = parsed.as_ref().and_then(|value| value.get("error"));
    let kind = error
        .and_then(|error| error.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("api_error");
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| String::from_utf8_lossy(body).trim().to_owned());
    let code = error
        .and_then(|error| error.get("code"))
        .cloned()
        .unwrap_or(Value::Null);
    json!({"error": {"message": message, "type": kind, "param": Value::Null, "code": code}})
}

async fn anthropic_error_to_chat(
    resp: reqwest::Response,
    status: reqwest::StatusCode,
    decision: &RouteDecision,
    request_id: &str,
) -> Result<Response<BoxBody>> {
    warn!(
        "upstream returned HTTP {} (task={} selected={} request={request_id})",
        status.as_u16(),
        decision.task_class,
        decision.selected_model
    );
    let retry_after = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = resp.bytes().await.unwrap_or_default();
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = json_response(status, anthropic_error_body_to_openai(&body));
    if let Some(retry_after) = retry_after
        && let Ok(value) = HeaderValue::from_str(&retry_after)
    {
        response.headers_mut().insert("retry-after", value);
    }
    add_decision_headers(response.headers_mut(), decision);
    Ok(response)
}

/// Anthropic Messages SSE -> `chat.completion.chunk` SSE.
pub(crate) struct AnthropicToChatTranslator {
    id: String,
    model: String,
    created: u64,
    include_usage: bool,
    tool_names: ToolNameRewrites,
    /// Leading bytes of a UTF-8 character split across chunks.
    pending: Vec<u8>,
    buffer: String,
    /// Anthropic content-block index -> Chat `tool_calls` index.
    tool_index: HashMap<usize, usize>,
    stop_reason: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    started: bool,
    finished: bool,
}

impl AnthropicToChatTranslator {
    pub(crate) fn new(
        id: String,
        model: String,
        include_usage: bool,
        tool_names: ToolNameRewrites,
    ) -> Self {
        Self {
            id,
            model,
            created: unix_now(),
            include_usage,
            tool_names,
            pending: Vec::new(),
            buffer: String::new(),
            tool_index: HashMap::new(),
            stop_reason: None,
            input_tokens: 0,
            output_tokens: 0,
            started: false,
            finished: false,
        }
    }

    fn chunk(&self, delta: Value, finish: Option<&str>) -> String {
        let data = json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}],
        });
        format!("data: {data}\n\n")
    }

    fn ensure_started(&mut self, out: &mut String) {
        if !self.started {
            self.started = true;
            out.push_str(&self.chunk(json!({"role": "assistant", "content": ""}), None));
        }
    }

    pub(crate) fn push_bytes(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        // Decode up to an incomplete trailing character; its remaining bytes
        // arrive in the next chunk.
        let complete = match std::str::from_utf8(&self.pending) {
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            _ => self.pending.len(),
        };
        let rest = self.pending.split_off(complete);
        self.buffer
            .push_str(&String::from_utf8_lossy(&self.pending));
        self.pending = rest;
        let mut out = String::new();
        while !self.finished
            && let Some(raw) = drain_sse_event(&mut self.buffer)
        {
            if let Some((event, data)) = parse_sse_event(&raw)
                && let Ok(value) = serde_json::from_str::<Value>(&data)
            {
                self.process(&event, &value, &mut out);
            }
        }
        out
    }

    /// An OpenAI-shaped error event that ends the stream without a finish
    /// chunk or `[DONE]`, so the client can tell the answer is incomplete.
    pub(crate) fn fail(&mut self, message: &str) -> String {
        self.fail_with("api_error", message)
    }

    fn fail_with(&mut self, kind: &str, message: &str) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        let data = json!({"error": {"message": message, "type": kind,
                                    "param": Value::Null, "code": Value::Null}});
        format!("data: {data}\n\n")
    }

    /// The upstream body ended. A stream that never reached `message_stop`
    /// was cut off, so it is reported as a failure, not as finished.
    pub(crate) fn end_of_stream(&mut self) -> String {
        self.fail("upstream stream ended before the response completed")
    }

    fn process(&mut self, event: &str, value: &Value, out: &mut String) {
        match event {
            "message_start" => {
                self.input_tokens = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                self.ensure_started(out);
            }
            "content_block_start" => {
                self.ensure_started(out);
                if value.pointer("/content_block/type").and_then(Value::as_str) == Some("tool_use")
                {
                    let block = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let index = self.tool_index.len();
                    self.tool_index.insert(block, index);
                    let name = self.tool_names.client(
                        value
                            .pointer("/content_block/name")
                            .and_then(Value::as_str)
                            .unwrap_or("tool"),
                    );
                    let delta = json!({"tool_calls": [{
                        "index": index,
                        "id": value.pointer("/content_block/id").and_then(Value::as_str).unwrap_or("call_rayline"),
                        "type": "function",
                        "function": {"name": name, "arguments": ""},
                    }]});
                    out.push_str(&self.chunk(delta, None));
                }
            }
            "content_block_delta" => {
                self.ensure_started(out);
                match value.pointer("/delta/type").and_then(Value::as_str) {
                    Some("text_delta") => {
                        if let Some(text) = value.pointer("/delta/text").and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            out.push_str(&self.chunk(json!({"content": text}), None));
                        }
                    }
                    Some("input_json_delta") => {
                        let block =
                            value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                        if let (Some(index), Some(partial)) = (
                            self.tool_index.get(&block),
                            value.pointer("/delta/partial_json").and_then(Value::as_str),
                        ) && !partial.is_empty()
                        {
                            out.push_str(&self.chunk(
                                json!({"tool_calls": [{"index": index, "function": {"arguments": partial}}]}),
                                None,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(reason.to_owned());
                }
                if let Some(output) = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_u64)
                {
                    self.output_tokens = output;
                }
            }
            "message_stop" => out.push_str(&self.finish()),
            "error" => {
                let kind = value
                    .pointer("/error/type")
                    .and_then(Value::as_str)
                    .unwrap_or("api_error")
                    .to_owned();
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("upstream stream reported an error")
                    .to_owned();
                out.push_str(&self.fail_with(&kind, &message));
            }
            _ => {}
        }
    }

    /// The final chunk, the optional usage chunk, and `[DONE]`; once.
    pub(crate) fn finish(&mut self) -> String {
        if self.finished {
            return String::new();
        }
        self.finished = true;
        let mut out = String::new();
        self.ensure_started(&mut out);
        out.push_str(&self.chunk(json!({}), Some(finish_reason(self.stop_reason.as_deref()))));
        if self.include_usage {
            let data = json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": self.input_tokens,
                    "completion_tokens": self.output_tokens,
                    "total_tokens": self.input_tokens + self.output_tokens,
                },
            });
            out.push_str(&format!("data: {data}\n\n"));
        }
        out.push_str("data: [DONE]\n\n");
        out
    }

    /// A complete `chat.completion` as a chunk stream.
    fn replay_completion(&mut self, completion: &Value) -> String {
        let mut out = String::new();
        self.ensure_started(&mut out);
        let message = &completion["choices"][0]["message"];
        if let Some(text) = message.get("content").and_then(Value::as_str) {
            out.push_str(&self.chunk(json!({"content": text}), None));
        }
        for (index, call) in message
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .enumerate()
        {
            let mut call = call.clone();
            call["index"] = json!(index);
            out.push_str(&self.chunk(json!({"tool_calls": [call]}), None));
        }
        self.stop_reason = match completion["choices"][0]["finish_reason"].as_str() {
            Some("tool_calls") => Some("tool_use".to_owned()),
            Some("length") => Some("max_tokens".to_owned()),
            _ => None,
        };
        self.input_tokens = completion
            .pointer("/usage/prompt_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        self.output_tokens = completion
            .pointer("/usage/completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        out.push_str(&self.finish());
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_request_translates_messages_tools_and_controls() {
        let body = json!({
            "model": "arc-sage",
            "max_completion_tokens": 256,
            "temperature": 0.2,
            "stop": "END",
            "stream": true,
            "messages": [
                {"role": "system", "content": "Be brief."},
                {"role": "user", "content": [{"type": "text", "text": "Read the file."},
                                              {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}}]},
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function",
                     "function": {"name": "read.file", "arguments": "{\"path\":\"a.txt\"}"}}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "hello"},
                {"role": "user", "content": "And now?"}
            ],
            "tools": [{"type": "function", "function": {"name": "read.file", "description": "Read",
                       "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}}],
            "tool_choice": "required"
        });
        let (out, rewrites) = chat_to_anthropic_request(&body, "arc-sage");
        assert_eq!(out["system"], "Be brief.");
        assert_eq!(out["max_tokens"], 256);
        assert_eq!(out["temperature"], 0.2);
        assert_eq!(out["stop_sequences"], json!(["END"]));
        assert_eq!(out["stream"], true);
        assert_eq!(out["tool_choice"], json!({"type": "any"}));
        let upstream = rewrites.upstream("read.file").to_owned();
        assert_ne!(upstream, "read.file", "unsafe names are rewritten");
        assert_eq!(out["tools"][0]["name"], upstream);
        let messages = out["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][1]["type"], "image");
        assert_eq!(messages[1]["role"], "assistant");
        assert_eq!(messages[1]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["content"][0]["name"], upstream);
        assert_eq!(messages[1]["content"][0]["input"]["path"], "a.txt");
        // The tool result and the following user text merge into one user turn.
        assert_eq!(messages[2]["role"], "user");
        assert_eq!(messages[2]["content"][0]["type"], "tool_result");
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "call_1");
        assert_eq!(messages[2]["content"][1]["text"], "And now?");
        assert_eq!(messages.len(), 3);
    }

    #[test]
    fn custom_tools_are_refused_and_parallel_off_is_honoured() {
        let body = json!({"messages": [], "tools": [
            {"type": "function", "function": {"name": "shell"}},
            {"type": "custom", "custom": {"name": "apply_patch", "format": {"type": "grammar"}}}
        ]});
        assert_eq!(
            untranslatable_chat_tool_types(&body),
            vec!["custom".to_owned()]
        );
        let refusal = untranslatable_chat_tools_response(&body).expect("refused");
        assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
        assert!(
            untranslatable_chat_tools_response(
                &json!({"tools": [{"type": "function", "function": {"name": "shell"}}]})
            )
            .is_none()
        );
        let tools = json!([{"type": "function", "function": {"name": "shell"}}]);
        let (sequential, _) = chat_to_anthropic_request(
            &json!({"messages": [], "parallel_tool_calls": false, "tools": tools}),
            "m",
        );
        assert_eq!(
            sequential["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
        let (required, _) = chat_to_anthropic_request(
            &json!({"messages": [], "parallel_tool_calls": false, "tool_choice": "required",
                    "tools": tools}),
            "m",
        );
        assert_eq!(
            required["tool_choice"],
            json!({"type": "any", "disable_parallel_tool_use": true})
        );
        let (none, _) = chat_to_anthropic_request(
            &json!({"messages": [], "parallel_tool_calls": false, "tool_choice": "none",
                    "tools": tools}),
            "m",
        );
        assert_eq!(none["tool_choice"], json!({"type": "none"}));
    }

    #[test]
    fn anthropic_errors_reach_chat_clients_in_openai_shape() {
        let mapped = anthropic_error_body_to_openai(
            br#"{"type":"error","error":{"type":"invalid_request_error","code":"unsupported_server_tool","message":"no"}}"#,
        );
        assert_eq!(
            mapped,
            json!({"error": {"message": "no", "type": "invalid_request_error",
                             "param": null, "code": "unsupported_server_tool"}})
        );
        let plain = anthropic_error_body_to_openai(b"upstream exploded");
        assert_eq!(plain["error"]["message"], "upstream exploded");
        assert_eq!(plain["error"]["type"], "api_error");
    }

    #[test]
    fn anthropic_message_becomes_chat_completion_with_tool_calls() {
        let (_, rewrites) = chat_to_anthropic_request(
            &json!({"messages": [], "tools": [{"type": "function", "function": {"name": "read.file"}}]}),
            "m",
        );
        let upstream = rewrites.upstream("read.file").to_owned();
        let message = json!({
            "content": [{"type": "text", "text": "Reading."},
                        {"type": "tool_use", "id": "call_9", "name": upstream, "input": {"path": "a"}}],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "output_tokens": 4}
        });
        let completion = anthropic_message_to_chat(&message, "chatcmpl_x", "arc-sage", &rewrites);
        let choice = &completion["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["content"], "Reading.");
        assert_eq!(
            choice["message"]["tool_calls"][0]["function"]["name"],
            "read.file"
        );
        assert_eq!(
            choice["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"path\":\"a\"}"
        );
        assert_eq!(completion["usage"]["total_tokens"], 14);
        assert_eq!(completion["object"], "chat.completion");
    }

    #[test]
    fn chat_translator_reassembles_split_utf8_crlf_and_reports_errors() {
        let new = || {
            AnthropicToChatTranslator::new(
                "chatcmpl_x".to_owned(),
                "m".to_owned(),
                false,
                ToolNameRewrites::default(),
            )
        };
        let mut upstream = String::new();
        push_sse(
            &mut upstream,
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"café"}}),
        );
        push_sse(
            &mut upstream,
            "message_stop",
            json!({"type":"message_stop"}),
        );
        let bytes = upstream.replace('\n', "\r\n").into_bytes();
        let split = bytes
            .windows(2)
            .position(|pair| pair == "é".as_bytes())
            .expect("é")
            + 1;
        let mut translator = new();
        let mut out = translator.push_bytes(&bytes[..split]);
        out.push_str(&translator.push_bytes(&bytes[split..]));
        out.push_str(&translator.end_of_stream());
        assert!(out.contains("\"content\":\"café\""), "{out}");
        assert!(out.ends_with("data: [DONE]\n\n"), "{out}");

        let mut errored = new();
        let mut upstream = String::new();
        push_sse(
            &mut upstream,
            "error",
            json!({"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}),
        );
        let out = errored.push_bytes(upstream.as_bytes()) + &errored.end_of_stream();
        assert!(out.contains("\"type\":\"overloaded_error\""), "{out}");
        assert!(!out.contains("[DONE]"), "{out}");

        let mut cut = new();
        let out = cut.end_of_stream();
        assert!(out.contains("\"error\""), "{out}");
        assert!(!out.contains("[DONE]"), "{out}");
    }

    #[test]
    fn anthropic_stream_becomes_chat_chunks() {
        let mut translator = AnthropicToChatTranslator::new(
            "chatcmpl_x".to_owned(),
            "arc-swift".to_owned(),
            true,
            ToolNameRewrites::default(),
        );
        let mut upstream = String::new();
        push_sse(
            &mut upstream,
            "message_start",
            json!({"type":"message_start","message":{"usage":{"input_tokens":7}}}),
        );
        push_sse(
            &mut upstream,
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
        push_sse(
            &mut upstream,
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}),
        );
        push_sse(
            &mut upstream,
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"call_1","name":"shell","input":{}}}),
        );
        push_sse(
            &mut upstream,
            "content_block_delta",
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"ls\"}"}}),
        );
        push_sse(
            &mut upstream,
            "message_delta",
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
        );
        push_sse(
            &mut upstream,
            "message_stop",
            json!({"type":"message_stop"}),
        );
        let out = translator.push_bytes(upstream.as_bytes());
        let chunks: Vec<Value> = out
            .split("\n\n")
            .filter_map(|event| event.strip_prefix("data: "))
            .filter(|data| *data != "[DONE]")
            .map(|data| serde_json::from_str(data).expect("chunk json"))
            .collect();
        assert!(out.trim_end().ends_with("data: [DONE]"));
        assert_eq!(chunks[0]["choices"][0]["delta"]["role"], "assistant");
        assert!(
            chunks
                .iter()
                .any(|c| c["choices"][0]["delta"]["content"] == "Hi")
        );
        let named = chunks
            .iter()
            .find(|c| c["choices"][0]["delta"]["tool_calls"][0]["id"] == "call_1")
            .expect("tool call start");
        assert_eq!(
            named["choices"][0]["delta"]["tool_calls"][0]["function"]["name"],
            "shell"
        );
        assert!(chunks.iter().any(
            |c| c["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                == "{\"cmd\":\"ls\"}"
        ));
        assert!(
            chunks
                .iter()
                .any(|c| c["choices"][0]["finish_reason"] == "tool_calls")
        );
        let usage = chunks
            .iter()
            .find(|c| c.get("usage").is_some())
            .expect("usage chunk");
        assert_eq!(usage["usage"]["total_tokens"], 10);
    }
}
