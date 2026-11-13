//! The execution core's `token_counter` for each deployment whose model names
//! a `tokenizer`: the local process loads that model's published tokenizer and chat
//! template, serves the count on its own loopback listener with its own
//! credential, and hands the core a copy of its configuration that names it.
//!
//! Without a counter the core sizes a model it has no tokenizer for by bytes,
//! several times its real token count, and an image in the history makes that
//! size unknown, which the prefer-local policy reads as "does not fit".

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use rayline_tokencount::PromptCounter;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

/// The environment variable carrying the counter's credential to the core.
pub(crate) const COUNTER_KEY_ENV: &str = "RAYLINE_TOKEN_COUNTER_KEY";
const COUNTER_PATH: &str = "/v1/token-count";
/// Prepared requests carry base64 images; the core's own body limit.
const MAX_BODY_BYTES: usize = 64 << 20;
/// The core counts the same prepared body for capacity and for prefix
/// evidence within a turn; remember the last few.
const REMEMBERED: usize = 8;

pub(crate) struct Counter {
    counter: Arc<PromptCounter>,
    token: String,
    remembered: Mutex<VecDeque<([u8; 32], usize)>>,
}

/// The loaded counters, each with its listener, and the core configuration
/// that names them.
pub(crate) struct Prepared {
    pub(crate) counters: Vec<(Arc<Counter>, TcpListener)>,
    pub(crate) config: PathBuf,
}

/// Loads the model files of each `(deployment, dir)` and writes
/// `<config>.counted.json`: the core configuration with each deployment's
/// `token_counter`, `token_units` and, for a model with an image processor,
/// image input.
pub(crate) async fn prepare(
    counters: &[(String, PathBuf)],
    config: &Path,
    token: String,
) -> Result<Prepared> {
    let mut profile: Value = serde_json::from_slice(&std::fs::read(config)?)?;
    let mut prepared = Vec::with_capacity(counters.len());
    for (deployment, dir) in counters {
        let counter = tokio::task::spawn_blocking({
            let dir = dir.clone();
            move || PromptCounter::from_dir(&dir)
        })
        .await
        .context("load the prompt counter")?
        .with_context(|| {
            format!(
                "load the prompt counter for {deployment} from {}",
                dir.display()
            )
        })?;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let url = format!(
            "http://127.0.0.1:{}{COUNTER_PATH}",
            listener.local_addr()?.port()
        );
        name_counter(
            &mut profile,
            deployment,
            &url,
            counter.units(),
            counter.accepts_images(),
        )?;
        prepared.push((
            Arc::new(Counter {
                counter: Arc::new(counter),
                token: token.clone(),
                remembered: Mutex::new(VecDeque::new()),
            }),
            listener,
        ));
    }
    let counted = config.with_extension("counted.json");
    let _ = std::fs::remove_file(&counted);
    crate::hybrid::write_private_new(&counted, &serde_json::to_vec_pretty(&profile)?)?;
    Ok(Prepared {
        counters: prepared,
        config: counted,
    })
}

fn name_counter(
    profile: &mut Value,
    deployment: &str,
    url: &str,
    units: &str,
    images: bool,
) -> Result<()> {
    let target = profile
        .get_mut("deployments")
        .and_then(Value::as_array_mut)
        .and_then(|deployments| {
            deployments
                .iter_mut()
                .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(deployment))
        })
        .with_context(|| format!("the hybrid profile has no deployment {deployment}"))?;
    if target.get("kind").and_then(Value::as_str) != Some("local") {
        bail!("deployment {deployment} is not local");
    }
    let modalities = target
        .get_mut("input_modalities")
        .and_then(Value::as_array_mut)
        .with_context(|| format!("deployment {deployment} has no input_modalities"))?;
    if images && !modalities.iter().any(|modality| modality == "image") {
        modalities.push(json!("image"));
    }
    target["token_counter"] = json!({"url": url, "auth_env": COUNTER_KEY_ENV});
    target["token_units"] = json!(units);
    Ok(())
}

pub(crate) async fn serve(listener: TcpListener, counter: Arc<Counter>) -> Result<()> {
    let mut connections = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                let counter = counter.clone();
                connections.spawn(async move {
                    let service = service_fn(move |request| {
                        let counter = counter.clone();
                        async move { Ok::<_, Infallible>(counter.handle(request).await) }
                    });
                    if let Err(error) = auto::Builder::new(TokioExecutor::new())
                        .serve_connection(TokioIo::new(socket), service).await {
                        tracing::debug!(%error, "token counter connection closed");
                    }
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
        }
    }
}

