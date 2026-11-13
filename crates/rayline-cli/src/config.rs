//! `settings.json`: the typed config, its one load, env overrides and load
//! errors (docs/spec.md section 5).
//!
//! [`load`] reads `~/.config/rayline/settings.json`, or the file passed with
//! `--config`, applies the `RAYLINE_*` env overrides and validates the result
//! once. A [`Settings`] value is valid by construction: code that holds one
//! trusts it. [`check_local_start`] runs the checks the local process makes
//! before it serves anything.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use url::Url;

/// A wire protocol an endpoint serves and a harness speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    AnthropicMessages,
    OpenaiChat,
    OpenaiResponses,
}

impl Protocol {
    pub const ALL: [Protocol; 3] = [
        Protocol::AnthropicMessages,
        Protocol::OpenaiChat,
        Protocol::OpenaiResponses,
    ];

    /// The name used in `settings.json`.
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::AnthropicMessages => "anthropic_messages",
            Protocol::OpenaiChat => "openai_chat",
            Protocol::OpenaiResponses => "openai_responses",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|protocol| protocol.as_str() == value)
    }
}

/// `endpoints.<id>`: one of the user's own inference servers.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub base_url: Url,
    /// Non-empty, without repeats.
    pub protocols: Vec<Protocol>,
    /// Name of the env var holding the server's key.
    pub api_key_env: Option<String>,
}

/// `models.<id>.price`, overriding ARC's price for that model.
#[derive(Debug, Clone, PartialEq)]
pub struct Price {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

/// `models.<id>`: a model ARC can pick that one of `endpoints` serves.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// A key of [`Settings::endpoints`].
    pub endpoint: String,
    /// The name the endpoint knows the model by.
    pub model: String,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// Hugging Face tokenizer repo.
    pub tokenizer: Option<String>,
    pub price: Option<Price>,
}

/// `routing.policy`.
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// ARC's preset id, sent to ARC unchanged.
    pub preset: String,
    /// ARC's `routing.policy.overrides`, sent to ARC unchanged and not
    /// validated here.
    pub overrides: Option<Map<String, Value>>,
}

/// `routing`: present exactly when the config is for local mode.
#[derive(Debug, Clone, PartialEq)]
pub struct Routing {
    pub decisions_model: String,
    pub classifier_model: String,
    pub policy: Policy,
}

/// `environments.<name>`: an internal environment selected with `--env`.
#[derive(Debug, Clone, PartialEq)]
pub struct Environment {
    pub base_url: Url,
    pub login_url: Url,
}

/// `update.channel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateChannel {
    Prod,
    Dev,
    Main,
}

impl UpdateChannel {
    pub const ALL: [UpdateChannel; 3] =
        [UpdateChannel::Prod, UpdateChannel::Dev, UpdateChannel::Main];

    /// The name used in `settings.json` and `RAYLINE_UPDATE_CHANNEL`.
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateChannel::Prod => "prod",
            UpdateChannel::Dev => "dev",
            UpdateChannel::Main => "main",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.as_str() == value)
    }
}

/// `update`.
#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub channel: UpdateChannel,
}

/// The loaded and validated config.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    /// The file the config was read from. It may not exist: a missing default
    /// file is an empty config.
    pub file: PathBuf,
    pub endpoints: BTreeMap<String, Endpoint>,
    pub models: BTreeMap<String, Model>,
    pub routing: Option<Routing>,
    pub environments: BTreeMap<String, Environment>,
    pub update: Update,
    /// The run is in local mode. `routing` is then present.
    pub local: bool,
    /// The `--env` name, always a key of `environments`. `None` is prod.
    pub env: Option<String>,
}

/// Which file a run reads and how its mode is chosen (section 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode<'a> {
    /// `rayline run` with neither flag: the default file, cloud mode.
    Cloud,
    /// `rayline run --local` and `rayline local start`: the default file,
    /// local mode.
    Local,
    /// `rayline local start --config <file>`: `<file>`, local mode.
    LocalFile(&'a Path),
    /// `rayline run --config <file>`: `<file>`, in local mode when it has
    /// `routing` and cloud mode otherwise.
    File(&'a Path),
}

/// What [`load`] reads and what it must refuse.
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions<'a> {
    pub mode: Mode<'a>,
    /// `--env <name>`. `None` is prod.
    pub env: Option<&'a str>,
    /// The protocol of the harness being run. `None` when no harness is.
    pub harness_protocol: Option<Protocol>,
}

