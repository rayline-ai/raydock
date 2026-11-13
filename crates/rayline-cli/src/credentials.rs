//! `~/.config/rayline/credentials.json`: the Rayline session and router key.
//!
//! Built-in prod sits at the top level; `environments.<name>` holds the same two
//! objects for an environment selected with `--env <name>`. An environment is signed out when its objects
//! are absent, and signed in when both are present with every field. The file is
//! validated as a whole when it is loaded: anything else is a load error that
//! tells the user to run `auth login`. Load errors name the key, never a value.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const ROOT_KEYS: &[&str] = &["session", "router_key", "environments"];
const ENVIRONMENT_KEYS: &[&str] = &["session", "router_key"];
const SESSION_KEYS: &[&str] = &[
    "email",
    "subject",
    "access_token",
    "access_token_expires_at",
    "refresh_token",
    "refresh_token_expires_at",
    "signed_in_at",
];
const ROUTER_KEY_KEYS: &[&str] = &["api_key", "subject", "saved_at"];

/// The signed-in Rayline session of one environment. Timestamps are Unix
/// seconds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Session {
    pub email: String,
    pub subject: String,
    pub access_token: String,
    pub access_token_expires_at: i64,
    pub refresh_token: String,
    pub refresh_token_expires_at: i64,
    pub signed_in_at: i64,
}

/// The router key minted for the account `subject`. `saved_at` is Unix seconds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RouterKey {
    pub api_key: String,
    pub subject: String,
    pub saved_at: i64,
}

/// What a signed-in environment stores: its session and its router key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Account {
    pub session: Session,
    pub router_key: RouterKey,
}

/// The whole credentials file, validated.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct Credentials {
    prod: Option<Account>,
    environments: BTreeMap<String, Account>,
}

#[derive(Debug)]
pub enum CredentialsError {
    Read { path: PathBuf, error: io::Error },
    Invalid { path: PathBuf, reason: String },
}

impl fmt::Display for CredentialsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, error } => {
                write!(formatter, "cannot read {}: {error}", path.display())
            }
            Self::Invalid { path, reason } => write!(
                formatter,
                "{} is invalid: {reason}. Run: {} auth login",
                path.display(),
                crate::CLI_BIN
            ),
        }
    }
}

impl std::error::Error for CredentialsError {}

/// Which slot of the file an environment uses: `None` is built-in prod at the
/// top level, `Some(name)` is `environments.<name>`.
pub(crate) type Slot<'a> = Option<&'a str>;

impl Credentials {
    pub(crate) fn account(&self, slot: Slot<'_>) -> Option<&Account> {
        match slot {
            None => self.prod.as_ref(),
            Some(name) => self.environments.get(name),
        }
    }

    pub(crate) fn account_mut(&mut self, slot: Slot<'_>) -> Option<&mut Account> {
        match slot {
            None => self.prod.as_mut(),
            Some(name) => self.environments.get_mut(name),
        }
    }

    pub(crate) fn set_account(&mut self, slot: Slot<'_>, account: Account) {
        match slot {
            None => self.prod = Some(account),
            Some(name) => {
                self.environments.insert(name.to_owned(), account);
            }
        }
    }

    pub(crate) fn remove_account(&mut self, slot: Slot<'_>) -> Option<Account> {
        match slot {
            None => self.prod.take(),
            Some(name) => self.environments.remove(name),
        }
    }

    fn to_value(&self) -> io::Result<Value> {
        let mut root = Map::new();
        if let Some(account) = &self.prod {
            insert_account(&mut root, account)?;
        }
        if !self.environments.is_empty() {
            let mut environments = Map::new();
            for (name, account) in &self.environments {
                let mut entry = Map::new();
                insert_account(&mut entry, account)?;
                environments.insert(name.clone(), Value::Object(entry));
            }
            root.insert("environments".to_owned(), Value::Object(environments));
        }
        Ok(Value::Object(root))
    }
}

pub(crate) fn credentials_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("credentials.json")
}

/// Load and validate the credentials file. A missing file is signed out
/// everywhere.
pub(crate) fn load(home: &Path) -> Result<Credentials, CredentialsError> {
    let path = credentials_file(home);
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Credentials::default());
        }
        Err(error) => return Err(CredentialsError::Read { path, error }),
    };
    parse(&contents).map_err(|reason| CredentialsError::Invalid { path, reason })
}

