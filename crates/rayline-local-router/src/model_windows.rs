//! Real context windows for the Codex model catalog (`GET /v1/models`).
//!
//! Codex sizes its usable window and its auto-compaction threshold from each
//! catalog entry's `context_window`, so advertising one fixed value for every
//! model makes Codex compact too early on large-window models (and too late on
//! small local ones). Instead, the router asks each configured endpoint what
//! its models accept and caches the answer per endpoint:
//!
//! - any endpoint: its model list (`GET …/v1/models`), reading `context_length`,
//!   `context_window` or `max_model_len` per entry. Entries are keyed by `id`
//!   and, when present, by `product`, so a config that names a product id still
//!   matches a listing whose `id` is more specific.
//! - loopback runtimes, for models the list does not size: Ollama's
//!   `GET /api/ps` (`context_length` of a loaded model) and then
//!   `POST /api/show` (the model's `num_ctx` parameter), or llama.cpp's
//!   `GET /props` (`n_ctx`). None of these load a model.
//! - subscription (`client_bearer`) endpoints: the upstream catalog the router
//!   already proxies for `/v1/models` (see [`listed_windows`]).
//!
//! A model that no source sizes keeps the catalog's default entry. An answer
//! is reused for [`REFRESH_AFTER`], then asked again, so a model loaded (or
//! reloaded with another context) later is picked up. A failed lookup keeps the
//! previous answer for the models it did not size, and a failed endpoint is
//! retried on the next `/v1/models` request.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{
    AppState, AuthMode, AuthStyle, EndpointConfig, EndpointProtocol, RouterConfig,
    apply_endpoint_headers, endpoint_hidden_from_catalog,
};

/// Per-request timeout for every window lookup.
const FETCH_TIMEOUT: Duration = Duration::from_secs(2);
/// Upper bound for sizing one endpoint (its list plus any runtime lookups).
/// Codex gives its whole `/v1/models` refresh a few seconds, and endpoints are
/// sized concurrently, so one slow endpoint cannot hold the catalog for longer.
const ENDPOINT_TIMEOUT: Duration = Duration::from_secs(3);
/// How long an endpoint's answer is reused before it is asked again. Codex
/// itself refetches `/v1/models` only every few minutes, so this mainly
/// collapses bursts of requests into one lookup.
const REFRESH_AFTER: Duration = Duration::from_secs(60);

/// A model's context window as Codex's catalog describes it: the window it
/// runs with by default, and the largest a user override may raise it to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModelWindow {
    pub(crate) context: u64,
    pub(crate) max: u64,
}

impl From<u64> for ModelWindow {
    /// A source that reports one window: it is both the default and the cap.
    fn from(window: u64) -> Self {
        Self {
            context: window,
            max: window,
        }
    }
}

/// Context windows by model id, for one endpoint.
pub(crate) type EndpointWindows = HashMap<String, ModelWindow>;
/// Context windows by endpoint id, then model id.
pub(crate) type Windows = HashMap<String, EndpointWindows>;

/// Endpoint windows resolved so far, with when each was fetched. Only
/// successful lookups are kept.
#[derive(Default)]
pub(crate) struct ModelWindowCache {
    resolved: Mutex<HashMap<String, (Instant, EndpointWindows)>>,
}

