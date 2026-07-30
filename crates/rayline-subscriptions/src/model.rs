use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ModelFamily(String);

impl ModelFamily {
    pub fn from_requested_model(model: &str) -> Self {
        Self(normalize_model_family(model))
    }

    pub fn from_display_name(display_name: &str) -> Self {
        Self(normalize_model_family(display_name))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelFamily {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

fn normalize_model_family(model: &str) -> String {
    let normalized = model.trim().to_ascii_lowercase();
    for family in ["fable", "opus", "sonnet", "haiku"] {
        if normalized
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|part| part == family)
        {
            return family.to_owned();
        }
    }

    let without_prefix = normalized.strip_prefix("claude-").unwrap_or(&normalized);
    without_prefix
        .trim_matches(|character: char| !character.is_ascii_alphanumeric())
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_known_claude_model_families() {
        assert_eq!(
            ModelFamily::from_requested_model("claude-fable-5").as_str(),
            "fable"
        );
        assert_eq!(
            ModelFamily::from_requested_model("claude-sonnet-4-5-20250929").as_str(),
            "sonnet"
        );
        assert_eq!(ModelFamily::from_display_name("Fable").as_str(), "fable");
    }

    #[test]
    fn retains_a_stable_unknown_family() {
        assert_eq!(
            ModelFamily::from_requested_model("claude-new-model").as_str(),
            "new-model"
        );
    }
}
