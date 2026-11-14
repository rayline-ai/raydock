//! Parity with the reference projection.
//!
//! `fixtures/arc-1.0-20261111-goldens.jsonl` holds, for every golden request
//! published with the ARC 1.0 weights (release `arc-1.0-20261111`), what the
//! reference implementation gives the encoder: the role blocks, the token ids
//! and the readout offsets. Each line is
//! `{"case_id", "body", "sources", "blocks": [[role, content]], "tokens", "offsets"}`,
//! with `body` the request as JSON text.
//!
//! The block comparison always runs. The token comparison needs the
//! release's `tokenizer.json` (13 MB, not committed) and
//! `tokenizer_config.json`:
//!
//! ```text
//! ARC_RELEASE_DIR=/path/to/release \
//!   cargo test -p rayline-arc-input --test parity -- --ignored
//! ```
//!
//! `ARC_PROJECTION_DUMPS` (comma-separated paths) adds dump files of the same
//! shape from elsewhere, such as recorded traffic; a line with `error` is a
//! request the reference refuses, and one with `skipped` is not compared.
//! `ARC_PROJECTION_BODIES` names a file of `{"id", "body", "sources"}` lines
//! whose `body` replaces the dump's for the same id (the request text exactly
//! as sent). Failures name the case and where it differs, never the content.

use std::collections::HashMap;
use std::path::Path;

use rayline_arc_input::{ArcTokenizer, ProjectionError, project_request};
use serde_json::Value;

const PUBLIC_GOLDENS: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/arc-1.0-20261111-goldens.jsonl"
);

#[derive(Default)]
struct Outcome {
    compared: usize,
    tokenized: usize,
    refused_alike: usize,
    skipped: usize,
    failures: Vec<String>,
}

fn rows(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("a dump line is JSON"))
        .collect()
}

fn sources(value: Option<&Value>) -> Vec<Option<String>> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| item.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn first_difference<T: PartialEq>(left: &[T], right: &[T]) -> String {
    match left.iter().zip(right).position(|(a, b)| a != b) {
        Some(index) => format!("first differs at {index}"),
        None => format!("lengths {} and {}", left.len(), right.len()),
    }
}

fn compare(
    path: &Path,
    tokenizer: Option<&ArcTokenizer>,
    bodies: &HashMap<String, Value>,
) -> Outcome {
    let mut outcome = Outcome::default();
    for row in rows(path) {
        let id = row["case_id"].as_str().unwrap_or("?").to_owned();
        if row.get("skipped").is_some() {
            outcome.skipped += 1;
            continue;
        }
        let sent = bodies.get(&id);
        let (body, attribution) = match sent {
            Some(sent) => (sent["body"].as_str(), sources(sent.get("sources"))),
            None => (row["body"].as_str(), sources(row.get("sources"))),
        };
        let Some(body) = body else {
            outcome.skipped += 1;
            continue;
        };
        let projected = project_request(body, &attribution);
        if row.get("error").is_some() {
            // The reference refuses an input with no blocks when it tokenizes.
            let encoded = projected.and_then(|blocks| match tokenizer {
                Some(tokenizer) => tokenizer.encode(&blocks).map(|_| ()),
                None if blocks.is_empty() => Err(ProjectionError::Refused(String::new())),
                None => Ok(()),
            });
            match encoded {
                Err(_) => outcome.refused_alike += 1,
                Ok(()) => outcome.failures.push(format!(
                    "{id}: projected, but the reference refuses ({})",
                    row["error"].as_str().unwrap_or("?")
                )),
            }
            continue;
        }
        let blocks = match projected {
            Ok(blocks) => blocks,
            Err(error) => {
                let kind = match error {
                    ProjectionError::Json(_) => "json",
                    ProjectionError::Refused(_) => "refused",
                    ProjectionError::Unsupported(_) => "unsupported",
                    ProjectionError::Tokenizer(_) => "tokenizer",
                    ProjectionError::Internal(_) => "internal",
                };
                outcome
                    .failures
                    .push(format!("{id}: {kind} error, but the reference projects"));
                continue;
            }
        };
        outcome.compared += 1;
        if let Some(expected) = row.get("blocks").and_then(Value::as_array) {
            let expected: Vec<(&str, &str)> = expected
                .iter()
                .map(|block| {
                    (
                        block[0].as_str().unwrap_or(""),
                        block[1].as_str().unwrap_or(""),
                    )
                })
                .collect();
            let actual: Vec<(&str, &str)> = blocks
                .iter()
                .map(|block| (block.role, block.content.as_str()))
                .collect();
            if actual != expected {
                let at = actual.iter().zip(&expected).position(|(a, b)| a != b);
                let detail = match at {
                    Some(index) if actual[index].0 != expected[index].0 => {
                        format!("block {index} role")
                    }
                    Some(index) => format!(
                        "block {index} content, {}",
                        first_difference(actual[index].1.as_bytes(), expected[index].1.as_bytes())
                    ),
                    None => format!("{} blocks, expected {}", actual.len(), expected.len()),
                };
                outcome.failures.push(format!("{id}: {detail}"));
                continue;
            }
        }
        let Some(tokenizer) = tokenizer else {
            continue;
        };
        let expected_tokens: Vec<u32> = row["tokens"]
            .as_array()
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| id.as_u64().and_then(|id| u32::try_from(id).ok()))
                    .collect()
            })
            .unwrap_or_default();
        let expected_offsets: Vec<usize> = row["offsets"]
            .as_array()
            .map(|offsets| {
                offsets
                    .iter()
                    .filter_map(|offset| offset.as_u64().and_then(|at| usize::try_from(at).ok()))
                    .collect()
            })
            .unwrap_or_default();
        match tokenizer.encode(&blocks) {
            Ok(input) if input.tokens != expected_tokens => outcome.failures.push(format!(
                "{id}: tokens, {}",
                first_difference(&input.tokens, &expected_tokens)
            )),
            Ok(input) if input.readout_offsets != expected_offsets => {
                outcome.failures.push(format!(
                    "{id}: readout offsets, {}",
                    first_difference(&input.readout_offsets, &expected_offsets)
                ));
            }
            Ok(_) => outcome.tokenized += 1,
            Err(_) => outcome.failures.push(format!(
                "{id}: tokenization failed, the reference's did not"
            )),
        }
    }
    outcome
}

