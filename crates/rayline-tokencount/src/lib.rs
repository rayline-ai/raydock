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
/// Where Qwen2-VL-style models publish their image sizing instead.
pub const PREPROCESSOR_FILE: &str = "preprocessor_config.json";

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
    /// `transformers` renders with a `tool_use` template when the request has
    /// tools and the model publishes one.
    tool_template: bool,
    /// `bos_token`, `eos_token` and the other named special tokens, which
    /// `transformers` passes to every template.
    special_tokens: Map<String, Value>,
    images: Option<ImageSizing>,
    units: String,
}

/// A model's chat templates: the default one and, when published, the one
/// `transformers` selects for a request with tools.
pub struct ChatTemplates {
    pub default: String,
    pub tool_use: Option<String>,
}

impl PromptCounter {
    /// Loads `tokenizer.json`, the chat template (`chat_template.jinja`, else
    /// `tokenizer_config.json`'s `chat_template`) and, when present,
    /// `processor_config.json` (else `preprocessor_config.json`) from a model
    /// snapshot directory.
    pub fn from_dir(dir: &Path) -> Result<Self> {
        let tokenizer_bytes = std::fs::read(dir.join(TOKENIZER_FILE))
            .with_context(|| format!("read {}", dir.join(TOKENIZER_FILE).display()))?;
        let config_path = dir.join(TOKENIZER_CONFIG_FILE);
        let config = read_json(&config_path)?;
        if config.as_ref().is_some_and(|config| !config.is_object()) {
            bail!("{} is not a JSON object", config_path.display());
        }
        let templates = read_templates(dir, config.as_ref())?;
        let mut images = None;
        for file in [PROCESSOR_FILE, PREPROCESSOR_FILE] {
            let path = dir.join(file);
            if let Some(processor) = read_json(&path)? {
                images = ImageSizing::from_processor_config(&processor)
                    .with_context(|| format!("read {}", path.display()))?;
                break;
            }
        }
        let special_tokens = config
            .as_ref()
            .and_then(Value::as_object)
            .map(special_tokens)
            .unwrap_or_default();
        Self::build(&tokenizer_bytes, &templates, images, special_tokens)
    }

    pub fn new(tokenizer_json: &[u8], template: &str, processor: Option<&Value>) -> Result<Self> {
        let templates = ChatTemplates {
            default: template.to_owned(),
            tool_use: None,
        };
        let images = processor
            .map(ImageSizing::from_processor_config)
            .transpose()?
            .flatten();
        Self::build(tokenizer_json, &templates, images, Map::new())
    }