/// Write the whole credentials file atomically, readable only by the user.
pub(crate) fn save(home: &Path, credentials: &Credentials) -> io::Result<()> {
    crate::status::write_json_atomic(&credentials_file(home), &credentials.to_value()?)
}

pub(crate) fn format_timestamp(seconds: i64) -> io::Result<String> {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|timestamp| timestamp.format(&Rfc3339).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("timestamp {seconds} is out of range"),
            )
        })
}

fn parse(contents: &str) -> Result<Credentials, String> {
    let value = crate::config::parse_json(contents).map_err(|failure| match failure {
        crate::config::JsonFailure::Syntax(error) => format!(
            "not valid JSON (line {}, column {})",
            error.line(),
            error.column()
        ),
        crate::config::JsonFailure::DuplicateKey(key) => format!("`{key}` appears more than once"),
    })?;
    let root = value
        .as_object()
        .ok_or_else(|| "the top level must be an object".to_owned())?;
    reject_unknown_keys(root, "", ROOT_KEYS)?;

    let prod = parse_slot(root, "")?;
    let mut environments = BTreeMap::new();
    if let Some(value) = root.get("environments") {
        let entries = value
            .as_object()
            .ok_or_else(|| "`environments` must be an object".to_owned())?;
        for (name, value) in entries {
            let path = format!("environments.{name}.");
            if !crate::status::is_valid_root_env(name) {
                return Err(format!(
                    "`environments.{name}` is not a valid environment name"
                ));
            }
            let entry = value
                .as_object()
                .ok_or_else(|| format!("`environments.{name}` must be an object"))?;
            reject_unknown_keys(entry, &path, ENVIRONMENT_KEYS)?;
            let account = parse_slot(entry, &path)?
                .ok_or_else(|| format!("`environments.{name}` is missing `session`"))?;
            environments.insert(name.clone(), account);
        }
    }
    Ok(Credentials { prod, environments })
}

fn parse_slot(slot: &Map<String, Value>, path: &str) -> Result<Option<Account>, String> {
    match (slot.get("session"), slot.get("router_key")) {
        (None, None) => Ok(None),
        (Some(_), None) => Err(format!("`{path}router_key` is missing")),
        (None, Some(_)) => Err(format!("`{path}session` is missing")),
        (Some(session), Some(router_key)) => Ok(Some(Account {
            session: parse_session(session, &format!("{path}session."))?,
            router_key: parse_router_key(router_key, &format!("{path}router_key."))?,
        })),
    }
}

fn parse_session(value: &Value, path: &str) -> Result<Session, String> {
    let object = object(value, path)?;
    reject_unknown_keys(object, path, SESSION_KEYS)?;
    Ok(Session {
        email: string(object, path, "email")?,
        subject: string(object, path, "subject")?,
        access_token: string(object, path, "access_token")?,
        access_token_expires_at: timestamp(object, path, "access_token_expires_at")?,
        refresh_token: string(object, path, "refresh_token")?,
        refresh_token_expires_at: timestamp(object, path, "refresh_token_expires_at")?,
        signed_in_at: timestamp(object, path, "signed_in_at")?,
    })
}

fn parse_router_key(value: &Value, path: &str) -> Result<RouterKey, String> {
    let object = object(value, path)?;
    reject_unknown_keys(object, path, ROUTER_KEY_KEYS)?;
    Ok(RouterKey {
        api_key: string(object, path, "api_key")?,
        subject: string(object, path, "subject")?,
        saved_at: timestamp(object, path, "saved_at")?,
    })
}

fn object<'a>(value: &'a Value, path: &str) -> Result<&'a Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| format!("`{}` must be an object", path.trim_end_matches('.')))
}

fn reject_unknown_keys(
    object: &Map<String, Value>,
    path: &str,
    known: &[&str],
) -> Result<(), String> {
    match object.keys().find(|key| !known.contains(&key.as_str())) {
        Some(key) => Err(format!("unknown key `{path}{key}`")),
        None => Ok(()),
    }
}

fn string(object: &Map<String, Value>, path: &str, key: &str) -> Result<String, String> {
    match object.get(key) {
        None => Err(format!("`{path}{key}` is missing")),
        Some(Value::String(value)) if !value.is_empty() => Ok(value.clone()),
        Some(_) => Err(format!("`{path}{key}` must be a non-empty string")),
    }
}