impl Counter {
    async fn handle(self: Arc<Self>, request: Request<Incoming>) -> Response<Full<Bytes>> {
        if request.uri().path() != COUNTER_PATH {
            return reply(StatusCode::NOT_FOUND, json!({"error": "not found"}));
        }
        let authorized = request
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|presented| constant_time_eq(presented.as_bytes(), self.token.as_bytes()));
        if !authorized {
            return reply(StatusCode::UNAUTHORIZED, json!({"error": "unauthorized"}));
        }
        let units = self.counter.units().to_owned();
        match *request.method() {
            Method::GET => reply(StatusCode::OK, json!({"units": units})),
            Method::POST => {
                let body = match Limited::new(request.into_body(), MAX_BODY_BYTES)
                    .collect()
                    .await
                {
                    Ok(body) => body.to_bytes(),
                    Err(_) => {
                        return reply(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            json!({"error": "request too large"}),
                        );
                    }
                };
                match self.count(body).await {
                    Ok(tokens) => reply(
                        StatusCode::OK,
                        json!({"prompt_tokens": tokens, "units": units}),
                    ),
                    Err(error) => {
                        tracing::warn!(error = format!("{error:#}"), "token count failed");
                        reply(
                            StatusCode::UNPROCESSABLE_ENTITY,
                            json!({"error": format!("{error:#}")}),
                        )
                    }
                }
            }
            _ => reply(
                StatusCode::METHOD_NOT_ALLOWED,
                json!({"error": "method not allowed"}),
            ),
        }
    }

    async fn count(&self, body: Bytes) -> Result<usize> {
        let key: [u8; 32] = Sha256::digest(&body).into();
        if let Some(tokens) = self.recall(&key) {
            return Ok(tokens);
        }
        let counter = self.counter.clone();
        let tokens = tokio::task::spawn_blocking(move || -> Result<usize> {
            let request: Value =
                serde_json::from_slice(&body).context("parse the prepared request")?;
            Ok(counter.count_chat(&request)?.prompt_tokens)
        })
        .await??;
        let mut remembered = self
            .remembered
            .lock()
            .expect("token counter memory lock poisoned");
        if remembered.len() == REMEMBERED {
            remembered.pop_front();
        }
        remembered.push_back((key, tokens));
        Ok(tokens)
    }

    fn recall(&self, key: &[u8; 32]) -> Option<usize> {
        let remembered = self
            .remembered
            .lock()
            .expect("token counter memory lock poisoned");
        remembered
            .iter()
            .find(|(seen, _)| seen == key)
            .map(|(_, tokens)| *tokens)
    }
}

fn reply(status: StatusCode, value: Value) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(Bytes::from(value.to_string())));
    *response.status_mut() = status;
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("application/json"),
    );
    response
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Value {
        json!({"deployments": [
            {"id": "local", "kind": "local", "token_units": "", "input_modalities": ["text"]},
            {"id": "cloud-opus", "kind": "cloud", "input_modalities": ["text"]}
        ]})
    }

    #[test]
    fn names_the_counter_on_the_local_deployment_only() {
        let mut value = profile();
        name_counter(
            &mut value,
            "local",
            "http://127.0.0.1:9/v1/token-count",
            "hf-tokenizer:ab",
            true,
        )
        .unwrap();
        let local = &value["deployments"][0];
        assert_eq!(
            local["token_counter"],
            json!({"url": "http://127.0.0.1:9/v1/token-count", "auth_env": COUNTER_KEY_ENV})
        );
        assert_eq!(local["token_units"], "hf-tokenizer:ab");
        assert_eq!(local["input_modalities"], json!(["text", "image"]));
        assert_eq!(value["deployments"][1], profile()["deployments"][1]);
    }

    #[test]
    fn a_text_model_stays_text_only() {
        let mut value = profile();
        name_counter(&mut value, "local", "http://127.0.0.1:9/x", "u", false).unwrap();
        assert_eq!(value["deployments"][0]["input_modalities"], json!(["text"]));
    }

    #[test]
    fn refuses_a_cloud_or_missing_deployment() {
        assert!(name_counter(&mut profile(), "cloud-opus", "u", "u", false).is_err());
        assert!(name_counter(&mut profile(), "missing", "u", "u", false).is_err());
    }

    #[test]
    fn a_local_deployment_without_input_modalities_is_an_error() {
        for images in [false, true] {
            let mut value = profile();
            let local = value["deployments"][0].as_object_mut().unwrap();
            local.remove("input_modalities");
            let error = name_counter(&mut value, "local", "u", "u", images).unwrap_err();
            assert_eq!(
                error.to_string(),
                "deployment local has no input_modalities"
            );
            assert!(value["deployments"][0].get("token_counter").is_none());
        }
    }
}