/// A config that Rayline refuses to run with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    /// The home directory, which holds the default file, was not found.
    NoHome,
    /// The file could not be read.
    Read { file: PathBuf, error: String },
    /// The file is not a JSON object.
    Parse { file: PathBuf, error: String },
    /// A key in the file is wrong.
    Key {
        file: PathBuf,
        key: String,
        problem: String,
    },
    /// An env override is wrong.
    Env {
        var: &'static str,
        key: &'static str,
        problem: String,
    },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::NoHome => write!(f, "home directory not found"),
            ConfigError::Read { file, error } => write!(f, "{}: {error}", file.display()),
            ConfigError::Parse { file, error } => {
                write!(f, "{}: {error}", file.display())
            }
            ConfigError::Key { file, key, problem } => {
                write!(f, "{}: `{key}` {problem}", file.display())
            }
            ConfigError::Env { var, key, problem } => {
                write!(f, "`{var}` (overrides `{key}`) {problem}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

/// The env overrides of section 5.7: (env var, key).
const ROUTING_DECISIONS_MODEL: (&str, &str) =
    ("RAYLINE_ROUTING_DECISIONS_MODEL", "routing.decisions_model");
const ROUTING_CLASSIFIER_MODEL: (&str, &str) = (
    "RAYLINE_ROUTING_CLASSIFIER_MODEL",
    "routing.classifier_model",
);
const ROUTING_POLICY_PRESET: (&str, &str) =
    ("RAYLINE_ROUTING_POLICY_PRESET", "routing.policy.preset");
const UPDATE_CHANNEL: (&str, &str) = ("RAYLINE_UPDATE_CHANNEL", "update.channel");

/// Loads the config. Precedence, lowest to highest: built-in defaults, the
/// file, env overrides.
pub fn load(options: &LoadOptions<'_>) -> Result<Settings, ConfigError> {
    let home = dirs::home_dir().ok_or(ConfigError::NoHome)?;
    load_from(&home, options, &|name| std::env::var_os(name))
}

/// Refuses what the local process must refuse before it serves anything:
/// an `api_key_env` naming an env var that isn't set, and a model ARC doesn't
/// know (`arc_knows` returns false for its id) without `context_window` or
/// `max_output_tokens`.
pub fn check_local_start(
    settings: &Settings,
    arc_knows: impl Fn(&str) -> bool,
) -> Result<(), ConfigError> {
    check_local_start_with(settings, &|name| std::env::var_os(name), &arc_knows)
}

type EnvLookup<'a> = dyn Fn(&str) -> Option<OsString> + 'a;

fn settings_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("settings.json")
}

fn load_from(
    home: &Path,
    options: &LoadOptions<'_>,
    env: &EnvLookup<'_>,
) -> Result<Settings, ConfigError> {
    let (file, default_file) = match options.mode {
        Mode::Cloud | Mode::Local => (settings_file(home), true),
        Mode::LocalFile(path) | Mode::File(path) => (path.to_path_buf(), false),
    };
    let text = match std::fs::read_to_string(&file) {
        Ok(text) => Some(text),
        Err(error) if error.kind() == io::ErrorKind::NotFound && default_file => None,
        Err(error) => {
            return Err(ConfigError::Read {
                file,
                error: error.to_string(),
            });
        }
    };
    let root = match text {
        Some(text) => parse_json(&text).map_err(|failure| match failure {
            JsonFailure::Syntax(error) => ConfigError::Parse {
                file: file.clone(),
                error: format!("invalid JSON: {error}"),
            },
            JsonFailure::DuplicateKey(key) => ConfigError::Key {
                file: file.clone(),
                key,
                problem: "appears more than once".to_owned(),
            },
        })?,
        None => Value::Object(Map::new()),
    };

    let doc = Doc { file: &file };
    let raw = doc.root(&root)?;
    let routing = apply_routing_overrides(raw.routing, env)?;
    let update = apply_update_override(raw.update, env)?;
    let routing = doc.finish_routing(routing)?;

    let local = match options.mode {
        Mode::Cloud => false,
        Mode::Local | Mode::LocalFile(_) => true,
        Mode::File(_) => routing.is_some(),
    };
    if local && routing.is_none() {
        return Err(doc.err("routing", "is required in local mode"));
    }
    for (id, model) in &raw.models {
        if !raw.endpoints.contains_key(&model.endpoint) {
            return Err(doc.err(
                format!("{}.endpoint", child("models", id)),
                format!("names `{}`, which is not in `endpoints`", model.endpoint),
            ));
        }
    }
    if let Some(protocol) = options.harness_protocol.filter(|_| local) {
        for (id, endpoint) in &raw.endpoints {
            if !endpoint.protocols.contains(&protocol) {
                return Err(doc.err(
                    format!("{}.protocols", child("endpoints", id)),
                    format!(
                        "does not list `{}`, the protocol of the harness being run",
                        protocol.as_str()
                    ),
                ));
            }
        }
    }
    if let Some(name) = options.env
        && !raw.environments.contains_key(name)
    {
        return Err(doc.err(
            child("environments", name),
            format!("is not defined, and `--env {name}` selects it"),
        ));
    }

    Ok(Settings {
        file,
        endpoints: raw.endpoints,
        models: raw.models,
        routing,
        environments: raw.environments,
        update,
        local,
        env: options.env.map(str::to_owned),
    })
}

fn check_local_start_with(
    settings: &Settings,
    env: &EnvLookup<'_>,
    arc_knows: &dyn Fn(&str) -> bool,
) -> Result<(), ConfigError> {
    let doc = Doc {
        file: &settings.file,
    };
    for (id, endpoint) in &settings.endpoints {
        if let Some(var) = &endpoint.api_key_env
            && env(var).is_none()
        {
            return Err(doc.err(
                format!("{}.api_key_env", child("endpoints", id)),
                format!("names `{var}`, which is not set"),
            ));
        }
    }
    for (id, model) in &settings.models {
        if arc_knows(id) {
            continue;
        }
        for (key, value) in [
            ("context_window", model.context_window),
            ("max_output_tokens", model.max_output_tokens),
        ] {
            if value.is_none() {
                return Err(doc.err(
                    format!("{}.{key}", child("models", id)),
                    "is required: ARC doesn't know this model",
                ));
            }
        }
    }
    Ok(())
}

/// The file's sections after their own checks, before env overrides and the
/// checks across sections.
struct RawSettings {
    endpoints: BTreeMap<String, Endpoint>,
    models: BTreeMap<String, Model>,
    routing: Option<RawRouting>,
    environments: BTreeMap<String, Environment>,
    update: Option<UpdateChannel>,
}

/// `routing` as written: its required keys may still come from env overrides.
#[derive(Default)]
struct RawRouting {
    decisions_model: Option<String>,
    classifier_model: Option<String>,
    preset: Option<String>,
    overrides: Option<Map<String, Value>>,
}

fn apply_routing_overrides(
    routing: Option<RawRouting>,
    env: &EnvLookup<'_>,
) -> Result<Option<RawRouting>, ConfigError> {
    let decisions_model = env_string(env, ROUTING_DECISIONS_MODEL)?;
    let classifier_model = env_string(env, ROUTING_CLASSIFIER_MODEL)?;
    let preset = env_string(env, ROUTING_POLICY_PRESET)?;
    let Some(mut routing) = routing else {
        let set = [
            (ROUTING_DECISIONS_MODEL, &decisions_model),
            (ROUTING_CLASSIFIER_MODEL, &classifier_model),
            (ROUTING_POLICY_PRESET, &preset),
        ]
        .into_iter()
        .find(|(_, value)| value.is_some());
        return match set {
            Some(((var, key), _)) => Err(ConfigError::Env {
                var,
                key,
                problem: "is set, but the config file has no `routing`".to_owned(),
            }),
            None => Ok(None),
        };
    };
    if let Some(value) = decisions_model {
        routing.decisions_model = Some(value);
    }
    if let Some(value) = classifier_model {
        routing.classifier_model = Some(value);
    }
    if let Some(value) = preset {
        routing.preset = Some(value);
    }
    Ok(Some(routing))
}

fn apply_update_override(
    channel: Option<UpdateChannel>,
    env: &EnvLookup<'_>,
) -> Result<Update, ConfigError> {
    let (var, key) = UPDATE_CHANNEL;
    let channel = match env_string(env, UPDATE_CHANNEL)? {
        Some(value) => UpdateChannel::parse(&value).ok_or_else(|| ConfigError::Env {
            var,
            key,
            problem: one_of(UpdateChannel::ALL.map(UpdateChannel::as_str)),
        })?,
        None => channel.unwrap_or(UpdateChannel::Prod),
    };
    Ok(Update { channel })
}

/// An override's value, validated like a non-empty string in the file.
fn env_string(
    env: &EnvLookup<'_>,
    (var, key): (&'static str, &'static str),
) -> Result<Option<String>, ConfigError> {
    let Some(value) = env(var) else {
        return Ok(None);
    };
    let problem = match value.into_string() {
        Ok(value) if !value.is_empty() => return Ok(Some(value)),
        Ok(_) => "must not be empty",
        Err(_) => "must be valid UTF-8",
    };
    Err(ConfigError::Env {
        var,
        key,
        problem: problem.to_owned(),
    })
}

fn one_of<const N: usize>(names: [&str; N]) -> String {
    let names = names.map(|name| format!("`{name}`"));
    format!("must be one of {}", names.join(", "))
}

/// Parses one entry of a section keyed by id: (entry, its key).
type ParseEntry<'a, T> = fn(&Doc<'a>, &Map<String, Value>, &str) -> Result<T, ConfigError>;

/// Parses and checks the file, naming it in every error.
struct Doc<'a> {
    file: &'a Path,
}