    pub fn build(
        tokenizer_json: &[u8],
        templates: &ChatTemplates,
        images: Option<ImageSizing>,
        special_tokens: Map<String, Value>,
    ) -> Result<Self> {
        let mut tokenizer = Tokenizer::from_bytes(tokenizer_json)
            .map_err(|error| anyhow!("load tokenizer.json: {error}"))?;
        // A saved truncation or padding would change the count itself.
        tokenizer
            .with_truncation(None)
            .map_err(|error| anyhow!("clear tokenizer truncation: {error}"))?;
        tokenizer.with_padding(None);
        let mut env = environment();
        env.add_template_owned("chat", without_generation_tags(&templates.default))
            .context("compile the chat template")?;
        if let Some(tool_use) = &templates.tool_use {
            env.add_template_owned("tool_use", without_generation_tags(tool_use))
                .context("compile the tool_use chat template")?;
        }
        for (name, label) in [
            ("chat", "the chat template"),
            ("tool_use", "the tool_use chat template"),
        ] {
            let Ok(template) = env.get_template(name) else {
                continue;
            };
            let mut missing: Vec<String> = template
                .undeclared_variables(false)
                .into_iter()
                .filter(|variable| {
                    is_special_token(variable) && !special_tokens.contains_key(variable)
                })
                .collect();
            missing.sort();
            if !missing.is_empty() {
                bail!(
                    "{label} uses `{}`, which {TOKENIZER_CONFIG_FILE} does not define",
                    missing.join("`, `")
                );
            }
        }
        let mut digest = Sha256::new();
        digest.update(Sha256::digest(tokenizer_json));
        digest.update(Sha256::digest(templates.default.as_bytes()));
        if let Some(tool_use) = &templates.tool_use {
            digest.update(Sha256::digest(tool_use.as_bytes()));
        }
        let hex: String = digest
            .finalize()
            .iter()
            .take(8)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Ok(Self {
            tokenizer,
            env,
            tool_template: templates.tool_use.is_some(),
            special_tokens,
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
        let mut context = self.special_tokens.clone();
        context.insert("messages".into(), Value::Array(messages));
        context.insert("add_generation_prompt".into(), Value::Bool(true));
        if let Some(tools) = body
            .get("tools")
            .and_then(Value::as_array)
            .filter(|tools| !tools.is_empty())
        {
            context.insert("tools".into(), Value::Array(tools.clone()));
        }
        let template = if self.tool_template && context.contains_key("tools") {
            "tool_use"
        } else {
            "chat"
        };
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
            .get_template(template)
            .and_then(|template| {
                template.render(JinjaValue::from(minijinja::value::Serde(&context)))
            })
            // Only the kind and line: a template's message can quote the prompt.
            .map_err(|error| {
                anyhow!(
                    "render the chat template: {:?} at line {:?}",
                    error.kind(),
                    error.line()
                )
            })?;
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

/// `transformers` adds `{% generation %}…{% endgeneration %}` to mark the
/// assistant's tokens for training masks; it renders its body unchanged.
/// minijinja has no such tag, so it becomes an always-true `if`, keeping the
/// tags' whitespace control.
fn without_generation_tags(template: &str) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{%") {
        out.push_str(&rest[..start]);
        let tag = &rest[start..];
        let Some(end) = tag.find("%}") else {
            out.push_str(tag);
            return out;
        };
        let inner = &tag[2..end];
        let (open, body) = inner
            .strip_prefix('-')
            .map_or(("", inner), |body| ("-", body));
        let (body, close) = body
            .strip_suffix('-')
            .map_or((body, ""), |body| (body, "-"));
        match body.trim() {
            "generation" => out.push_str(&format!("{{%{open} if true {close}%}}")),
            "endgeneration" => out.push_str(&format!("{{%{open} endif {close}%}}")),
            _ => out.push_str(&tag[..end + 2]),
        }
        rest = &tag[end + 2..];
    }
    out.push_str(rest);
    out
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

/// `chat_template.jinja` when published, else `tokenizer_config.json`'s
/// `chat_template`: a string, or named templates (`default`, `tool_use`).
fn read_templates(dir: &Path, config: Option<&Value>) -> Result<ChatTemplates> {
    let named = |name: &str| -> Option<String> {
        config?
            .get("chat_template")?
            .as_array()?
            .iter()
            .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))?
            .get("template")?
            .as_str()
            .map(str::to_owned)
    };
    match std::fs::read_to_string(dir.join(TEMPLATE_FILE)) {
        Ok(default) => {
            return Ok(ChatTemplates {
                default,
                tool_use: named("tool_use"),
            });
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("read chat_template.jinja"),
    }
    match config.and_then(|config| config.get("chat_template")) {
        Some(Value::String(default)) => Ok(ChatTemplates {
            default: default.clone(),
            tool_use: None,
        }),
        Some(Value::Array(_)) => Ok(ChatTemplates {
            default: named("default")
                .ok_or_else(|| anyhow!("tokenizer_config.json has no default chat template"))?,
            tool_use: named("tool_use"),
        }),
        _ => bail!("the model has no chat template"),
    }
}

/// A template variable `transformers` fills from the special tokens: a
/// missing one would render as nothing.
fn is_special_token(variable: &str) -> bool {
    variable.ends_with("_token") || variable == "additional_special_tokens"
}

/// The `*_token` entries of `tokenizer_config.json` (a string or an added
/// token's `content`) and `additional_special_tokens`, as `transformers`
/// passes them to the template.
fn special_tokens(config: &Map<String, Value>) -> Map<String, Value> {
    let mut tokens = Map::new();
    let content = |value: &Value| -> Option<Value> {
        match value {
            Value::String(_) => Some(value.clone()),
            Value::Object(token) => token
                .get("content")
                .filter(|content| content.is_string())
                .cloned(),
            _ => None,
        }
    };
    for (key, value) in config {
        if key.ends_with("_token")
            && let Some(token) = content(value)
        {
            tokens.insert(key.clone(), token);
        }
    }
    if let Some(additional) = config
        .get("additional_special_tokens")
        .and_then(Value::as_array)
    {
        tokens.insert(
            "additional_special_tokens".into(),
            Value::Array(additional.iter().filter_map(content).collect()),
        );
    }
    tokens
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
    let separators: Option<Vec<String>> = kwargs.get("separators")?;
    kwargs.assert_all_used()?;
    let separators = match separators.as_deref() {
        None => None,
        Some([item, key]) => Some((item.clone(), key.clone())),
        Some(_) => {
            return Err(Error::new(
                ErrorKind::InvalidOperation,
                "tojson separators must be (item, key)",
            ));
        }
    };
    let json = serde_json::to_value(&value)
        .map_err(|error| Error::new(ErrorKind::BadSerialization, error.to_string()))?;
    let options = pyjson::Options {
        ensure_ascii: ensure_ascii.unwrap_or(false),
        indent,
        sort_keys: sort_keys.unwrap_or(false),
        separators,
    };
    Ok(JinjaValue::from_safe_string(pyjson::dumps(&json, &options)))
}

#[cfg(test)]
mod tests;