fn tokenizer_from_env() -> ArcTokenizer {
    let dir = std::env::var("ARC_RELEASE_DIR").expect(
        "set ARC_RELEASE_DIR to a directory with the release's tokenizer.json and tokenizer_config.json",
    );
    let read = |name: &str| {
        let path = Path::new(&dir).join(name);
        std::fs::read(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
    };
    let config = String::from_utf8(read("tokenizer_config.json")).expect("the config is UTF-8");
    ArcTokenizer::from_release(&read("tokenizer.json"), &config).expect("the ARC tokenizer loads")
}

fn report(name: &str, outcome: &Outcome) {
    println!(
        "{name}: {} projected alike, {} tokenized alike, {} refused alike, {} skipped, {} mismatched",
        outcome.compared - outcome.failures.len().min(outcome.compared),
        outcome.tokenized,
        outcome.refused_alike,
        outcome.skipped,
        outcome.failures.len()
    );
    for failure in outcome.failures.iter().take(40) {
        println!("  {failure}");
    }
}

#[test]
fn public_goldens_project_to_the_reference_blocks() {
    let outcome = compare(Path::new(PUBLIC_GOLDENS), None, &HashMap::new());
    report("public goldens", &outcome);
    assert!(outcome.failures.is_empty());
    assert_eq!((outcome.compared, outcome.skipped), (54, 0));
}

#[test]
#[ignore = "needs ARC_RELEASE_DIR, the release's tokenizer files"]
fn public_goldens_tokenize_to_the_reference_ids_and_offsets() {
    let tokenizer = tokenizer_from_env();
    let outcome = compare(Path::new(PUBLIC_GOLDENS), Some(&tokenizer), &HashMap::new());
    report("public goldens", &outcome);
    assert!(outcome.failures.is_empty());
    assert_eq!(outcome.tokenized, 54);
}

#[test]
#[ignore = "needs ARC_RELEASE_DIR and ARC_PROJECTION_DUMPS"]
fn local_dumps_match_the_reference() {
    let tokenizer = tokenizer_from_env();
    let dumps = std::env::var("ARC_PROJECTION_DUMPS")
        .expect("set ARC_PROJECTION_DUMPS to comma-separated dump files");
    let bodies: HashMap<String, Value> = std::env::var("ARC_PROJECTION_BODIES")
        .map(|path| {
            rows(Path::new(&path))
                .into_iter()
                .filter_map(|row| Some((row["id"].as_str()?.to_owned(), row)))
                .collect()
        })
        .unwrap_or_default();
    let mut failed = 0;
    for path in dumps.split(',').filter(|path| !path.is_empty()) {
        let outcome = compare(Path::new(path), Some(&tokenizer), &bodies);
        report(path, &outcome);
        failed += outcome.failures.len();
    }
    assert_eq!(failed, 0);
}