impl<'a> Doc<'a> {
    fn err(&self, key: impl Into<String>, problem: impl Into<String>) -> ConfigError {
        ConfigError::Key {
            file: self.file.to_path_buf(),
            key: key.into(),
            problem: problem.into(),
        }
    }

    fn root(&self, root: &Value) -> Result<RawSettings, ConfigError> {
        let Value::Object(root) = root else {
            return Err(ConfigError::Parse {
                file: self.file.to_path_buf(),
                error: "the top level must be an object".to_owned(),
            });
        };
        self.known_keys(
            root,
            "",
            &["endpoints", "models", "routing", "environments", "update"],
        )?;
        Ok(RawSettings {
            endpoints: self.entries(root, "endpoints", Self::endpoint)?,
            models: self.entries(root, "models", Self::model)?,
            routing: root
                .get("routing")
                .map(|value| self.routing(value))
                .transpose()?,
            environments: self.entries(root, "environments", Self::environment)?,
            update: root
                .get("update")
                .map(|value| self.update(value))
                .transpose()?
                .flatten(),
        })
    }

    /// A section keyed by id. A missing section is empty.
    fn entries<T>(
        &self,
        root: &Map<String, Value>,
        section: &str,
        parse: ParseEntry<'a, T>,
    ) -> Result<BTreeMap<String, T>, ConfigError> {
        let Some(value) = root.get(section) else {
            return Ok(BTreeMap::new());
        };
        let entries = self.object(value, section)?;
        let mut parsed = BTreeMap::new();
        for (id, value) in entries {
            let key = child(section, id);
            if id.is_empty() {
                return Err(self.err(key, "has an empty id"));
            }
            let entry = self.object(value, &key)?;
            parsed.insert(id.clone(), parse(self, entry, &key)?);
        }
        Ok(parsed)
    }

    fn endpoint(&self, entry: &Map<String, Value>, key: &str) -> Result<Endpoint, ConfigError> {
        self.known_keys(entry, key, &["base_url", "protocols", "api_key_env"])?;
        let base_url = self.url(
            self.required(entry, key, "base_url")?,
            &child(key, "base_url"),
        )?;
        let protocols_key = child(key, "protocols");
        let Value::Array(values) = self.required(entry, key, "protocols")? else {
            return Err(self.err(protocols_key, "must be a list"));
        };
        if values.is_empty() {
            return Err(self.err(protocols_key, "must not be empty"));
        }
        let mut protocols = Vec::with_capacity(values.len());
        for value in values {
            let protocol = value.as_str().and_then(Protocol::parse).ok_or_else(|| {
                self.err(
                    &protocols_key,
                    format!("entries {}", one_of(Protocol::ALL.map(Protocol::as_str))),
                )
            })?;
            if protocols.contains(&protocol) {
                return Err(self.err(
                    &protocols_key,
                    format!("lists `{}` more than once", protocol.as_str()),
                ));
            }
            protocols.push(protocol);
        }
        Ok(Endpoint {
            base_url,
            protocols,
            api_key_env: self.optional_string(entry, key, "api_key_env")?,
        })
    }

    fn model(&self, entry: &Map<String, Value>, key: &str) -> Result<Model, ConfigError> {
        self.known_keys(
            entry,
            key,
            &[
                "endpoint",
                "model",
                "context_window",
                "max_output_tokens",
                "tokenizer",
                "price",
            ],
        )?;
        Ok(Model {
            endpoint: self.string(
                self.required(entry, key, "endpoint")?,
                &child(key, "endpoint"),
            )?,
            model: self.string(self.required(entry, key, "model")?, &child(key, "model"))?,
            context_window: self.optional_count(entry, key, "context_window")?,
            max_output_tokens: self.optional_count(entry, key, "max_output_tokens")?,
            tokenizer: self.optional_string(entry, key, "tokenizer")?,
            price: entry
                .get("price")
                .map(|value| self.price(value, &child(key, "price")))
                .transpose()?,
        })
    }

    fn price(&self, value: &Value, key: &str) -> Result<Price, ConfigError> {
        let price = self.object(value, key)?;
        self.known_keys(
            price,
            key,
            &["input", "output", "cache_read", "cache_write"],
        )?;
        let number = |name: &str| {
            self.required(price, key, name)?
                .as_f64()
                .filter(|number| *number >= 0.0)
                .ok_or_else(|| self.err(child(key, name), "must be a number >= 0"))
        };
        Ok(Price {
            input: number("input")?,
            output: number("output")?,
            cache_read: number("cache_read")?,
            cache_write: number("cache_write")?,
        })
    }

    fn routing(&self, value: &Value) -> Result<RawRouting, ConfigError> {
        let key = "routing";
        let routing = self.object(value, key)?;
        self.known_keys(
            routing,
            key,
            &["decisions_model", "classifier_model", "policy"],
        )?;
        let mut raw = RawRouting {
            decisions_model: self.optional_string(routing, key, "decisions_model")?,
            classifier_model: self.optional_string(routing, key, "classifier_model")?,
            ..RawRouting::default()
        };
        if let Some(value) = routing.get("policy") {
            let key = "routing.policy";
            let policy = self.object(value, key)?;
            self.known_keys(policy, key, &["preset", "overrides"])?;
            raw.preset = self.optional_string(policy, key, "preset")?;
            raw.overrides = policy
                .get("overrides")
                .map(|value| self.object(value, "routing.policy.overrides").cloned())
                .transpose()?;
        }
        Ok(raw)
    }

    /// `routing` with its required keys present.
    fn finish_routing(&self, routing: Option<RawRouting>) -> Result<Option<Routing>, ConfigError> {
        let Some(routing) = routing else {
            return Ok(None);
        };
        let required =
            |value: Option<String>, key: &str| value.ok_or_else(|| self.err(key, "is required"));
        Ok(Some(Routing {
            decisions_model: required(routing.decisions_model, "routing.decisions_model")?,
            classifier_model: required(routing.classifier_model, "routing.classifier_model")?,
            policy: Policy {
                preset: required(routing.preset, "routing.policy.preset")?,
                overrides: routing.overrides,
            },
        }))
    }

    fn environment(
        &self,
        entry: &Map<String, Value>,
        key: &str,
    ) -> Result<Environment, ConfigError> {
        self.known_keys(entry, key, &["base_url", "login_url"])?;
        Ok(Environment {
            base_url: self.url(
                self.required(entry, key, "base_url")?,
                &child(key, "base_url"),
            )?,
            login_url: self.url(
                self.required(entry, key, "login_url")?,
                &child(key, "login_url"),
            )?,
        })
    }

    fn update(&self, value: &Value) -> Result<Option<UpdateChannel>, ConfigError> {
        let key = "update";
        let update = self.object(value, key)?;
        self.known_keys(update, key, &["channel"])?;
        update
            .get("channel")
            .map(|value| {
                value
                    .as_str()
                    .and_then(UpdateChannel::parse)
                    .ok_or_else(|| {
                        self.err(
                            "update.channel",
                            one_of(UpdateChannel::ALL.map(UpdateChannel::as_str)),
                        )
                    })
            })
            .transpose()
    }