fn timestamp(object: &Map<String, Value>, path: &str, key: &str) -> Result<i64, String> {
    match object.get(key) {
        None => Err(format!("`{path}{key}` is missing")),
        Some(Value::String(value)) => OffsetDateTime::parse(value, &Rfc3339)
            .map(OffsetDateTime::unix_timestamp)
            .map_err(|_| format!("`{path}{key}` must be an RFC 3339 timestamp")),
        Some(_) => Err(format!("`{path}{key}` must be an RFC 3339 timestamp")),
    }
}

fn insert_account(object: &mut Map<String, Value>, account: &Account) -> io::Result<()> {
    let session = &account.session;
    let router_key = &account.router_key;
    object.insert(
        "session".to_owned(),
        serde_json::json!({
            "email": session.email,
            "subject": session.subject,
            "access_token": session.access_token,
            "access_token_expires_at": format_timestamp(session.access_token_expires_at)?,
            "refresh_token": session.refresh_token,
            "refresh_token_expires_at": format_timestamp(session.refresh_token_expires_at)?,
            "signed_in_at": format_timestamp(session.signed_in_at)?,
        }),
    );
    object.insert(
        "router_key".to_owned(),
        serde_json::json!({
            "api_key": router_key.api_key,
            "subject": router_key.subject,
            "saved_at": format_timestamp(router_key.saved_at)?,
        }),
    );
    Ok(())
}

/// Sign `slot` in with `api_key` as its router key, for tests elsewhere in the
/// crate that need a stored key.
#[cfg(test)]
pub(crate) fn save_test_router_key(home: &Path, slot: Slot<'_>, api_key: &str) {
    let mut credentials = load(home).expect("load test credentials");
    credentials.set_account(
        slot,
        Account {
            session: Session {
                email: "you@example.com".to_owned(),
                subject: "uid_test".to_owned(),
                access_token: "test-access-token".to_owned(),
                access_token_expires_at: 4_102_444_800,
                refresh_token: "test-refresh-token".to_owned(),
                refresh_token_expires_at: 4_102_444_800,
                signed_in_at: 1_794_477_600,
            },
            router_key: RouterKey {
                api_key: api_key.to_owned(),
                subject: "uid_test".to_owned(),
                saved_at: 1_794_477_600,
            },
        },
    );
    save(home, &credentials).expect("save test credentials");
}

#[cfg(test)]
mod tests {
    use super::*;

    const ACCESS_TOKEN: &str = "access-secret-value";
    const REFRESH_TOKEN: &str = "refresh-secret-value";
    const API_KEY: &str = "rlk-secret-value";

    fn session_json() -> Value {
        serde_json::json!({
            "email": "you@example.com",
            "subject": "uid_abc123",
            "access_token": ACCESS_TOKEN,
            "access_token_expires_at": "2026-11-12T11:00:00Z",
            "refresh_token": REFRESH_TOKEN,
            "refresh_token_expires_at": "2026-12-12T10:00:00Z",
            "signed_in_at": "2026-11-12T10:00:00Z"
        })
    }

    fn router_key_json() -> Value {
        serde_json::json!({
            "api_key": API_KEY,
            "subject": "uid_abc123",
            "saved_at": "2026-11-12T10:00:05Z"
        })
    }

    fn valid_file() -> Value {
        serde_json::json!({
            "session": session_json(),
            "router_key": router_key_json(),
            "environments": {
                "dev": { "session": session_json(), "router_key": router_key_json() }
            }
        })
    }

    fn sample_account() -> Account {
        Account {
            session: Session {
                email: "you@example.com".to_owned(),
                subject: "uid_abc123".to_owned(),
                access_token: ACCESS_TOKEN.to_owned(),
                access_token_expires_at: 1_794_481_200,
                refresh_token: REFRESH_TOKEN.to_owned(),
                refresh_token_expires_at: 1_797_069_600,
                signed_in_at: 1_794_477_600,
            },
            router_key: RouterKey {
                api_key: API_KEY.to_owned(),
                subject: "uid_abc123".to_owned(),
                saved_at: 1_794_477_605,
            },
        }
    }

    /// Parse `value` and return the load error, asserting it names no secret.
    fn reason(value: &Value) -> String {
        let reason = parse(&value.to_string()).expect_err("invalid credentials");
        for secret in [ACCESS_TOKEN, REFRESH_TOKEN, API_KEY] {
            assert!(!reason.contains(secret), "{reason}");
        }
        reason
    }

