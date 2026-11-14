//! Times local ARC routing decisions phase by phase.
//!
//! Replays `/v1/routes` request bodies through the Routes API router with the
//! in-process decider and a running ARC encoder server, and prints, per turn,
//! where the time went. It reads files only; nothing here is shipped.
//!
//! ```text
//! cargo run --release -p rayline-daemon --example arc_turn_bench -- \
//!   --release DIR --encoder http://127.0.0.1:PORT --turns DIR [--repeat N] t1 t2 ...
//! ```
//!
//! Each turn `tN` is `DIR/tN.json` (the body as sent) and `DIR/tN.meta.json`
//! (`headers`, and the recorded `model`, `thinking_level`, `tokens`).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{Context, Result};
use rayline_arc_decide::{LocalEncoder, ReadoutRequest, Readouts, Release};
use rayline_arc_input::{ArcTokenizer, project_request};
use rayline_arc_routes::{Catalog, DecideFuture, Decider, Refusal, RequestHeaders, Router};
use serde_json::Value;

fn ms(since: Instant) -> f64 {
    since.elapsed().as_secs_f64() * 1e3
}

#[derive(Debug, Default, Clone)]
struct Phases {
    splice_ms: f64,
    project_ms: f64,
    tokenize_ms: f64,
    blocks: usize,
    tokens: usize,
    decide_ms: f64,
    readouts_ms: f64,
    decider_ms: f64,
}

struct Timed<'a> {
    inner: &'a LocalEncoder,
    ms: Mutex<f64>,
}

impl Readouts for Timed<'_> {
    fn readouts(&self, request: &ReadoutRequest<'_>) -> Result<Vec<Vec<f32>>, String> {
        let started = Instant::now();
        let rows = self.inner.readouts(request);
        if let Ok(mut total) = self.ms.lock() {
            *total += ms(started);
        }
        rows
    }
}

struct Bench {
    release: Arc<Release>,
    encoder: LocalEncoder,
    tokenizer: ArcTokenizer,
    last: Mutex<Phases>,
}

impl Bench {
    fn run(&self, body: &Value, request: Option<&str>) -> Result<Value, Refusal> {
        let started = Instant::now();
        let refuse = |code: &str, message: String| Refusal::new(code, message);
        // As the daemon's in-process decider does.
        let mut text = serde_json::to_string(body)
            .map_err(|error| refuse("internal_error", format!("decide body: {error}")))?;
        if let (Some(request), Some(rest)) = (request, body.as_object()) {
            let mut rest = rest.clone();
            rest.remove("request");
            let rest = serde_json::to_string(&rest)
                .map_err(|error| refuse("internal_error", format!("decide body: {error}")))?;
            text = match rest.strip_prefix('{') {
                Some("}") => format!("{{\"request\":{request}}}"),
                Some(members) => format!("{{\"request\":{request},{members}"),
                None => text,
            };
        }
        let input: rayline_arc_decide::DecideInput = serde_json::from_str(&text)
            .map_err(|error| refuse("invalid_request", format!("decide body: {error}")))?;
        let mut phases = Phases {
            splice_ms: ms(started),
            ..Phases::default()
        };

        // The projection and tokenization alone (decide repeats them).
        let at = Instant::now();
        let blocks = project_request(input.request.get(), &[])
            .map_err(|error| refuse("projection", error.to_string()))?;
        phases.project_ms = ms(at);
        let at = Instant::now();
        let encoded = self
            .tokenizer
            .encode(&blocks)
            .map_err(|error| refuse("projection", error.to_string()))?;
        phases.tokenize_ms = ms(at);
        phases.blocks = blocks.len();
        phases.tokens = encoded.tokens.len();
        if std::env::var_os("ARC_BENCH_PREFIXES").is_some() {
            // Per block end: the offset and a digest of the tokens up to it,
            // to see which prefixes conversations share. No content.
            use std::hash::{Hash, Hasher};
            let ends: Vec<String> = encoded
                .readout_offsets
                .iter()
                .map(|end| {
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    encoded.tokens[..=*end].hash(&mut hasher);
                    format!("{end}:{:06x}", hasher.finish() & 0xff_ffff)
                })
                .collect();
            eprintln!("prefixes {}", ends.join(" "));
        }

        let timed = Timed {
            inner: &self.encoder,
            ms: Mutex::new(0.0),
        };
        let at = Instant::now();
        let output = rayline_arc_decide::decide(&self.release, &input, &timed)
            .map_err(|error| refuse(error.code(), error.to_string()))?;
        phases.decide_ms = ms(at);
        phases.readouts_ms = timed.ms.lock().map(|v| *v).unwrap_or_default();
        let out = serde_json::to_value(output)
            .map_err(|error| refuse("internal_error", format!("decide answer: {error}")))?;
        phases.decider_ms = ms(started) - (phases.project_ms + phases.tokenize_ms);
        if let Ok(mut last) = self.last.lock() {
            *last = phases;
        }
        Ok(out)
    }
}