    fn known_keys(
        &self,
        object: &Map<String, Value>,
        key: &str,
        known: &[&str],
    ) -> Result<(), ConfigError> {
        match object.keys().find(|name| !known.contains(&name.as_str())) {
            Some(name) => Err(self.err(child(key, name), "is not a known key")),
            None => Ok(()),
        }
    }

    fn object<'v>(
        &self,
        value: &'v Value,
        key: &str,
    ) -> Result<&'v Map<String, Value>, ConfigError> {
        value
            .as_object()
            .ok_or_else(|| self.err(key, "must be an object"))
    }

    fn required<'v>(
        &self,
        object: &'v Map<String, Value>,
        key: &str,
        name: &str,
    ) -> Result<&'v Value, ConfigError> {
        object
            .get(name)
            .ok_or_else(|| self.err(child(key, name), "is required"))
    }

    fn string(&self, value: &Value, key: &str) -> Result<String, ConfigError> {
        match value.as_str() {
            Some("") => Err(self.err(key, "must not be empty")),
            Some(value) => Ok(value.to_owned()),
            None => Err(self.err(key, "must be a string")),
        }
    }

    fn optional_string(
        &self,
        object: &Map<String, Value>,
        key: &str,
        name: &str,
    ) -> Result<Option<String>, ConfigError> {
        object
            .get(name)
            .map(|value| self.string(value, &child(key, name)))
            .transpose()
    }

    /// An absolute `http` or `https` URL.
    fn url(&self, value: &Value, key: &str) -> Result<Url, ConfigError> {
        let text = self.string(value, key)?;
        match Url::parse(&text) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => Ok(url),
            _ => Err(self.err(key, "must be an http or https URL")),
        }
    }

    /// An optional integer > 0.
    fn optional_count(
        &self,
        object: &Map<String, Value>,
        key: &str,
        name: &str,
    ) -> Result<Option<u64>, ConfigError> {
        object
            .get(name)
            .map(|value| {
                value
                    .as_u64()
                    .filter(|count| *count > 0)
                    .ok_or_else(|| self.err(child(key, name), "must be an integer > 0"))
            })
            .transpose()
    }
}

/// The key of `name` inside `key`, e.g. `endpoints.gpu-box`. A name that is
/// not a plain word is quoted: `models."z-ai/glm-5.3-flash"`.
fn child(key: &str, name: &str) -> String {
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let name = if plain {
        name.to_owned()
    } else {
        Value::String(name.to_owned()).to_string()
    };
    if key.is_empty() {
        name
    } else {
        format!("{key}.{name}")
    }
}

/// Why the file's text is not a config document.
enum JsonFailure {
    Syntax(serde_json::Error),
    /// An object repeats a key: the repeated key.
    DuplicateKey(String),
}

/// Parses JSON text, refusing any object that repeats a key.
fn parse_json(text: &str) -> Result<Value, JsonFailure> {
    let duplicate = RefCell::new(None);
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let parsed = StrictValue {
        key: String::new(),
        duplicate: &duplicate,
    }
    .deserialize(&mut deserializer)
    .and_then(|value| deserializer.end().map(|()| value));
    match (parsed, duplicate.into_inner()) {
        (_, Some(key)) => Err(JsonFailure::DuplicateKey(key)),
        (Ok(value), None) => Ok(value),
        (Err(error), None) => Err(JsonFailure::Syntax(error)),
    }
}

/// Deserializes one JSON value at `key`. A repeated key in an object is
/// recorded in `duplicate` and stops the parse.
struct StrictValue<'a> {
    key: String,
    duplicate: &'a RefCell<Option<String>>,
}