    fn temp_home(name: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rayline-credentials-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn parses_the_documented_shape() {
        let credentials = parse(&valid_file().to_string()).expect("valid");

        assert_eq!(credentials.account(None), Some(&sample_account()));
        assert_eq!(credentials.account(Some("dev")), Some(&sample_account()));
        assert_eq!(credentials.account(Some("staging")), None);
    }

    #[test]
    fn empty_object_is_signed_out_everywhere() {
        let credentials = parse("{}").expect("valid");

        assert_eq!(credentials, Credentials::default());
    }

    #[test]
    fn missing_file_is_signed_out_everywhere() {
        let home = temp_home("missing");

        assert_eq!(load(&home).expect("missing"), Credentials::default());
    }

    #[test]
    fn save_then_load_round_trips() {
        let home = temp_home("round-trip");
        let mut credentials = Credentials::default();
        credentials.set_account(None, sample_account());
        credentials.set_account(Some("dev"), sample_account());

        save(&home, &credentials).expect("save");
        let written: Value =
            serde_json::from_str(&fs::read_to_string(credentials_file(&home)).expect("read"))
                .expect("json");

        assert_eq!(written, valid_file());
        assert_eq!(load(&home).expect("load"), credentials);
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn removing_the_last_account_writes_an_empty_object() {
        let home = temp_home("remove");
        let mut credentials = Credentials::default();
        credentials.set_account(None, sample_account());
        assert_eq!(credentials.remove_account(None), Some(sample_account()));

        save(&home, &credentials).expect("save");

        assert_eq!(
            fs::read_to_string(credentials_file(&home)).expect("read"),
            "{}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn load_error_names_the_file_and_the_login_command() {
        let home = temp_home("load-error");
        let path = credentials_file(&home);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        fs::write(&path, "{\"version\": 2}").expect("write");

        let message = load(&home).expect_err("invalid").to_string();

        assert!(message.contains(&path.display().to_string()), "{message}");
        assert!(message.contains("unknown key `version`"), "{message}");
        assert!(
            message.ends_with(&format!("Run: {} auth login", crate::CLI_BIN)),
            "{message}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn rejects_a_file_that_is_not_json() {
        assert_eq!(
            parse("{\"session\": ").expect_err("not json"),
            "not valid JSON (line 1, column 12)"
        );
        assert!(parse("").expect_err("empty").starts_with("not valid JSON"));
    }

    #[test]
    fn rejects_a_repeated_key_naming_it() {
        let session = session_json().to_string();
        let router_key = router_key_json().to_string();
        let repeated_slot = format!(
            r#"{{"session": {session}, "router_key": {router_key}, "session": {session}}}"#
        );
        let repeated_field = format!(
            r#"{{"environments": {{"dev": {{"session": {session}, "router_key": {{"api_key": "{API_KEY}", "api_key": "{API_KEY}", "subject": "uid_abc123", "saved_at": "2026-11-12T10:00:05Z"}}}}}}}}"#
        );
        for (text, key) in [
            (repeated_slot, "session"),
            (repeated_field, "environments.dev.router_key.api_key"),
        ] {
            let reason = parse(&text).expect_err("a repeated key");
            assert_eq!(reason, format!("`{key}` appears more than once"));
            assert!(!reason.contains(API_KEY), "{reason}");
        }
    }

    #[test]
    fn rejects_a_top_level_that_is_not_an_object() {
        assert_eq!(
            reason(&serde_json::json!([])),
            "the top level must be an object"
        );
    }

    #[test]
    fn rejects_unknown_keys_at_every_level() {
        let cases = [
            ("", "version"),
            ("", "extra"),
            ("session", "extra"),
            ("session", "accessToken"),
            ("router_key", "apiKey"),
        ];
        for (object, key) in cases {
            let mut file = valid_file();
            let target = if object.is_empty() {
                &mut file
            } else {
                &mut file[object]
            };
            target[key] = Value::from(1);
            let path = if object.is_empty() {
                key.to_owned()
            } else {
                format!("{object}.{key}")
            };
            assert_eq!(reason(&file), format!("unknown key `{path}`"));
        }

        let mut file = valid_file();
        file["environments"]["dev"]["version"] = Value::from(1);
        assert_eq!(reason(&file), "unknown key `environments.dev.version`");
        let mut file = valid_file();
        file["environments"]["dev"]["session"]["extra"] = Value::from(1);
        assert_eq!(
            reason(&file),
            "unknown key `environments.dev.session.extra`"
        );
    }

    #[test]
    fn rejects_half_a_signed_in_environment() {
        for (present, missing) in [("session", "router_key"), ("router_key", "session")] {
            let mut file = valid_file();
            file.as_object_mut().expect("object").remove(missing);
            assert_eq!(reason(&file), format!("`{missing}` is missing"));
            let _ = present;

            let mut file = valid_file();
            file["environments"]["dev"]
                .as_object_mut()
                .expect("object")
                .remove(missing);
            assert_eq!(
                reason(&file),
                format!("`environments.dev.{missing}` is missing")
            );
        }
    }

    #[test]
    fn rejects_objects_of_the_wrong_type() {
        for (key, value) in [
            ("session", Value::from("x")),
            ("router_key", Value::Null),
            ("environments", Value::from(1)),
        ] {
            let mut file = valid_file();
            file[key] = value;
            assert_eq!(reason(&file), format!("`{key}` must be an object"));
        }
        let mut file = valid_file();
        file["environments"]["dev"] = Value::from(true);
        assert_eq!(reason(&file), "`environments.dev` must be an object");
        let mut file = valid_file();
        file["environments"]["dev"]["session"] = Value::Array(Vec::new());
        assert_eq!(
            reason(&file),
            "`environments.dev.session` must be an object"
        );
    }

    #[test]
    fn rejects_an_empty_environment_entry() {
        let mut file = valid_file();
        file["environments"]["dev"] = serde_json::json!({});

        assert_eq!(reason(&file), "`environments.dev` is missing `session`");
    }

    #[test]
    fn rejects_invalid_environment_names() {
        for name in ["", "dev env", "dev/x"] {
            let mut file = valid_file();
            file["environments"][name] = file["environments"]["dev"].clone();
            assert_eq!(
                reason(&file),
                format!("`environments.{name}` is not a valid environment name")
            );
        }
    }

    #[test]
    fn rejects_every_missing_field() {
        for (object, keys) in [("session", SESSION_KEYS), ("router_key", ROUTER_KEY_KEYS)] {
            for key in keys {
                let mut file = valid_file();
                file[object].as_object_mut().expect("object").remove(*key);
                assert_eq!(reason(&file), format!("`{object}.{key}` is missing"));

                let mut file = valid_file();
                file["environments"]["dev"][object]
                    .as_object_mut()
                    .expect("object")
                    .remove(*key);
                assert_eq!(
                    reason(&file),
                    format!("`environments.dev.{object}.{key}` is missing")
                );
            }
        }
    }

    #[test]
    fn rejects_empty_or_non_string_text_fields() {
        let text_fields = [
            ("session", "email"),
            ("session", "subject"),
            ("session", "access_token"),
            ("session", "refresh_token"),
            ("router_key", "api_key"),
            ("router_key", "subject"),
        ];
        for (object, key) in text_fields {
            for value in [Value::from(""), Value::from(5), Value::Null] {
                let mut file = valid_file();
                file[object][key] = value;
                assert_eq!(
                    reason(&file),
                    format!("`{object}.{key}` must be a non-empty string")
                );
            }
        }
    }

    #[test]
    fn rejects_invalid_timestamps() {
        let timestamp_fields = [
            ("session", "access_token_expires_at"),
            ("session", "refresh_token_expires_at"),
            ("session", "signed_in_at"),
            ("router_key", "saved_at"),
        ];
        for (object, key) in timestamp_fields {
            for value in [
                Value::from(""),
                Value::from("2026-11-12"),
                Value::from("2026-11-12 11:00:00"),
                Value::from(1_794_481_200_000_i64),
                Value::from(ACCESS_TOKEN),
            ] {
                let mut file = valid_file();
                file[object][key] = value;
                assert_eq!(
                    reason(&file),
                    format!("`{object}.{key}` must be an RFC 3339 timestamp")
                );
            }
        }
    }

    #[test]
    fn accepts_rfc3339_offsets() {
        let mut file = valid_file();
        file["session"]["signed_in_at"] = Value::from("2026-11-12T11:00:00+01:00");

        let credentials = parse(&file.to_string()).expect("valid");

        assert_eq!(
            credentials
                .account(None)
                .expect("prod")
                .session
                .signed_in_at,
            1_794_477_600
        );
    }
}
