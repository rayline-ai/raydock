//! Explicit model capabilities shared by the execution gateway and launchers.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ModelCapabilities {
    pub context_window: u64,
    pub max_output_tokens: u64,
    pub input_modalities: Vec<String>,
}

impl ModelCapabilities {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.context_window == 0
            || self.max_output_tokens == 0
            || self.max_output_tokens > self.context_window
        {
            return Err("invalid execution model context/output limits");
        }
        if !self.input_modalities.iter().any(|value| value == "text")
            || self
                .input_modalities
                .iter()
                .any(|value| !matches!(value.as_str(), "text" | "image"))
            || self
                .input_modalities
                .iter()
                .enumerate()
                .any(|(index, value)| self.input_modalities[..index].contains(value))
        {
            return Err("invalid execution model input modalities");
        }
        Ok(())
    }
}