impl ModelWindowCache {
    /// Windows for every endpoint that can be sized without the client's
    /// credentials, fetching the endpoints not resolved recently concurrently.
    pub(crate) async fn windows(&self, state: &AppState) -> Windows {
        let pending = {
            let resolved = self.resolved.lock().await;
            state
                .config
                .endpoints
                .iter()
                .filter(|endpoint| {
                    sized_by_lookup(endpoint)
                        && resolved
                            .get(&endpoint.id)
                            .is_none_or(|(fetched_at, _)| fetched_at.elapsed() >= REFRESH_AFTER)
                })
                .collect::<Vec<_>>()
        };
        if !pending.is_empty() {
            let fetched =
                futures::future::join_all(pending.into_iter().map(|endpoint| async move {
                    let models = models_to_size(&state.config, endpoint);
                    let deadline = tokio::time::Instant::now() + ENDPOINT_TIMEOUT;
                    let windows =
                        fetch_endpoint_windows(&state.http, endpoint, &models, deadline).await;
                    (endpoint.id.clone(), windows)
                }))
                .await;
            let now = Instant::now();
            let mut resolved = self.resolved.lock().await;
            for (id, windows) in fetched {
                // A refresh updates the models it sized and keeps earlier
                // answers for the rest: a runtime lookup that fails this time
                // must not drop a window that was already known.
                if let Some(windows) = windows {
                    let (fetched_at, known) = resolved
                        .entry(id)
                        .or_insert_with(|| (now, EndpointWindows::new()));
                    *fetched_at = now;
                    known.extend(windows);
                }
            }
        }
        self.resolved
            .lock()
            .await
            .iter()
            .map(|(id, (_, windows))| (id.clone(), windows.clone()))
            .collect()
    }

    /// Makes every cached answer due for a refresh.
    #[cfg(test)]
    pub(crate) async fn expire(&self) {
        for (fetched_at, _) in self.resolved.lock().await.values_mut() {
            *fetched_at = fetched_at
                .checked_sub(REFRESH_AFTER)
                .expect("an instant REFRESH_AFTER in the past");
        }
    }
}

/// The models to size on `endpoint`: the ones it lists in the config, plus any
/// model a route sends to it (a route may name a model the endpoint does not
/// list, and the catalog sentinel takes that route's window).
fn models_to_size(config: &RouterConfig, endpoint: &EndpointConfig) -> Vec<String> {
    let routes = &config.routes;
    let routed = [&routes.main, &routes.subagent, &routes.default]
        .into_iter()
        .flatten()
        .chain(routes.model_routes.values())
        .chain(routes.subagents.values())
        .filter(|route| route.endpoint == endpoint.id)
        .map(|route| &route.model);
    let mut models = Vec::new();
    for model in endpoint.models.iter().chain(routed) {
        if !models.contains(model) {
            models.push(model.clone());
        }
    }
    models
}

/// Endpoints the router sizes itself. Subscription endpoints need the inbound
/// client's bearer, so they are sized from the catalog proxied for that client
/// instead; endpoints hidden from the catalog (missing key) are skipped.
fn sized_by_lookup(endpoint: &EndpointConfig) -> bool {
    endpoint.auth != Some(AuthMode::ClientBearer) && !endpoint_hidden_from_catalog(endpoint)
}

/// Sizes `models` on one endpoint. The listing is asked first; a loopback
/// runtime is then asked about the models the listing did not size, within
/// what is left of `deadline`, so a slow runtime cannot discard the windows the
/// listing (or the runtime's own earlier answers) already gave. `None` when
/// nothing on the endpoint answered.
async fn fetch_endpoint_windows(
    http: &reqwest::Client,
    endpoint: &EndpointConfig,
    models: &[String],
    deadline: tokio::time::Instant,
) -> Option<EndpointWindows> {
    let listed = by(
        deadline,
        get_json(http, endpoint, &models_url(&endpoint.base_url)),
    )
    .await;
    let mut reachable = listed.is_some();
    let mut windows = listed.as_ref().map(listed_windows).unwrap_or_default();
    let missing = models
        .iter()
        .filter(|model| !windows.contains_key(*model))
        .cloned()
        .collect::<Vec<_>>();
    if is_loopback(&endpoint.base_url) && !missing.is_empty() {
        let root = runtime_root(&endpoint.base_url);
        if let Some(runtime) = runtime_windows(http, endpoint, &root, &missing, deadline).await {
            reachable = true;
            windows.extend(runtime);
        }
    }
    reachable.then_some(windows)
}

