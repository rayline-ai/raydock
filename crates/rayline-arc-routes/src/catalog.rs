//! The serving package's action catalog, as the routes contract reads it.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::digest::sha256_hex;

/// The policy's model name and the public catalog id the hosted router names
/// it by. A model the caller priced is matched to the caller's own key first;
/// a model in neither keeps its bare name.
const PUBLIC_IDS: &[(&str, &str)] = &[
    ("claude-haiku-5.5", "anthropic/claude-haiku-5.5"),
    ("claude-opus-5.5", "anthropic/claude-opus-5.5"),
    ("claude-sonnet-5.5", "anthropic/claude-sonnet-5.5"),
    ("deepseek-v4.1-flash", "deepseek/deepseek-v4.1-flash-1017"),
    ("kimi-k3", "moonshotai/kimi-k3"),
    ("gpt-5.6-terra", "openai/gpt-5.6-terra"),
    ("gpt-6-astra", "openai/gpt-6-astra"),
    ("gpt-6-luna", "openai/gpt-6-luna"),
    ("gpt-6.1-sol", "openai/gpt-6.1-sol"),
    ("qwen3.8-27b", "qwen/qwen3.8-27b"),
    ("glm-5.3", "z-ai/glm-5.3"),
    ("glm-5.3-flash", "z-ai/glm-5.3-flash"),
];

/// One action of the package: a model under one thinking control.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Action {
    pub model: String,
    #[serde(default)]
    pub control_id: Option<String>,
    /// The thinking level the control names, when it names one.
    #[serde(default)]
    pub level: Option<String>,
}

/// What the contract needs of a serving package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Catalog {
    /// The name the decide request addresses the package by.
    pub alias: String,
    /// sha256 of the package manifest's bytes.
    pub package_sha256: String,
    pub thinking_controls_sha256: Option<String>,
    pub actions: BTreeMap<String, Action>,
    /// Policy model name to public model id.
    pub public_ids: BTreeMap<String, String>,
    /// `decision.fallback_action_id`: what a route serves when the decider
    /// is unavailable and the conversation holds no model.
    pub fallback_action_id: Option<String>,
}

#[derive(Debug)]
pub struct CatalogError(String);

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid package manifest: {}", self.0)
    }
}

impl std::error::Error for CatalogError {}

#[derive(Deserialize)]
struct Manifest {
    #[serde(default)]
    thinking_controls_sha256: Option<String>,
    actions: Vec<ManifestAction>,
    #[serde(default)]
    decision: Option<ManifestDecision>,
}

#[derive(Deserialize)]
struct ManifestDecision {
    #[serde(default)]
    fallback_action_id: Option<String>,
}

#[derive(Deserialize)]
struct ManifestAction {
    action_id: String,
    #[serde(flatten)]
    action: Action,
}

impl Catalog {
    /// Reads a package manifest (`package.json`) exactly as stored: its digest
    /// is over these bytes.
    pub fn from_manifest(raw: &[u8], alias: &str) -> Result<Self, CatalogError> {
        let manifest: Manifest =
            serde_json::from_slice(raw).map_err(|error| CatalogError(error.to_string()))?;
        Ok(Self {
            alias: alias.to_owned(),
            package_sha256: sha256_hex(raw),
            thinking_controls_sha256: manifest.thinking_controls_sha256,
            actions: manifest
                .actions
                .into_iter()
                .map(|entry| (entry.action_id, entry.action))
                .collect(),
            public_ids: PUBLIC_IDS
                .iter()
                .map(|(name, id)| ((*name).to_owned(), (*id).to_owned()))
                .collect(),
            fallback_action_id: manifest
                .decision
                .and_then(|decision| decision.fallback_action_id),
        })
    }

    /// The id an answer names a policy model by when no caller price key
    /// priced it: its public id, else the bare name.
    pub fn public_id(&self, model: &str) -> String {
        self.public_ids
            .get(model)
            .map_or_else(|| model.to_owned(), String::clone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_actions_and_hashes_the_manifest_bytes() {
        let raw = br#"{"schema_version": "x", "thinking_controls_sha256": "t",
            "actions": [{"action_id": "a1", "model": "glm-5.3", "control_id": "c1", "level": null, "control": {}},
                        {"action_id": "a2", "model": "novel-model", "level": "up"}]}"#;
        let catalog = Catalog::from_manifest(raw, "local/pkg").unwrap();
        assert_eq!(catalog.package_sha256, sha256_hex(raw));
        assert_eq!(catalog.thinking_controls_sha256.as_deref(), Some("t"));
        assert_eq!(catalog.actions["a1"].control_id.as_deref(), Some("c1"));
        assert_eq!(catalog.actions["a1"].level, None);
        assert_eq!(catalog.actions["a2"].level.as_deref(), Some("up"));
        assert!(Catalog::from_manifest(b"{}", "x").is_err());
    }

    #[test]
    fn a_model_is_named_by_its_public_id_else_its_name() {
        let catalog = Catalog::from_manifest(br#"{"actions": []}"#, "p").unwrap();
        assert_eq!(catalog.public_id("kimi-k3"), "moonshotai/kimi-k3");
        assert_eq!(catalog.public_id("unlisted"), "unlisted");
    }
}
