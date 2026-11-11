//! Counts a local model's prompt tokens the way its server does: the model's
//! published chat template renders the OpenAI Chat request, its published
//! `tokenizer.json` encodes the result, and each image is charged what the
//! model's image processor makes of it.
//!
//! The rendering follows Hugging Face `transformers` (`apply_chat_template`):
//! `trim_blocks` and `lstrip_blocks`, Python string methods, a `tojson` that
//! writes Python's `json.dumps`, and tool-call arguments passed as objects.
//! A server that adds its own tokens around the template (a closing think
//! tag, a folded system message) can count a few more; callers cover that
//! with a margin.

mod image;
mod pyjson;

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use minijinja::{Environment, Error, ErrorKind, Value as JinjaValue};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use tokenizers::Tokenizer;

pub use image::ImageSizing;

/// The files a counter reads from a model snapshot directory.
pub const TOKENIZER_FILE: &str = "tokenizer.json";
pub const TEMPLATE_FILE: &str = "chat_template.jinja";
pub const TOKENIZER_CONFIG_FILE: &str = "tokenizer_config.json";
pub const PROCESSOR_FILE: &str = "processor_config.json";

/// A prompt's size in the model's own tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub prompt_tokens: usize,
    /// Images in the request, each charged by [`ImageSizing`].
    pub images: usize,
}

pub struct PromptCounter {
    tokenizer: Tokenizer,
    env: Environment<'static>,
    images: Option<ImageSizing>,
    units: String,
}

impl PromptCounter {
    /// Loads `tokenizer.json`, the chat template (`chat_template.jinja`, else
    /// `tokenizer_config.json`'s `chat_template`) and, when present,
    /// `processor_config.json` from a model snapshot directory.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let tokenizer_bytes = std::fs::read(dir.join(TOKENIZER_FILE))
            .with_context(|| format!("read {}", dir.join(TOKENIZER_FILE).display()))?;
        let template = read_template(dir)?;
        let processor = match std::fs::read(dir.join(PROCESSOR_FILE)) {
            Ok(bytes) => Some(
                serde_json::from_slice::<Value>(&bytes).context("parse processor_config.json")?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read processor_config.json"),
        };
        Self::new(&tokenizer_bytes, &template, processor.as_ref())
    }

    pub fn new(tokenizer_json: &[u8], template: &str, processor: Option<&Value>) -> Result<Self> {
        let tokenizer = Tokenizer::from_bytes(tokenizer_json)
            .map_err(|error| anyhow!("load tokenizer.json: {error}"))?;
        let mut env = environment();
        env.add_template_owned("chat", template.to_owned())
            .context("compile the chat template")?;
        let images = processor
            .map(ImageSizing::from_processor_config)
            .transpose()?
            .flatten();
        let mut digest = Sha256::new();
        digest.update(Sha256::digest(tokenizer_json));
        digest.update(Sha256::digest(template.as_bytes()));
        let hex: String = digest
            .finalize()
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            tokenizer,
            env,
            images,
            units: format!("hf-tokenizer:{hex}"),
        })
    }

    /// Names this tokenizer and template: two counters with the same units
    /// count alike.
    pub fn units(&self) -> &str {
        &self.units
    }

    /// Whether the model takes images (its snapshot has an image processor).
    pub fn accepts_images(&self) -> bool {
        self.images.is_some()
    }

    /// Counts an OpenAI Chat Completions request body as the model will see it.
    pub fn count_chat(&self, body: &Value) -> Result<Count> {
        let messages = body
            .get("messages")
            .and_then(Value::as_array)
            .filter(|messages| !messages.is_empty())
            .ok_or_else(|| anyhow!("a Chat request needs messages"))?;
        let mut images = Vec::new();
        let messages: Vec<Value> = messages
            .iter()
            .map(|message| prepare_message(message, &mut images))
            .collect::<Result<_>>()?;
        if !images.is_empty() && self.images.is_none() {
            bail!("the request has images and the model has no image processor");
        }
        let mut context = Map::new();
        context.insert("messages".into(), Value::Array(messages));
        context.insert("add_generation_prompt".into(), Value::Bool(true));
        if let Some(tools) = body
            .get("tools")
            .and_then(Value::as_array)
            .filter(|tools| !tools.is_empty())
        {
            context.insert("tools".into(), Value::Array(tools.clone()));
        }
        if let Some(effort) = body
            .get("reasoning_effort")
            .filter(|value| value.is_string())
        {
            context.insert("reasoning_effort".into(), effort.clone());
        }
        if let Some(kwargs) = body.get("chat_template_kwargs").and_then(Value::as_object) {
            for (key, value) in kwargs {
                context.insert(key.clone(), value.clone());
            }
        }
        let rendered = self
            .env
            .get_template("chat")
            .and_then(|template| {
                template.render(JinjaValue::from(minijinja::value::Serde(&context)))
            })
            .map_err(|error| anyhow!("render the chat template: {error:#}"))?;
        let encoding = self
            .tokenizer
            .encode(rendered, false)
            .map_err(|error| anyhow!("tokenize: {error}"))?;
        let mut tokens = encoding.len();
        if let Some(sizing) = &self.images {
            for image in &images {
                // The template writes one placeholder token per image; the
                // server expands it to the image's patch tokens.
                tokens = tokens - 1 + sizing.tokens(*image);
            }
        }
        Ok(Count {
            prompt_tokens: tokens,
            images: images.len(),
        })
    }
}