/// `lookup`'s answer, or `None` once `deadline` passes. Each lookup is bounded
/// on its own, so the ones that finished in time keep their answers.
async fn by<T>(
    deadline: tokio::time::Instant,
    lookup: impl Future<Output = Option<T>>,
) -> Option<T> {
    tokio::time::timeout_at(deadline, lookup)
        .await
        .ok()
        .flatten()
}

/// Windows a local runtime reports for `missing`: Ollama (`/api/ps`, then
/// `/api/show`) when it answers `/api/ps`, else llama.cpp (`/props`). `None`
/// when the runtime answered none of these.
async fn runtime_windows(
    http: &reqwest::Client,
    endpoint: &EndpointConfig,
    root: &str,
    missing: &[String],
    deadline: tokio::time::Instant,
) -> Option<EndpointWindows> {
    if let Some(ps) = by(
        deadline,
        get_json(http, endpoint, &format!("{root}/api/ps")),
    )
    .await
    {
        return Some(ollama_windows(http, endpoint, root, &ps, missing, deadline).await);
    }
    let mut reachable = false;
    let mut windows = EndpointWindows::new();
    for (model, props) in futures::future::join_all(missing.iter().map(|model| async move {
        let url =
            reqwest::Url::parse_with_params(&format!("{root}/props"), [("model", model.as_str())])
                .ok()?;
        Some((
            model,
            by(deadline, get_json(http, endpoint, url.as_str())).await?,
        ))
    }))
    .await
    .into_iter()
    .flatten()
    {
        reachable = true;
        if let Some(window) = llama_props_window(&props) {
            windows.insert(model.clone(), window.into());
        }
    }
    reachable.then_some(windows)
}

/// Ollama: a loaded model's `context_length` from `/api/ps` is the window it is
/// actually running with; otherwise the `num_ctx` parameter from `/api/show`
/// (baked into the tag) is the window it will load with. A model with neither
/// gets Ollama's host-dependent default, which the router cannot know.
async fn ollama_windows(
    http: &reqwest::Client,
    endpoint: &EndpointConfig,
    root: &str,
    ps: &Value,
    missing: &[String],
    deadline: tokio::time::Instant,
) -> EndpointWindows {
    let mut windows = EndpointWindows::new();
    let mut unloaded = Vec::new();
    for model in missing {
        match ollama_loaded_window(ps, model) {
            Some(window) => {
                windows.insert(model.clone(), window.into());
            }
            None => unloaded.push(model),
        }
    }
    let shown = futures::future::join_all(unloaded.into_iter().map(|model| async move {
        let request = http
            .post(format!("{root}/api/show"))
            .json(&json!({ "model": model }));
        let window = by(deadline, send_json(request, endpoint))
            .await
            .as_ref()
            .and_then(ollama_num_ctx);
        (model, window)
    }))
    .await;
    for (model, window) in shown {
        if let Some(window) = window {
            windows.insert(model.clone(), window.into());
        }
    }
    windows
}

async fn get_json(http: &reqwest::Client, endpoint: &EndpointConfig, url: &str) -> Option<Value> {
    send_json(http.get(url), endpoint).await
}

async fn send_json(request: reqwest::RequestBuilder, endpoint: &EndpointConfig) -> Option<Value> {
    let protocol_default = match endpoint.protocol {
        EndpointProtocol::AnthropicMessages => AuthStyle::Anthropic,
        EndpointProtocol::OpenAIChat | EndpointProtocol::OpenAIResponses => AuthStyle::Bearer,
    };
    let request =
        apply_endpoint_headers(request.timeout(FETCH_TIMEOUT), endpoint, protocol_default).ok()?;
    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().await.ok()
}

/// The model-list URL for an endpoint. OpenAI-style base URLs already end in
/// `/v1` (`https://api.openai.com/v1`, `http://127.0.0.1:11434/v1`); the others
/// (`https://api.anthropic.com`, `https://openrouter.ai/api`) do not.
fn models_url(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/models")
    } else {
        format!("{base}/v1/models")
    }
}

