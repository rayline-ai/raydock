//! What an image costs a vision model: the Qwen2-VL-style `smart_resize`
//! that GLM and Qwen image processors share. The image is scaled to a
//! multiple of `patch_size * merge_size` within the processor's token limits;
//! each merged patch is one token.

use anyhow::{Result, bail};
use base64::Engine;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSizing {
    factor: u64,
    min_tokens: u64,
    max_tokens: u64,
}

/// An image part's pixel size when the request carries its bytes; an image
/// only referenced by URL is charged the processor's maximum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Pixels { width: u64, height: u64 },
    Unknown,
}

impl Source {
    pub(crate) fn from_part(part: &Value) -> Self {
        let url = part
            .pointer("/image_url/url")
            .or_else(|| part.get("image_url"))
            .or_else(|| part.pointer("/source/data"))
            .and_then(Value::as_str);
        let Some(url) = url else { return Self::Unknown };
        let data = url
            .strip_prefix("data:")
            .and_then(|rest| rest.split_once(";base64,"))
            .map_or(url, |(_, data)| data);
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(data.trim()) else {
            return Self::Unknown;
        };
        match imagesize::blob_size(&bytes) {
            Ok(size) if size.width > 0 && size.height > 0 => Self::Pixels {
                width: size.width as u64,
                height: size.height as u64,
            },
            _ => Self::Unknown,
        }
    }
}

impl ImageSizing {
    /// Reads `image_processor` (or the top level) of `processor_config.json`.
    /// A config with no patch size describes no image input: `None`.
    pub fn from_processor_config(config: &Value) -> Result<Option<Self>> {
        let processor = config.get("image_processor").unwrap_or(config);
        let Some(patch) = processor.get("patch_size").and_then(Value::as_u64) else {
            return Ok(None);
        };
        let merge = processor
            .get("merge_size")
            .and_then(Value::as_u64)
            .unwrap_or(1);
        let factor = patch * merge;
        let area = factor * factor;
        let tokens = |tokens_key: &str, pixels_key: &str| {
            processor
                .get(tokens_key)
                .and_then(Value::as_u64)
                .or_else(|| {
                    processor
                        .get(pixels_key)
                        .and_then(Value::as_u64)
                        .map(|pixels| pixels / area)
                })
                .or_else(|| {
                    processor
                        .pointer(&format!("/size/{pixels_key}"))
                        .and_then(Value::as_u64)
                        .map(|pixels| pixels / area)
                })
        };
        let (Some(min_tokens), Some(max_tokens)) = (
            tokens("min_image_tokens", "min_pixels"),
            tokens("max_image_tokens", "max_pixels"),
        ) else {
            bail!("processor_config.json has a patch size but no image token limits");
        };
        if factor == 0 || min_tokens == 0 || max_tokens < min_tokens {
            bail!("processor_config.json has inconsistent image limits");
        }
        Ok(Some(Self {
            factor,
            min_tokens,
            max_tokens,
        }))
    }

    pub(crate) fn tokens(&self, source: Source) -> usize {
        let Source::Pixels { width, height } = source else {
            return self.max_tokens as usize;
        };
        let (height, width) = self.resize(height, width);
        ((height / self.factor) * (width / self.factor)) as usize
    }

    fn resize(&self, height: u64, width: u64) -> (u64, u64) {
        let factor = self.factor as f64;
        let (h, w) = (height as f64, width as f64);
        let min_pixels = (self.min_tokens * self.factor * self.factor) as f64;
        let max_pixels = (self.max_tokens * self.factor * self.factor) as f64;
        let mut h_bar = (round_half_even(h / factor) * factor).max(factor);
        let mut w_bar = (round_half_even(w / factor) * factor).max(factor);
        if h_bar * w_bar > max_pixels {
            let beta = (h * w / max_pixels).sqrt();
            h_bar = ((h / beta / factor).floor() * factor).max(factor);
            w_bar = ((w / beta / factor).floor() * factor).max(factor);
        } else if h_bar * w_bar < min_pixels {
            let beta = (min_pixels / (h * w)).sqrt();
            h_bar = (h * beta / factor).ceil() * factor;
            w_bar = (w * beta / factor).ceil() * factor;
        }
        (h_bar as u64, w_bar as u64)
    }
}

/// Python's `round`.
fn round_half_even(value: f64) -> f64 {
    let rounded = value.round();
    if (value - value.trunc()).abs() == 0.5 && rounded % 2.0 != 0.0 {
        rounded - value.signum()
    } else {
        rounded
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn glm() -> ImageSizing {
        let config = json!({"image_processor": {"patch_size": 14, "merge_size": 2, "min_image_tokens": 16, "max_image_tokens": 8000}});
        ImageSizing::from_processor_config(&config)
            .unwrap()
            .unwrap()
    }

    #[test]
    fn charges_what_the_glm_server_charged() {
        // GLM-5.3-Flash on TensorFold: a 2172x724 PNG added 2027 tokens to a
        // prompt beside its one placeholder, so the image is 2028 tokens.
        assert_eq!(
            glm().tokens(Source::Pixels {
                width: 2172,
                height: 724
            }),
            2028
        );
    }

    #[test]
    fn stays_within_the_processors_limits() {
        let sizing = glm();
        assert_eq!(
            sizing.tokens(Source::Pixels {
                width: 1,
                height: 1
            }),
            16
        );
        assert!(
            sizing.tokens(Source::Pixels {
                width: 6016,
                height: 3384
            }) <= 8000
        );
        assert_eq!(sizing.tokens(Source::Unknown), 8000);
    }

    #[test]
    fn rounds_like_python() {
        assert_eq!(round_half_even(2.5), 2.0);
        assert_eq!(round_half_even(3.5), 4.0);
        assert_eq!(round_half_even(77.57), 78.0);
    }

    #[test]
    fn a_processor_without_patches_takes_no_images() {
        assert_eq!(
            ImageSizing::from_processor_config(&json!({"feature_size": 80})).unwrap(),
            None
        );
    }
}