fn read_template(dir: &Path) -> Result<String> {
    match std::fs::read_to_string(dir.join(TEMPLATE_FILE)) {
        Ok(template) => return Ok(template),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read chat_template.jinja"),
    }
    let config: Value = serde_json::from_slice(
        &std::fs::read(dir.join(TOKENIZER_CONFIG_FILE))
            .context("the model has no chat template")?,
    )
    .context("parse tokenizer_config.json")?;
    match config.get("chat_template") {
        Some(Value::String(template)) => Ok(template.clone()),
        // A list of named templates: the default one renders chat.
        Some(Value::Array(named)) => named
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some("default"))
            .and_then(|entry| entry.get("template")?.as_str())
            .map(str::to_owned)
            .ok_or_else(|| anyhow!("tokenizer_config.json has no default chat template")),
        _ => bail!("the model has no chat template"),
    }
}

/// `transformers` renders tool-call arguments as objects: the template
/// iterates them. Image parts are recorded for sizing and kept for the
/// template's placeholder.
fn prepare_message(message: &Value, images: &mut Vec<image::Source>) -> Result<Value> {
    let mut message = message.clone();
    if let Some(calls) = message.get_mut("tool_calls").and_then(Value::as_array_mut) {
        for call in calls {
            if let Some(arguments) = call.pointer_mut("/function/arguments")
                && let Some(text) = arguments.as_str()
                && let Ok(parsed @ Value::Object(_)) = serde_json::from_str::<Value>(text)
            {
                *arguments = parsed;
            }
        }
    }
    if let Some(parts) = message.get("content").and_then(Value::as_array) {
        for part in parts {
            match part.get("type").and_then(Value::as_str) {
                Some("image_url" | "image" | "input_image") => {
                    images.push(image::Source::from_part(part))
                }
                Some("video" | "video_url" | "input_audio" | "audio" | "audio_url") => {
                    bail!("the counter does not size video or audio")
                }
                _ => {}
            }
        }
    }
    Ok(message)
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_syntax(
        minijinja::syntax::SyntaxConfig::builder()
            .trim_blocks(true)
            .lstrip_blocks(true)
            .build()
            .expect("the default delimiters with trimming are a valid syntax"),
    );
    env.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    env.add_filter("tojson", tojson);
    env.add_function(
        "raise_exception",
        |message: String| -> Result<JinjaValue, Error> {
            Err(Error::new(ErrorKind::InvalidOperation, message))
        },
    );
    // Templates print today's date; its width is what matters, not the day.
    env.add_function("strftime_now", |_format: String| -> String {
        "26 Jul 2026".to_owned()
    });
    env
}

/// `transformers`' `tojson`: Python's `json.dumps` with `ensure_ascii=False`
/// unless asked, `", "`/`": "` separators unless indented.
fn tojson(value: JinjaValue, kwargs: minijinja::value::Kwargs) -> Result<JinjaValue, Error> {
    let ensure_ascii: Option<bool> = kwargs.get("ensure_ascii")?;
    let indent: Option<usize> = kwargs.get("indent")?;
    let sort_keys: Option<bool> = kwargs.get("sort_keys")?;
    let _: Option<JinjaValue> = kwargs.get("separators")?;
    kwargs.assert_all_used()?;
    let json = serde_json::to_value(&value)
        .map_err(|error| Error::new(ErrorKind::BadSerialization, error.to_string()))?;
    let options = pyjson::Options {
        ensure_ascii: ensure_ascii.unwrap_or(false),
        indent,
        sort_keys: sort_keys.unwrap_or(false),
    };
    Ok(JinjaValue::from_safe_string(pyjson::dumps(&json, &options)))
}

#[cfg(test)]
mod tests;