/// The server root of a local runtime, where its native (non-`/v1`) API lives.
fn runtime_root(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_owned()
}

fn is_loopback(base_url: &str) -> bool {
    reqwest::Url::parse(base_url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?;
            let host = host.trim_start_matches('[').trim_end_matches(']');
            Some(
                host.eq_ignore_ascii_case("localhost")
                    || host
                        .parse::<std::net::IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback()),
            )
        })
        .unwrap_or(false)
}

/// Windows from a model list: OpenAI-style `data[]` keyed by `id` (and by
/// `product` when the entry names one), and Codex-style `models[]` keyed by
/// `slug`, keeping a Codex entry's `max_context_window`. Entries that state no
/// window are skipped.
pub(crate) fn listed_windows(value: &Value) -> EndpointWindows {
    let mut windows = EndpointWindows::new();
    for (list, key) in [("data", "id"), ("models", "slug")] {
        let Some(entries) = value.get(list).and_then(Value::as_array) else {
            continue;
        };
        for entry in entries {
            let Some(context) = ["context_length", "context_window", "max_model_len"]
                .into_iter()
                .find_map(|field| positive(entry.get(field)))
            else {
                continue;
            };
            // A Codex catalog may allow overriding the window up to a larger
            // `max_context_window`; keep that cap rather than lowering it.
            let window = ModelWindow {
                context,
                max: positive(entry.get("max_context_window"))
                    .map_or(context, |max| max.max(context)),
            };
            for name in [key, "product"] {
                if let Some(name) = entry.get(name).and_then(Value::as_str) {
                    windows.entry(name.to_owned()).or_insert(window);
                }
            }
        }
    }
    windows
}

/// The window of `model` if Ollama has it loaded (`/api/ps`). Ollama lists a
/// tag without an explicit tag name as `<name>:latest`.
fn ollama_loaded_window(ps: &Value, model: &str) -> Option<u64> {
    let latest = format!("{model}:latest");
    ps.get("models")?.as_array()?.iter().find_map(|entry| {
        let named = ["name", "model"].into_iter().any(|field| {
            entry
                .get(field)
                .and_then(Value::as_str)
                .is_some_and(|name| name == model || name == latest)
        });
        if named {
            positive(entry.get("context_length"))
        } else {
            None
        }
    })
}

/// The `num_ctx` parameter from an Ollama `/api/show` answer, whose
/// `parameters` field is the Modelfile's `PARAMETER` lines (`num_ctx  32768`).
fn ollama_num_ctx(show: &Value) -> Option<u64> {
    show.get("parameters")?.as_str()?.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == "num_ctx")
            .then(|| parts.next()?.parse::<u64>().ok())
            .flatten()
            .filter(|window| *window > 0)
    })
}

/// The per-slot window from a llama.cpp `/props` answer.
fn llama_props_window(props: &Value) -> Option<u64> {
    positive(props.pointer("/default_generation_settings/n_ctx"))
        .or_else(|| positive(props.get("n_ctx")))
}

