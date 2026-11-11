//! Fetches the local model's published tokenizer and chat template from
//! Hugging Face for rld's prompt counter (`rld hybrid --token-counter-dir`).
//!
//! The core counts Qwen3.8-27B itself. For another declared model it can
//! only bound a prompt by its bytes, several times the real count, and an
//! image in the history makes that bound unknown; with the model's own files
//! rld counts exactly. Without them (offline, or a model with no known repo)
//! the session starts as before.

use std::path::PathBuf;

use serde_json::Value;

/// Hugging Face repos publishing the tokenizer and chat template of a
/// registry model the core has no counter for.
const KNOWN_REPOS: &[(&str, &str)] = &[("z-ai/glm-5.3-flash", "zai-org/GLM-5.3-Flash")];

const REQUIRED: &[&str] = &[rayline_tokencount::TOKENIZER_FILE];
/// A template ships as its own file or inside `tokenizer_config.json`; a
/// processor config describes image input.
const OPTIONAL: &[&str] = &[
    rayline_tokencount::TEMPLATE_FILE,
    rayline_tokencount::TOKENIZER_CONFIG_FILE,
    rayline_tokencount::PROCESSOR_FILE,
    rayline_tokencount::PREPROCESSOR_FILE,
];

/// The repo to count `registry_model` with: `hybrid.local_tokenizer` (a repo
/// id, or `false` to turn counting off), else a known repo. `None` for the
/// core's own model and for an unknown one.
pub(crate) fn tokenizer_repo(
    registry_model: &str,
    settings: Option<&Value>,
) -> Result<Option<String>, String> {
    let setting = settings
        .and_then(|settings| settings.get("hybrid"))
        .and_then(|hybrid| hybrid.get("local_tokenizer"));
    match setting {
        Some(Value::Bool(false)) => return Ok(None),
        Some(Value::String(repo)) if is_repo_id(repo) => return Ok(Some(repo.clone())),
        Some(_) => {
            return Err("settings hybrid.local_tokenizer must be a Hugging Face repo id \"owner/name\" publishing the local model's tokenizer.json, or false".to_owned());
        }
        None => {}
    }
    if registry_model == crate::hybrid_profile::LOCAL_REGISTRY_MODEL {
        return Ok(None);
    }
    Ok(KNOWN_REPOS
        .iter()
        .find(|(model, _)| *model == registry_model)
        .map(|(_, repo)| (*repo).to_owned()))
}

fn is_repo_id(value: &str) -> bool {
    let mut parts = value.split('/');
    let valid = |part: Option<&str>| {
        part.is_some_and(|part| {
            !part.is_empty()
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        })
    };
    valid(parts.next()) && valid(parts.next()) && parts.next().is_none()
}

/// Downloads the counter's files at the repo's current commit into the
/// Hugging Face cache, or reuses the cached snapshot when the Hub is
/// unreachable. Returns the snapshot directory.
pub(crate) fn fetch(repo: &str) -> Result<PathBuf, String> {
    let commit = match rayline_hf::hf_api_get_commit(repo) {
        Ok(commit) => commit,
        Err(error) => cached_commit(repo).ok_or(error)?,
    };
    for (file, required) in REQUIRED
        .iter()
        .map(|file| (file, true))
        .chain(OPTIONAL.iter().map(|file| (file, false)))
    {
        if rayline_hf::verified_hf_cache_file(repo, file, &commit, None)?.is_some() {
            continue;
        }
        if let Err(error) = rayline_hf::download_to_hf_cache(
            repo,
            file,
            &commit,
            None,
            None,
            "tokenizer",
            None,
            0,
            0,
            None,
        ) && required
        {
            return Err(format!("download {file} from {repo}: {error}"));
        }
    }
    let dir = rayline_hf::hf_cache_snapshot_path(repo, "", &commit);
    let has_template = [
        rayline_tokencount::TEMPLATE_FILE,
        rayline_tokencount::TOKENIZER_CONFIG_FILE,
    ]
    .iter()
    .any(|file| dir.join(file).is_file());
    if !has_template {
        return Err(format!("{repo} publishes no chat template"));
    }
    Ok(dir)
}

/// The cached snapshot, without the network: what `start` uses when the
/// Hub does not answer in time.
pub(crate) fn cached(repo: &str) -> Option<PathBuf> {
    let dir = rayline_hf::hf_cache_snapshot_path(repo, "", &cached_commit(repo)?);
    let complete = dir.join(rayline_tokencount::TOKENIZER_FILE).is_file()
        && [
            rayline_tokencount::TEMPLATE_FILE,
            rayline_tokencount::TOKENIZER_CONFIG_FILE,
        ]
        .iter()
        .any(|file| dir.join(file).is_file());
    complete.then_some(dir)
}

/// The commit `refs/main` names in the Hugging Face cache.
fn cached_commit(repo: &str) -> Option<String> {
    let refs = rayline_hf::hf_cache_dir()
        .join(rayline_hf::repo_to_folder_name(repo))
        .join("refs")
        .join("main");
    let commit = std::fs::read_to_string(refs).ok()?.trim().to_owned();
    (!commit.is_empty()).then_some(commit)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_models_map_to_their_published_repo() {
        assert_eq!(
            tokenizer_repo("z-ai/glm-5.3-flash", None)
                .unwrap()
                .as_deref(),
            Some("zai-org/GLM-5.3-Flash")
        );
        assert_eq!(
            tokenizer_repo(crate::hybrid_profile::LOCAL_REGISTRY_MODEL, None).unwrap(),
            None
        );
        assert_eq!(tokenizer_repo("acme/unknown-1", None).unwrap(), None);
    }

    #[test]
    fn the_setting_names_a_repo_or_turns_counting_off() {
        let named = json!({"hybrid": {"local_tokenizer": "acme/Model-1.5"}});
        assert_eq!(
            tokenizer_repo("acme/unknown-1", Some(&named))
                .unwrap()
                .as_deref(),
            Some("acme/Model-1.5")
        );
        let off = json!({"hybrid": {"local_tokenizer": false}});
        assert_eq!(
            tokenizer_repo("z-ai/glm-5.3-flash", Some(&off)).unwrap(),
            None
        );
        for bad in [json!("no-owner"), json!("a/b/c"), json!(true), json!(3)] {
            let settings = json!({"hybrid": {"local_tokenizer": bad}});
            assert!(tokenizer_repo("z-ai/glm-5.3-flash", Some(&settings)).is_err());
        }
    }
}