impl Decider for Bench {
    fn decide<'a>(&'a self, body: &'a Value) -> DecideFuture<'a> {
        Box::pin(async move { self.run(body, None) })
    }

    fn decide_with_text<'a>(&'a self, body: &'a Value, request: &'a str) -> DecideFuture<'a> {
        Box::pin(async move { self.run(body, Some(request)) })
    }
}

struct Args {
    release: PathBuf,
    encoder: String,
    turns: PathBuf,
    repeat: usize,
    names: Vec<String>,
}

fn args() -> Result<Args> {
    let mut it = std::env::args().skip(1);
    let (mut release, mut encoder, mut turns, mut repeat, mut names) =
        (None, None, None, 1, Vec::new());
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--release" => release = it.next().map(PathBuf::from),
            "--encoder" => encoder = it.next(),
            "--turns" => turns = it.next().map(PathBuf::from),
            "--repeat" => repeat = it.next().context("--repeat N")?.parse()?,
            _ => names.push(arg),
        }
    }
    Ok(Args {
        release: release.context("--release DIR")?,
        encoder: encoder.context("--encoder URL")?,
        turns: turns.context("--turns DIR")?,
        repeat,
        names,
    })
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args = args()?;
    let at = Instant::now();
    let release = Release::load(&args.release).context("load the release")?;
    let tokenizer = ArcTokenizer::from_release(
        &std::fs::read(args.release.join("tokenizer.json"))?,
        &std::fs::read_to_string(args.release.join("tokenizer_config.json"))?,
    )?;
    eprintln!("release loaded in {:.0} ms", ms(at));
    let manifest = std::fs::read(args.release.join("package.json"))?;
    let catalog = Catalog::from_manifest(&manifest, "rayline/arc-1.0-20261107")?;
    let mut config = rayline_arc_routes::RouterConfig::new(catalog, "http://127.0.0.1:9");
    config.salt = "0".repeat(32);
    let bench = Arc::new(Bench {
        release: Arc::new(release),
        encoder: LocalEncoder::new(&args.encoder)
            .map_err(anyhow::Error::msg)?
            .with_stats(),
        tokenizer,
        last: Mutex::default(),
    });
    let router = Router::new(config, bench.clone())?;

    println!(
        "turn,rep,tokens,blocks,route_ms,router_ms,splice_ms,project_ms,tokenize_ms,decide_ms,readouts_ms,\
         enc_tokens,enc_req_kb,enc_resp_kb,enc_serialize_ms,enc_http_ms,enc_server_ms,enc_parse_ms,restored,\
         model,thinking,match"
    );
    for rep in 0..args.repeat {
        for name in &args.names {
            let raw = std::fs::read(args.turns.join(format!("{name}.json")))?;
            let meta: Value = serde_json::from_slice(&std::fs::read(
                args.turns.join(format!("{name}.meta.json")),
            )?)?;
            let header = |key: &str| meta["headers"][key].as_str().map(str::to_owned);
            let headers = RequestHeaders {
                conversation_id: header("x-conversation-id"),
                branch_id: header("x-rayline-branch-id"),
                wire_format: header("x-rayline-wire-format"),
                cell: None,
            };
            let body: Value = serde_json::from_slice(&raw)?;
            let at = Instant::now();
            let out = router
                .route_sent(body, Some(&raw), &headers)
                .await
                .map_err(|refusal| anyhow::anyhow!("{}: {}", refusal.code, refusal.message))?;
            let route_ms = ms(at);
            let p = bench.last.lock().map(|p| p.clone()).unwrap_or_default();
            let stats = bench.encoder.take_stats();
            let sum = |f: &dyn Fn(&rayline_arc_decide::EncodeStats) -> f64| -> f64 {
                stats.iter().map(f).sum()
            };
            let model = out["model"].as_str().unwrap_or("-");
            let thinking = out["thinking_level"].as_str().unwrap_or("-");
            let same = meta["model"].as_str() == Some(model)
                && meta["thinking_level"].as_str().unwrap_or("-") == thinking
                && meta["tokens"].as_u64() == out["usage"]["encoded_input_tokens"].as_u64();
            println!(
                "{name},{rep},{},{},{route_ms:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{},{:.0},{:.0},{:.1},{:.1},{:.1},{:.1},{},{model},{thinking},{same}",
                p.tokens,
                p.blocks,
                route_ms - p.decider_ms,
                p.splice_ms,
                p.project_ms,
                p.tokenize_ms,
                p.decide_ms,
                p.readouts_ms,
                stats.iter().map(|s| s.tokens).sum::<usize>(),
                sum(&|s| s.request_bytes as f64) / 1024.0,
                sum(&|s| s.response_bytes as f64) / 1024.0,
                sum(&|s| s.serialize_ms),
                sum(&|s| s.http_ms),
                sum(&|s| s.server_ms.unwrap_or(f64::NAN)),
                sum(&|s| s.parse_ms),
                stats
                    .iter()
                    .filter_map(|s| s.restored_tokens)
                    .sum::<usize>(),
            );
        }
    }
    Ok(())
}