fn positive(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_u64).filter(|value| *value > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listed_windows_reads_hosted_list_by_id_and_product() {
        let windows = listed_windows(&json!({
            "object": "list",
            "data": [
                {"id": "arc-swift-contributor-1.0", "product": "arc-swift-1.0",
                 "context_length": 1_000_000, "max_output_tokens": 128_000},
                {"id": "arc-sage-1.0", "product": "arc-sage-1.0", "context_length": 1_000_000},
                {"id": "no-window"}
            ]
        }));
        assert_eq!(
            windows.get("arc-swift-contributor-1.0"),
            Some(&ModelWindow::from(1_000_000))
        );
        assert_eq!(
            windows.get("arc-swift-1.0"),
            Some(&ModelWindow::from(1_000_000))
        );
        assert_eq!(
            windows.get("arc-sage-1.0"),
            Some(&ModelWindow::from(1_000_000))
        );
        assert!(!windows.contains_key("no-window"));
    }

    #[test]
    fn listed_windows_reads_vllm_and_codex_catalogs() {
        let vllm = listed_windows(&json!({"data": [{"id": "qwen", "max_model_len": 32_768}]}));
        assert_eq!(vllm.get("qwen"), Some(&ModelWindow::from(32_768)));
        let codex = listed_windows(&json!({
            "models": [
                {"slug": "gpt-big", "context_window": 272_000, "max_context_window": 872_000},
                {"slug": "zero", "context_window": 0}
            ]
        }));
        assert_eq!(
            codex.get("gpt-big"),
            Some(&ModelWindow {
                context: 272_000,
                max: 872_000
            })
        );
        assert!(!codex.contains_key("zero"));
    }

    #[test]
    fn ollama_windows_prefer_loaded_then_num_ctx() {
        let ps = json!({"models": [
            {"name": "qwen3:27b-96k", "model": "qwen3:27b-96k", "context_length": 98_304},
            {"name": "llama3:latest", "model": "llama3:latest", "context_length": 8_192}
        ]});
        assert_eq!(ollama_loaded_window(&ps, "qwen3:27b-96k"), Some(98_304));
        assert_eq!(ollama_loaded_window(&ps, "llama3"), Some(8_192));
        assert_eq!(ollama_loaded_window(&ps, "qwen2.5-coder:7b-32k"), None);

        let show = json!({"parameters": "stop                           \"<|im_end|>\"\nnum_ctx                        32768\ntemperature 0.7"});
        assert_eq!(ollama_num_ctx(&show), Some(32_768));
        assert_eq!(
            ollama_num_ctx(&json!({"parameters": "temperature 0.7"})),
            None
        );
        assert_eq!(ollama_num_ctx(&json!({})), None);
    }

    #[test]
    fn llama_props_window_reads_slot_context() {
        assert_eq!(
            llama_props_window(&json!({"default_generation_settings": {"n_ctx": 16_384}})),
            Some(16_384)
        );
        assert_eq!(llama_props_window(&json!({"n_ctx": 8_192})), Some(8_192));
        assert_eq!(llama_props_window(&json!({"total_slots": 1})), None);
    }

    #[test]
    fn urls_follow_base_url_conventions() {
        assert_eq!(
            models_url("https://api.rayline.ai"),
            "https://api.rayline.ai/v1/models"
        );
        assert_eq!(
            models_url("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_url("https://openrouter.ai/api"),
            "https://openrouter.ai/api/v1/models"
        );
        assert_eq!(
            runtime_root("http://127.0.0.1:11434/v1"),
            "http://127.0.0.1:11434"
        );
        assert_eq!(
            runtime_root("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080"
        );
        assert!(is_loopback("http://127.0.0.1:11434/v1"));
        assert!(is_loopback("http://localhost:8080"));
        assert!(is_loopback("http://[::1]:8080"));
        assert!(!is_loopback("https://api.rayline.ai"));
    }

    /// A loopback runtime that answers by request-line prefix; a `None` body
    /// never answers (a stalled runtime), and anything unknown gets a 404.
    async fn runtime_stub(routes: Vec<(&'static str, Option<Value>)>) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let response = match routes
                        .iter()
                        .find(|(prefix, _)| request.starts_with(prefix))
                    {
                        Some((_, Some(body))) => {
                            let body = body.to_string();
                            format!(
                                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\
                                 content-length: {}\r\nconnection: close\r\n\r\n{body}",
                                body.len()
                            )
                        }
                        Some((_, None)) => {
                            tokio::time::sleep(Duration::from_secs(30)).await;
                            return;
                        }
                        None => "HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\
                                 connection: close\r\n\r\n"
                            .to_owned(),
                    };
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        format!("http://127.0.0.1:{port}/v1")
    }

    fn runtime_endpoint(base_url: String, models: &[&str]) -> EndpointConfig {
        EndpointConfig {
            id: "runtime".to_owned(),
            kind: "provider".to_owned(),
            protocol: EndpointProtocol::OpenAIChat,
            base_url,
            api_key_env: None,
            models: models.iter().map(|m| (*m).to_owned()).collect(),
            headers: HashMap::new(),
            auth: None,
        }
    }

    #[test]
    fn models_to_size_include_routed_models() {
        let endpoint = runtime_endpoint("http://127.0.0.1:1/v1".to_owned(), &["listed"]);
        let route = |endpoint: &str, model: &str| crate::RouteTarget {
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
            ..Default::default()
        };
        let mut config = RouterConfig {
            endpoints: vec![endpoint.clone()],
            routes: Default::default(),
        };
        config.routes.main = Some(route("runtime", "routed"));
        config.routes.subagent = Some(route("elsewhere", "other"));
        config
            .routes
            .model_routes
            .insert("rayline-local".to_owned(), route("runtime", "listed"));
        assert_eq!(models_to_size(&config, &endpoint), ["listed", "routed"]);
    }

    /// A runtime that stalls past the deadline does not discard the windows
    /// the listing already gave.
    #[tokio::test]
    async fn a_stalled_runtime_keeps_the_listed_windows() {
        let base = runtime_stub(vec![
            (
                "GET /v1/models ",
                Some(json!({"data": [{"id": "sized", "context_length": 65_536}, {"id": "bare"}]})),
            ),
            ("GET /api/ps ", None),
        ])
        .await;
        let endpoint = runtime_endpoint(base, &["sized", "bare"]);
        let models = vec!["sized".to_owned(), "bare".to_owned()];
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let windows =
            fetch_endpoint_windows(&reqwest::Client::new(), &endpoint, &models, deadline).await;
        let windows = windows.expect("the listing answered");
        assert_eq!(windows.get("sized"), Some(&ModelWindow::from(65_536)));
        assert!(!windows.contains_key("bare"));
    }

    /// A loaded model keeps its `/api/ps` window when `/api/show` for another
    /// model stalls past the deadline.
    #[tokio::test]
    async fn a_stalled_show_keeps_the_loaded_windows() {
        let base = runtime_stub(vec![
            (
                "GET /v1/models ",
                Some(json!({"data": [{"id": "loaded"}, {"id": "cold"}]})),
            ),
            (
                "GET /api/ps ",
                Some(json!({"models": [{"name": "loaded", "context_length": 8_192}]})),
            ),
            ("POST /api/show ", None),
        ])
        .await;
        let endpoint = runtime_endpoint(base, &["loaded", "cold"]);
        let models = vec!["loaded".to_owned(), "cold".to_owned()];
        let deadline = tokio::time::Instant::now() + Duration::from_millis(500);
        let windows =
            fetch_endpoint_windows(&reqwest::Client::new(), &endpoint, &models, deadline).await;
        let windows = windows.expect("the runtime answered");
        assert_eq!(windows.get("loaded"), Some(&ModelWindow::from(8_192)));
        assert!(!windows.contains_key("cold"));
    }

    /// A model only a route names (the endpoint lists none) is still sized by
    /// the runtime.
    #[tokio::test]
    async fn a_routed_model_is_sized_by_the_runtime() {
        let base = runtime_stub(vec![
            ("GET /v1/models ", Some(json!({"data": [{"id": "routed"}]}))),
            (
                "GET /api/ps ",
                Some(json!({"models": [{"name": "routed", "context_length": 8_192}]})),
            ),
        ])
        .await;
        let endpoint = runtime_endpoint(base, &[]);
        let models = vec!["routed".to_owned()];
        let deadline = tokio::time::Instant::now() + ENDPOINT_TIMEOUT;
        let windows =
            fetch_endpoint_windows(&reqwest::Client::new(), &endpoint, &models, deadline).await;
        assert_eq!(
            windows.expect("the runtime answered").get("routed"),
            Some(&ModelWindow::from(8_192))
        );
    }
}