impl<'de> DeserializeSeed<'de> for StrictValue<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StrictValue<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Value, E> {
        Ok(Value::from(value))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(StrictValue {
            key: format!("{}[{}]", self.key, values.len()),
            duplicate: self.duplicate,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut object = Map::new();
        while let Some(name) = map.next_key::<String>()? {
            let key = child(&self.key, &name);
            if object.contains_key(&name) {
                *self.duplicate.borrow_mut() = Some(key);
                return Err(de::Error::custom("duplicate key"));
            }
            let value = map.next_value_seed(StrictValue {
                key,
                duplicate: self.duplicate,
            })?;
            object.insert(name, value);
        }
        Ok(Value::Object(object))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

    const GLM: &str = "z-ai/glm-5.3-flash";

    fn temp_home() -> PathBuf {
        let home = std::env::temp_dir().join(format!(
            "rayline-config-test-{}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        home
    }

    /// The example of section 5, with an internal environment.
    fn example() -> Value {
        json!({
            "endpoints": {
                "gpu-box": {
                    "base_url": "https://gpu-box.example.com:8443",
                    "protocols": ["anthropic_messages", "openai_chat", "openai_responses"],
                    "api_key_env": "GPU_BOX_API_KEY"
                },
                "ollama": {
                    "base_url": "http://127.0.0.1:11434",
                    "protocols": ["openai_chat", "anthropic_messages"]
                }
            },
            "models": {
                GLM: {
                    "endpoint": "gpu-box",
                    "model": "GLM-5.3-Flash-EXL3",
                    "context_window": 1_000_000,
                    "max_output_tokens": 32_000,
                    "tokenizer": "zai-org/GLM-5.3-Flash",
                    "price": { "input": 0, "output": 0, "cache_read": 0, "cache_write": 0 }
                }
            },
            "routing": {
                "decisions_model": "arc-swift-1.0",
                "classifier_model": "anthropic/claude-haiku-5.5",
                "policy": {
                    "preset": "arc-policy.prefer-local.v1",
                    "overrides": {
                        "selection": { "min_p_pass": 0.5 },
                        "schedule": { "model": ["cache_boundaries", "user_messages"] }
                    }
                }
            },
            "environments": {
                "dev": {
                    "base_url": "https://api.dev.example.com",
                    "login_url": "https://platform.dev.example.com/cli-auth"
                }
            },
            "update": { "channel": "dev" }
        })
    }

    fn set(doc: &mut Value, path: &[&str], value: Value) {
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut node = doc;
        for name in parents {
            node = node.get_mut(*name).expect("parent exists");
        }
        node.as_object_mut()
            .expect("parent is an object")
            .insert((*last).to_owned(), value);
    }

    fn remove(doc: &mut Value, path: &[&str]) {
        let (last, parents) = path.split_last().expect("non-empty path");
        let mut node = doc;
        for name in parents {
            node = node.get_mut(*name).expect("parent exists");
        }
        node.as_object_mut()
            .expect("parent is an object")
            .remove(*last)
            .expect("key exists");
    }

    fn write_default(home: &Path, doc: &Value) -> PathBuf {
        let file = settings_file(home);
        fs::create_dir_all(file.parent().expect("settings dir")).expect("create settings dir");
        fs::write(&file, doc.to_string()).expect("write settings");
        file
    }

    fn write_file(home: &Path, name: &str, doc: &Value) -> PathBuf {
        let file = home.join(name);
        fs::write(&file, doc.to_string()).expect("write config");
        file
    }

    fn opts(mode: Mode<'_>) -> LoadOptions<'_> {
        LoadOptions {
            mode,
            env: None,
            harness_protocol: None,
        }
    }

    fn load_env(
        home: &Path,
        options: LoadOptions<'_>,
        env: &[(&str, &str)],
    ) -> Result<Settings, ConfigError> {
        let env: HashMap<&str, OsString> = env
            .iter()
            .map(|(name, value)| (*name, OsString::from(value)))
            .collect();
        load_from(home, &options, &|name| env.get(name).cloned())
    }

    fn load(home: &Path, options: LoadOptions<'_>) -> Result<Settings, ConfigError> {
        load_env(home, options, &[])
    }

    /// Loads `doc` as the default file in cloud mode.
    fn load_doc(doc: &Value) -> Result<Settings, ConfigError> {
        let home = temp_home();
        write_default(&home, doc);
        load(&home, opts(Mode::Cloud))
    }

    #[track_caller]
    fn key_error<T: fmt::Debug>(result: Result<T, ConfigError>) -> (String, String) {
        match result {
            Err(ConfigError::Key { key, problem, .. }) => (key, problem),
            other => panic!("expected a key error, got {other:?}"),
        }
    }

    #[track_caller]
    fn env_error(result: Result<Settings, ConfigError>) -> (&'static str, String) {
        match result {
            Err(ConfigError::Env { var, problem, .. }) => (var, problem),
            other => panic!("expected an env error, got {other:?}"),
        }
    }

    #[test]
    fn missing_default_file_is_an_empty_config() {
        let home = temp_home();
        let settings = load(&home, opts(Mode::Cloud)).expect("load");
        assert_eq!(
            settings,
            Settings {
                file: settings_file(&home),
                endpoints: BTreeMap::new(),
                models: BTreeMap::new(),
                routing: None,
                environments: BTreeMap::new(),
                update: Update {
                    channel: UpdateChannel::Prod
                },
                local: false,
                env: None,
            }
        );
    }

    #[test]
    fn missing_config_file_is_a_load_error() {
        let home = temp_home();
        let missing = home.join("missing.json");
        for mode in [Mode::File(&missing), Mode::LocalFile(&missing)] {
            match load(&home, opts(mode)) {
                Err(ConfigError::Read { file, .. }) => assert_eq!(file, missing),
                other => panic!("expected a read error, got {other:?}"),
            }
        }
    }

    #[test]
    fn example_loads_into_typed_settings() {
        let settings = load_doc(&example()).expect("load");
        let gpu_box = &settings.endpoints["gpu-box"];
        assert_eq!(
            gpu_box.base_url.as_str(),
            "https://gpu-box.example.com:8443/"
        );
        assert_eq!(gpu_box.protocols, Protocol::ALL.to_vec());
        assert_eq!(gpu_box.api_key_env.as_deref(), Some("GPU_BOX_API_KEY"));
        assert_eq!(settings.endpoints["ollama"].api_key_env, None);
        assert_eq!(
            settings.models[GLM],
            Model {
                endpoint: "gpu-box".to_owned(),
                model: "GLM-5.3-Flash-EXL3".to_owned(),
                context_window: Some(1_000_000),
                max_output_tokens: Some(32_000),
                tokenizer: Some("zai-org/GLM-5.3-Flash".to_owned()),
                price: Some(Price {
                    input: 0.0,
                    output: 0.0,
                    cache_read: 0.0,
                    cache_write: 0.0,
                }),
            }
        );
        let routing = settings.routing.as_ref().expect("routing");
        assert_eq!(routing.decisions_model, "arc-swift-1.0");
        assert_eq!(routing.classifier_model, "anthropic/claude-haiku-5.5");
        assert_eq!(routing.policy.preset, "arc-policy.prefer-local.v1");
        assert_eq!(
            routing.policy.overrides.clone().map(Value::Object),
            Some(example()["routing"]["policy"]["overrides"].clone())
        );
        assert_eq!(
            settings.environments["dev"].login_url.as_str(),
            "https://platform.dev.example.com/cli-auth"
        );
        assert_eq!(settings.update.channel, UpdateChannel::Dev);
        assert!(!settings.local, "neither flag is cloud mode");
    }

    #[test]
    fn optional_keys_may_be_left_out() {
        let doc = json!({
            "endpoints": { "box": { "base_url": "http://box", "protocols": ["openai_chat"] } },
            "models": { "m": { "endpoint": "box", "model": "m" } },
            "routing": {
                "decisions_model": "d",
                "classifier_model": "c",
                "policy": { "preset": "p" }
            },
            "update": {}
        });
        let settings = load_doc(&doc).expect("load");
        let model = &settings.models["m"];
        assert_eq!(model.context_window, None);
        assert_eq!(model.max_output_tokens, None);
        assert_eq!(model.tokenizer, None);
        assert_eq!(model.price, None);
        assert_eq!(settings.routing.expect("routing").policy.overrides, None);
        assert_eq!(settings.update.channel, UpdateChannel::Prod);
    }

    #[test]
    fn mode_comes_from_the_flags_and_the_file() {
        let home = temp_home();
        let with_routing = write_file(&home, "local.json", &example());
        let mut cloud = example();
        remove(&mut cloud, &["routing"]);
        let without_routing = write_file(&home, "cloud.json", &cloud);
        write_default(&home, &example());

        assert!(!load(&home, opts(Mode::Cloud)).expect("cloud").local);
        assert!(load(&home, opts(Mode::Local)).expect("local").local);
        assert!(
            load(&home, opts(Mode::File(&with_routing)))
                .expect("file")
                .local
        );
        assert!(
            !load(&home, opts(Mode::File(&without_routing)))
                .expect("file")
                .local
        );
        let settings = load(&home, opts(Mode::LocalFile(&with_routing))).expect("local file");
        assert!(settings.local);
        assert_eq!(settings.file, with_routing);
    }

    #[test]
    fn config_file_replaces_the_default_file() {
        let home = temp_home();
        write_default(&home, &example());
        let file = write_file(&home, "other.json", &json!({}));
        let settings = load(&home, opts(Mode::File(&file))).expect("load");
        assert!(settings.endpoints.is_empty());
        assert!(settings.routing.is_none());
    }

    #[test]
    fn invalid_json_is_a_load_error() {
        let home = temp_home();
        let file = settings_file(&home);
        fs::create_dir_all(file.parent().expect("settings dir")).expect("create settings dir");
        for text in ["", "{", "{\"routing\": }"] {
            fs::write(&file, text).expect("write settings");
            assert!(
                matches!(
                    load(&home, opts(Mode::Cloud)),
                    Err(ConfigError::Parse { .. })
                ),
                "{text:?}"
            );
        }
    }

    #[test]
    fn duplicate_keys_are_load_errors_naming_the_key() {
        let cases = [
            (r#"{"update": {}, "update": {}}"#, "update"),
            (
                r#"{"update": {"channel": "dev", "channel": "main"}}"#,
                "update.channel",
            ),
            (
                r#"{"endpoints": {"box": {"base_url": "http://a", "base_url": "http://b"}}}"#,
                "endpoints.box.base_url",
            ),
            (
                r#"{"models": {"z-ai/glm-5.3-flash": {}, "z-ai/glm-5.3-flash": {}}}"#,
                "models.\"z-ai/glm-5.3-flash\"",
            ),
            (
                r#"{"routing": {"policy": {"overrides": {"selection": {"min_p_pass": 0.5, "min_p_pass": 0.6}}}}}"#,
                "routing.policy.overrides.selection.min_p_pass",
            ),
            (
                r#"{"routing": {"policy": {"overrides": {"rules": [{}, {"a": 1, "a": 2}]}}}}"#,
                "routing.policy.overrides.rules[1].a",
            ),
        ];
        let home = temp_home();
        let file = settings_file(&home);
        fs::create_dir_all(file.parent().expect("settings dir")).expect("create settings dir");
        for (text, key) in cases {
            fs::write(&file, text).expect("write settings");
            assert_eq!(
                key_error(load(&home, opts(Mode::Cloud))),
                (key.to_owned(), "appears more than once".to_owned()),
                "{text}"
            );
        }
    }

    #[test]
    fn json_parses_to_the_same_values_as_serde_json() {
        let text = r#"{"a": [1, -2, 3.5, 1e3, true, false, null, "sé", [], {}],
            "b": {"c": {"d": 18446744073709551615}}, "e": ""}"#;
        let expected: Value = serde_json::from_str(text).expect("serde_json parses it");
        assert!(matches!(parse_json(text), Ok(value) if value == expected));
        let doc = example();
        assert!(matches!(parse_json(&doc.to_string()), Ok(value) if value == doc));
        assert!(matches!(parse_json("{} x"), Err(JsonFailure::Syntax(_))));
    }

    #[test]
    fn top_level_must_be_an_object() {
        for doc in [json!([]), json!("x"), json!(null)] {
            assert!(
                matches!(load_doc(&doc), Err(ConfigError::Parse { .. })),
                "{doc}"
            );
        }
    }

    #[test]
    fn unknown_keys_are_load_errors_at_every_level() {
        let cases: &[(&[&str], &str)] = &[
            (&["extra"], "extra"),
            (&["local_model"], "local_model"),
            (&["hybrid"], "hybrid"),
            (
                &["endpoints", "gpu-box", "protocol"],
                "endpoints.gpu-box.protocol",
            ),
            (
                &["models", GLM, "n_ctx"],
                "models.\"z-ai/glm-5.3-flash\".n_ctx",
            ),
            (
                &["models", GLM, "price", "input_per_mtok"],
                "models.\"z-ai/glm-5.3-flash\".price.input_per_mtok",
            ),
            (&["routing", "local_price"], "routing.local_price"),
            (&["routing", "policy", "name"], "routing.policy.name"),
            (
                &["environments", "dev", "router_url"],
                "environments.dev.router_url",
            ),
            (&["update", "base_url"], "update.base_url"),
        ];
        for (path, key) in cases {
            let mut doc = example();
            set(&mut doc, path, json!("x"));
            assert_eq!(
                key_error(load_doc(&doc)),
                ((*key).to_owned(), "is not a known key".to_owned())
            );
        }
    }

    #[test]
    fn policy_overrides_are_passed_through_unchecked() {
        let mut doc = example();
        let overrides = json!({ "anything": { "ARC": ["checks", 1] }, "x": null });
        set(
            &mut doc,
            &["routing", "policy", "overrides"],
            overrides.clone(),
        );
        let settings = load_doc(&doc).expect("load");
        assert_eq!(
            settings.routing.expect("routing").policy.overrides,
            overrides.as_object().cloned()
        );
    }

    #[test]
    fn wrong_values_are_load_errors_naming_the_key() {
        let glm = |name: &str| format!("models.\"z-ai/glm-5.3-flash\".{name}");
        let gpu = |name: &str| format!("endpoints.gpu-box.{name}");
        let cases: Vec<(&[&str], Value, String)> = vec![
            (&["endpoints"], json!([]), "endpoints".to_owned()),
            (
                &["endpoints", "gpu-box"],
                json!("x"),
                "endpoints.gpu-box".to_owned(),
            ),
            (&["endpoints", ""], json!({}), "endpoints.\"\"".to_owned()),
            (
                &["endpoints", "gpu-box", "base_url"],
                json!(5),
                gpu("base_url"),
            ),
            (
                &["endpoints", "gpu-box", "base_url"],
                json!(""),
                gpu("base_url"),
            ),
            (
                &["endpoints", "gpu-box", "base_url"],
                json!("gpu-box:8443"),
                gpu("base_url"),
            ),
            (
                &["endpoints", "gpu-box", "base_url"],
                json!("/v1"),
                gpu("base_url"),
            ),
            (
                &["endpoints", "gpu-box", "base_url"],
                json!("ftp://gpu-box"),
                gpu("base_url"),
            ),
            (
                &["endpoints", "gpu-box", "protocols"],
                json!("openai_chat"),
                gpu("protocols"),
            ),
            (
                &["endpoints", "gpu-box", "protocols"],
                json!([]),
                gpu("protocols"),
            ),
            (
                &["endpoints", "gpu-box", "protocols"],
                json!(["openai"]),
                gpu("protocols"),
            ),
            (
                &["endpoints", "gpu-box", "protocols"],
                json!([1]),
                gpu("protocols"),
            ),
            (
                &["endpoints", "gpu-box", "protocols"],
                json!(["openai_chat", "openai_chat"]),
                gpu("protocols"),
            ),
            (
                &["endpoints", "gpu-box", "api_key_env"],
                json!(""),
                gpu("api_key_env"),
            ),
            (
                &["endpoints", "gpu-box", "api_key_env"],
                json!(1),
                gpu("api_key_env"),
            ),
            (&["models"], json!("x"), "models".to_owned()),
            (&["models", ""], json!({}), "models.\"\"".to_owned()),
            (&["models", GLM, "endpoint"], json!(""), glm("endpoint")),
            (&["models", GLM, "model"], json!(3), glm("model")),
            (
                &["models", GLM, "context_window"],
                json!(0),
                glm("context_window"),
            ),
            (
                &["models", GLM, "context_window"],
                json!(-1),
                glm("context_window"),
            ),
            (
                &["models", GLM, "context_window"],
                json!(1.5),
                glm("context_window"),
            ),
            (
                &["models", GLM, "context_window"],
                json!("1000"),
                glm("context_window"),
            ),
            (
                &["models", GLM, "max_output_tokens"],
                json!(0),
                glm("max_output_tokens"),
            ),
            (&["models", GLM, "tokenizer"], json!(""), glm("tokenizer")),
            (&["models", GLM, "price"], json!(1), glm("price")),
            (
                &["models", GLM, "price", "input"],
                json!(-0.5),
                glm("price.input"),
            ),
            (
                &["models", GLM, "price", "output"],
                json!("1"),
                glm("price.output"),
            ),
            (&["routing"], json!("local"), "routing".to_owned()),
            (
                &["routing", "decisions_model"],
                json!(""),
                "routing.decisions_model".to_owned(),
            ),
            (
                &["routing", "classifier_model"],
                json!(1),
                "routing.classifier_model".to_owned(),
            ),
            (
                &["routing", "policy"],
                json!("prefer-local"),
                "routing.policy".to_owned(),
            ),
            (
                &["routing", "policy", "preset"],
                json!(""),
                "routing.policy.preset".to_owned(),
            ),
            (
                &["routing", "policy", "overrides"],
                json!([]),
                "routing.policy.overrides".to_owned(),
            ),
            (
                &["routing", "policy", "overrides"],
                json!(null),
                "routing.policy.overrides".to_owned(),
            ),
            (&["environments"], json!([]), "environments".to_owned()),
            (
                &["environments", ""],
                json!({}),
                "environments.\"\"".to_owned(),
            ),
            (
                &["environments", "dev", "base_url"],
                json!("dev"),
                "environments.dev.base_url".to_owned(),
            ),
            (
                &["environments", "dev", "login_url"],
                json!(1),
                "environments.dev.login_url".to_owned(),
            ),
            (&["update"], json!("dev"), "update".to_owned()),
            (
                &["update", "channel"],
                json!("beta"),
                "update.channel".to_owned(),
            ),
            (
                &["update", "channel"],
                json!("Prod"),
                "update.channel".to_owned(),
            ),
            (
                &["update", "channel"],
                json!(1),
                "update.channel".to_owned(),
            ),
        ];
        for (path, value, key) in cases {
            let mut doc = example();
            set(&mut doc, path, value.clone());
            assert_eq!(key_error(load_doc(&doc)).0, key, "{path:?} = {value}");
        }
    }

    #[test]
    fn missing_required_keys_are_load_errors() {
        let glm = |name: &str| format!("models.\"z-ai/glm-5.3-flash\".{name}");
        let cases: Vec<(&[&str], String)> = vec![
            (
                &["endpoints", "gpu-box", "base_url"],
                "endpoints.gpu-box.base_url".to_owned(),
            ),
            (
                &["endpoints", "gpu-box", "protocols"],
                "endpoints.gpu-box.protocols".to_owned(),
            ),
            (&["models", GLM, "endpoint"], glm("endpoint")),
            (&["models", GLM, "model"], glm("model")),
            (&["models", GLM, "price", "input"], glm("price.input")),
            (&["models", GLM, "price", "output"], glm("price.output")),
            (
                &["models", GLM, "price", "cache_read"],
                glm("price.cache_read"),
            ),
            (
                &["models", GLM, "price", "cache_write"],
                glm("price.cache_write"),
            ),
            (
                &["routing", "decisions_model"],
                "routing.decisions_model".to_owned(),
            ),
            (
                &["routing", "classifier_model"],
                "routing.classifier_model".to_owned(),
            ),
            (&["routing", "policy"], "routing.policy.preset".to_owned()),
            (
                &["routing", "policy", "preset"],
                "routing.policy.preset".to_owned(),
            ),
            (
                &["environments", "dev", "base_url"],
                "environments.dev.base_url".to_owned(),
            ),
            (
                &["environments", "dev", "login_url"],
                "environments.dev.login_url".to_owned(),
            ),
        ];
        for (path, key) in cases {
            let mut doc = example();
            remove(&mut doc, path);
            assert_eq!(
                key_error(load_doc(&doc)),
                (key, "is required".to_owned()),
                "{path:?}"
            );
        }
    }

    #[test]
    fn local_mode_without_routing_is_a_load_error() {
        let home = temp_home();
        let mut doc = example();
        remove(&mut doc, &["routing"]);
        write_default(&home, &doc);
        let file = write_file(&home, "cloud.json", &doc);
        for mode in [Mode::Local, Mode::LocalFile(&file)] {
            assert_eq!(
                key_error(load(&home, opts(mode))),
                ("routing".to_owned(), "is required in local mode".to_owned())
            );
        }
        let empty = temp_home();
        assert_eq!(key_error(load(&empty, opts(Mode::Local))).0, "routing");
    }

    #[test]
    fn model_endpoint_must_be_in_endpoints() {
        let mut doc = example();
        set(&mut doc, &["models", GLM, "endpoint"], json!("lm-studio"));
        assert_eq!(
            key_error(load_doc(&doc)),
            (
                "models.\"z-ai/glm-5.3-flash\".endpoint".to_owned(),
                "names `lm-studio`, which is not in `endpoints`".to_owned()
            )
        );
    }

    #[test]
    fn local_mode_endpoints_must_list_the_harness_protocol() {
        let home = temp_home();
        write_default(&home, &example());
        let mut cloud = example();
        remove(&mut cloud, &["routing"]);
        let cloud_file = write_file(&home, "cloud.json", &cloud);
        let local_file = write_file(&home, "local.json", &example());
        let run = |mode, protocol| {
            load(
                &home,
                LoadOptions {
                    mode,
                    env: None,
                    harness_protocol: protocol,
                },
            )
        };

        for mode in [Mode::Local, Mode::File(&local_file)] {
            assert_eq!(
                key_error(run(mode, Some(Protocol::OpenaiResponses))),
                (
                    "endpoints.ollama.protocols".to_owned(),
                    "does not list `openai_responses`, the protocol of the harness being run"
                        .to_owned()
                )
            );
            run(mode, Some(Protocol::AnthropicMessages)).expect("both endpoints list it");
        }
        // Cloud mode doesn't use endpoints; `local start` has no harness.
        run(Mode::Cloud, Some(Protocol::OpenaiResponses)).expect("cloud");
        run(Mode::File(&cloud_file), Some(Protocol::OpenaiResponses)).expect("cloud file");
        run(Mode::Local, None).expect("local start");
        run(Mode::LocalFile(&local_file), None).expect("local start --config");
    }

    #[test]
    fn env_selects_an_environments_entry() {
        let home = temp_home();
        write_default(&home, &example());
        let with_env = |env| {
            load(
                &home,
                LoadOptions {
                    mode: Mode::Cloud,
                    env: Some(env),
                    harness_protocol: None,
                },
            )
        };
        assert_eq!(with_env("dev").expect("dev").env.as_deref(), Some("dev"));
        for name in ["staging", "prod"] {
            assert_eq!(
                key_error(with_env(name)),
                (
                    format!("environments.{name}"),
                    format!("is not defined, and `--env {name}` selects it")
                )
            );
        }
    }

    #[test]
    fn env_overrides_replace_file_values() {
        let home = temp_home();
        write_default(&home, &example());
        let settings = load_env(
            &home,
            opts(Mode::Cloud),
            &[
                ("RAYLINE_ROUTING_DECISIONS_MODEL", "arc-swift-2.0"),
                ("RAYLINE_ROUTING_CLASSIFIER_MODEL", "classifier-x"),
                (
                    "RAYLINE_ROUTING_POLICY_PRESET",
                    "arc-policy.hybrid-default.v1",
                ),
                ("RAYLINE_UPDATE_CHANNEL", "main"),
            ],
        )
        .expect("load");
        let routing = settings.routing.expect("routing");
        assert_eq!(routing.decisions_model, "arc-swift-2.0");
        assert_eq!(routing.classifier_model, "classifier-x");
        assert_eq!(routing.policy.preset, "arc-policy.hybrid-default.v1");
        assert!(
            routing.policy.overrides.is_some(),
            "overrides stay from the file"
        );
        assert_eq!(settings.update.channel, UpdateChannel::Main);
    }

    #[test]
    fn each_env_override_applies_on_its_own() {
        let home = temp_home();
        write_default(&home, &example());
        let file = write_file(&home, "local.json", &example());
        for mode in [Mode::Cloud, Mode::File(&file)] {
            let with = |var, value| load_env(&home, opts(mode), &[(var, value)]).expect("load");
            let routing = |settings: Settings| settings.routing.expect("routing");
            let settings = routing(with("RAYLINE_ROUTING_DECISIONS_MODEL", "d2"));
            assert_eq!(settings.decisions_model, "d2");
            assert_eq!(settings.classifier_model, "anthropic/claude-haiku-5.5");
            let settings = routing(with("RAYLINE_ROUTING_CLASSIFIER_MODEL", "c2"));
            assert_eq!(settings.classifier_model, "c2");
            assert_eq!(settings.decisions_model, "arc-swift-1.0");
            let settings = routing(with("RAYLINE_ROUTING_POLICY_PRESET", "p2"));
            assert_eq!(settings.policy.preset, "p2");
            for channel in UpdateChannel::ALL {
                assert_eq!(
                    with("RAYLINE_UPDATE_CHANNEL", channel.as_str())
                        .update
                        .channel,
                    channel
                );
            }
        }
    }

    #[test]
    fn env_overrides_fill_keys_missing_from_the_file() {
        let home = temp_home();
        let mut doc = example();
        remove(&mut doc, &["routing", "decisions_model"]);
        remove(&mut doc, &["routing", "policy"]);
        remove(&mut doc, &["update"]);
        write_default(&home, &doc);
        let settings = load_env(
            &home,
            opts(Mode::Local),
            &[
                ("RAYLINE_ROUTING_DECISIONS_MODEL", "arc-swift-1.0"),
                (
                    "RAYLINE_ROUTING_POLICY_PRESET",
                    "arc-policy.prefer-local.v1",
                ),
                ("RAYLINE_UPDATE_CHANNEL", "dev"),
            ],
        )
        .expect("load");
        let routing = settings.routing.expect("routing");
        assert_eq!(routing.decisions_model, "arc-swift-1.0");
        assert_eq!(routing.policy.preset, "arc-policy.prefer-local.v1");
        assert_eq!(routing.policy.overrides, None);
        assert_eq!(settings.update.channel, UpdateChannel::Dev);
    }

    #[test]
    fn invalid_env_overrides_are_load_errors() {
        let home = temp_home();
        write_default(&home, &example());
        let cases = [
            ("RAYLINE_ROUTING_DECISIONS_MODEL", "", "must not be empty"),
            ("RAYLINE_ROUTING_CLASSIFIER_MODEL", "", "must not be empty"),
            ("RAYLINE_ROUTING_POLICY_PRESET", "", "must not be empty"),
            ("RAYLINE_UPDATE_CHANNEL", "", "must not be empty"),
            (
                "RAYLINE_UPDATE_CHANNEL",
                "beta",
                "must be one of `prod`, `dev`, `main`",
            ),
            (
                "RAYLINE_UPDATE_CHANNEL",
                "PROD",
                "must be one of `prod`, `dev`, `main`",
            ),
        ];
        for (var, value, problem) in cases {
            assert_eq!(
                env_error(load_env(&home, opts(Mode::Cloud), &[(var, value)])),
                (var, problem.to_owned()),
                "{var}={value:?}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_env_override_is_a_load_error() {
        use std::os::unix::ffi::OsStringExt as _;
        let home = temp_home();
        write_default(&home, &example());
        let result = load_from(&home, &opts(Mode::Cloud), &|name| {
            (name == "RAYLINE_ROUTING_POLICY_PRESET").then(|| OsString::from_vec(vec![0xff]))
        });
        assert_eq!(
            env_error(result),
            (
                "RAYLINE_ROUTING_POLICY_PRESET",
                "must be valid UTF-8".to_owned()
            )
        );
    }

    #[test]
    fn routing_env_override_without_routing_in_the_file_is_a_load_error() {
        let home = temp_home();
        let mut doc = example();
        remove(&mut doc, &["routing"]);
        write_default(&home, &doc);
        let file = write_file(&home, "cloud.json", &doc);
        for var in [
            "RAYLINE_ROUTING_DECISIONS_MODEL",
            "RAYLINE_ROUTING_CLASSIFIER_MODEL",
            "RAYLINE_ROUTING_POLICY_PRESET",
        ] {
            for mode in [Mode::Cloud, Mode::File(&file)] {
                assert_eq!(
                    env_error(load_env(&home, opts(mode), &[(var, "x")])),
                    (
                        var,
                        "is set, but the config file has no `routing`".to_owned()
                    )
                );
            }
        }
        let settings = load_env(
            &home,
            opts(Mode::File(&file)),
            &[("RAYLINE_UPDATE_CHANNEL", "dev")],
        )
        .expect("update.channel needs no section");
        assert_eq!(settings.update.channel, UpdateChannel::Dev);
    }

    #[test]
    fn start_check_refuses_an_unset_api_key_env() {
        let settings = load_doc(&example()).expect("load");
        let arc_knows_all = |_: &str| true;
        assert_eq!(
            key_error(check_local_start_with(&settings, &|_| None, &arc_knows_all)),
            (
                "endpoints.gpu-box.api_key_env".to_owned(),
                "names `GPU_BOX_API_KEY`, which is not set".to_owned()
            )
        );
        let env = |name: &str| (name == "GPU_BOX_API_KEY").then(|| OsString::from("key"));
        check_local_start_with(&settings, &env, &arc_knows_all).expect("key is set");
    }

    #[test]
    fn start_check_needs_limits_for_models_arc_does_not_know() {
        let env = |_: &str| Some(OsString::from("key"));
        let arc_knows_glm = |id: &str| id == GLM;
        let arc_knows_none = |_: &str| false;
        for key in ["context_window", "max_output_tokens"] {
            let mut doc = example();
            remove(&mut doc, &["models", GLM, key]);
            let settings = load_doc(&doc).expect("load");
            check_local_start_with(&settings, &env, &arc_knows_glm).expect("ARC knows the model");
            assert_eq!(
                key_error(check_local_start_with(&settings, &env, &arc_knows_none)),
                (
                    format!("models.\"z-ai/glm-5.3-flash\".{key}"),
                    "is required: ARC doesn't know this model".to_owned()
                )
            );
        }
        let settings = load_doc(&example()).expect("load");
        check_local_start_with(&settings, &env, &arc_knows_none).expect("limits are set");
    }

    #[test]
    fn errors_name_the_file_and_the_key() {
        let key = ConfigError::Key {
            file: PathBuf::from("settings.json"),
            key: "routing.policy.preset".to_owned(),
            problem: "is required".to_owned(),
        };
        assert_eq!(
            key.to_string(),
            "settings.json: `routing.policy.preset` is required"
        );
        let env = ConfigError::Env {
            var: "RAYLINE_UPDATE_CHANNEL",
            key: "update.channel",
            problem: "must not be empty".to_owned(),
        };
        assert_eq!(
            env.to_string(),
            "`RAYLINE_UPDATE_CHANNEL` (overrides `update.channel`) must not be empty"
        );
    }
}
