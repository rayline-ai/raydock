use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use fs2::FileExt;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;
use zeroize::{Zeroize, Zeroizing};

const CREDENTIAL_FILE_NAME: &str = ".credentials.json";
const CREDENTIAL_LOCK_FILE_NAME: &str = ".credentials.json.rayline.lock";
const MAX_CREDENTIAL_BYTES: u64 = 1024 * 1024;
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_SERVICE_PREFIX: &str = "Claude Code-credentials-";
#[cfg(target_os = "macos")]
const CLAUDE_LEGACY_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[derive(Clone)]
pub struct SecretString(Zeroizing<String>);

impl SecretString {
    pub fn new(value: impl Into<String>) -> Self {
        Self(Zeroizing::new(value.into()))
    }

    pub fn expose(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for SecretString {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SecretString([REDACTED])")
    }
}

impl PartialEq for SecretString {
    fn eq(&self, other: &Self) -> bool {
        self.expose() == other.expose()
    }
}

impl Eq for SecretString {}

#[derive(Clone, Debug)]
pub struct CredentialStore {
    config_dir: PathBuf,
    #[cfg(target_os = "macos")]
    allow_legacy_keychain: bool,
}

impl CredentialStore {
    pub fn new(config_dir: impl Into<PathBuf>) -> Result<Self, CredentialError> {
        let requested = config_dir.into();
        if !requested.is_dir() {
            return Err(CredentialError::ConfigDirMissing(requested));
        }
        let config_dir = fs::canonicalize(&requested).map_err(|source| {
            CredentialError::CanonicalizeConfigDir {
                path: requested.clone(),
                source,
            }
        })?;
        #[cfg(target_os = "macos")]
        let allow_legacy_keychain = std::env::var_os("HOME")
            .map(PathBuf::from)
            .and_then(|home| home.join(".claude").canonicalize().ok())
            .is_some_and(|default| default == config_dir);
        Ok(Self {
            config_dir,
            #[cfg(target_os = "macos")]
            allow_legacy_keychain,
        })
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn load(&self) -> Result<CredentialDocument, CredentialError> {
        let _lock = self.acquire_lock()?;

        #[cfg(target_os = "macos")]
        {
            if let Some(document) = self.load_keychain()? {
                return Ok(document);
            }
        }

        let path = self.config_dir.join(CREDENTIAL_FILE_NAME);
        if path.is_file() {
            return self.load_file(&path);
        }

        Err(CredentialError::CredentialsNotFound {
            config_dir: self.config_dir.clone(),
        })
    }

    pub fn save_if_unchanged(
        &self,
        document: &mut CredentialDocument,
    ) -> Result<(), CredentialError> {
        let _lock = self.acquire_lock()?;
        let current = self.read_origin(&document.origin)?;
        let actual_version = credential_fingerprint(&current);
        if actual_version != document.version {
            return Err(CredentialError::ConcurrentUpdate {
                config_dir: self.config_dir.clone(),
            });
        }

        let serialized = serde_json::to_vec(&document.value).map_err(CredentialError::Serialize)?;
        match &document.origin {
            CredentialOrigin::File(path) => write_file_atomic(path, &serialized)?,
            #[cfg(target_os = "macos")]
            CredentialOrigin::Keychain { service, account } => {
                keychain::write_password(service, account, &serialized)
                    .map_err(CredentialError::KeychainWrite)?;
            }
        }
        document.version = credential_fingerprint(&serialized);
        Ok(())
    }

    fn acquire_lock(&self) -> Result<File, CredentialError> {
        let path = self.config_dir.join(CREDENTIAL_LOCK_FILE_NAME);
        let mut options = OpenOptions::new();
        options.create(true).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(&path)
            .map_err(|source| CredentialError::OpenLock {
                path: path.clone(),
                source,
            })?;
        set_private_open_file_permissions(&file, &path)?;
        file.lock_exclusive()
            .map_err(|source| CredentialError::Lock { path, source })?;
        Ok(file)
    }

    fn load_file(&self, path: &Path) -> Result<CredentialDocument, CredentialError> {
        let bytes = read_credential_file(path)?;
        CredentialDocument::parse(bytes, CredentialOrigin::File(path.to_owned()))
    }

    fn read_origin(&self, origin: &CredentialOrigin) -> Result<Vec<u8>, CredentialError> {
        match origin {
            CredentialOrigin::File(path) => read_credential_file(path),
            #[cfg(target_os = "macos")]
            CredentialOrigin::Keychain { service, account } => {
                keychain::read_password(service, account)
                    .map_err(CredentialError::KeychainRead)?
                    .ok_or_else(|| {
                        CredentialError::KeychainRead(format!("item {service:?} not found"))
                    })
            }
        }
    }

    #[cfg(target_os = "macos")]
    fn load_keychain(&self) -> Result<Option<CredentialDocument>, CredentialError> {
        let account = std::env::var("USER").map_err(|_| CredentialError::UsernameUnavailable)?;
        let mut services = vec![keychain_service_for_dir(&self.config_dir)];
        if self.allow_legacy_keychain {
            services.push(CLAUDE_LEGACY_KEYCHAIN_SERVICE.to_owned());
        }

        for service in services {
            let Ok(Some(bytes)) = keychain::read_password(&service, &account) else {
                continue;
            };
            let origin = CredentialOrigin::Keychain {
                service,
                account: account.clone(),
            };
            return CredentialDocument::parse(bytes, origin).map(Some);
        }
        Ok(None)
    }
}

pub struct CredentialDocument {
    value: Value,
    origin: CredentialOrigin,
    version: String,
}

impl CredentialDocument {
    fn parse(bytes: Vec<u8>, origin: CredentialOrigin) -> Result<Self, CredentialError> {
        if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
            return Err(CredentialError::CredentialPayloadTooLarge);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(CredentialError::InvalidJson)?;
        let document = Self {
            value,
            origin,
            version: credential_fingerprint(&bytes),
        };
        document.oauth_object()?;
        document.access_token()?;
        Ok(document)
    }

    pub fn access_token(&self) -> Result<SecretString, CredentialError> {
        self.oauth_string("accessToken")
            .map(|value| SecretString::new(value.to_owned()))
    }

    pub fn refresh_token(&self) -> Result<SecretString, CredentialError> {
        self.oauth_string("refreshToken")
            .map(|value| SecretString::new(value.to_owned()))
    }

    pub fn expires_at_unix_ms(&self) -> Result<i64, CredentialError> {
        self.oauth_object()?
            .get("expiresAt")
            .and_then(Value::as_i64)
            .ok_or(CredentialError::MissingOAuthField("expiresAt"))
    }

    pub fn scopes(&self) -> Vec<String> {
        self.oauth_object()
            .ok()
            .and_then(|oauth| oauth.get("scopes"))
            .and_then(Value::as_array)
            .map(|scopes| {
                scopes
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn subscription_type(&self) -> Option<&str> {
        self.oauth_object().ok()?.get("subscriptionType")?.as_str()
    }

    pub(crate) fn has_same_version(&self, other: &Self) -> bool {
        self.version == other.version
    }

    pub(crate) fn apply_refresh(
        &mut self,
        access_token: String,
        refresh_token: Option<String>,
        expires_at_unix_ms: i64,
        refresh_token_expires_at_unix_ms: Option<i64>,
        scopes: Option<Vec<String>>,
    ) -> Result<(), CredentialError> {
        let oauth = self.oauth_object_mut()?;
        oauth.insert("accessToken".to_owned(), Value::String(access_token));
        if let Some(refresh_token) = refresh_token {
            oauth.insert("refreshToken".to_owned(), Value::String(refresh_token));
        }
        oauth.insert(
            "expiresAt".to_owned(),
            Value::Number(expires_at_unix_ms.into()),
        );
        if let Some(expires_at) = refresh_token_expires_at_unix_ms {
            oauth.insert(
                "refreshTokenExpiresAt".to_owned(),
                Value::Number(expires_at.into()),
            );
        }
        if let Some(scopes) = scopes {
            oauth.insert(
                "scopes".to_owned(),
                Value::Array(scopes.into_iter().map(Value::String).collect()),
            );
        }
        Ok(())
    }

    fn oauth_string(&self, field: &'static str) -> Result<&str, CredentialError> {
        self.oauth_object()?
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(CredentialError::MissingOAuthField(field))
    }

    fn oauth_object(&self) -> Result<&Map<String, Value>, CredentialError> {
        self.value
            .get("claudeAiOauth")
            .and_then(Value::as_object)
            .ok_or(CredentialError::MissingOAuthObject)
    }

    fn oauth_object_mut(&mut self) -> Result<&mut Map<String, Value>, CredentialError> {
        self.value
            .get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or(CredentialError::MissingOAuthObject)
    }
}

impl std::fmt::Debug for CredentialDocument {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CredentialDocument")
            .field("origin", &self.origin)
            .field("version", &self.version)
            .field("value", &"[REDACTED]")
            .finish()
    }
}

impl Drop for CredentialDocument {
    fn drop(&mut self) {
        zeroize_json_value(&mut self.value);
    }
}

fn zeroize_json_value(value: &mut Value) {
    match value {
        Value::String(value) => value.zeroize(),
        Value::Array(values) => values.iter_mut().for_each(zeroize_json_value),
        Value::Object(values) => values.values_mut().for_each(zeroize_json_value),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[derive(Clone, Debug)]
enum CredentialOrigin {
    File(PathBuf),
    #[cfg(target_os = "macos")]
    Keychain {
        service: String,
        account: String,
    },
}

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("Claude config directory does not exist: {0}")]
    ConfigDirMissing(PathBuf),
    #[error("failed to resolve Claude config directory {path}: {source}")]
    CanonicalizeConfigDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("no Claude OAuth credential found for {config_dir}")]
    CredentialsNotFound { config_dir: PathBuf },
    #[error("Claude OAuth credential is missing object claudeAiOauth")]
    MissingOAuthObject,
    #[error("Claude OAuth credential is missing field {0}")]
    MissingOAuthField(&'static str),
    #[error("Claude credential JSON is invalid: {0}")]
    InvalidJson(#[source] serde_json::Error),
    #[error("Claude credential payload exceeds the size limit")]
    CredentialPayloadTooLarge,
    #[error("Claude credential file exceeds the size limit: {0}")]
    CredentialFileTooLarge(PathBuf),
    #[error("refusing unsafe Claude credential path: {0}")]
    UnsafeCredentialFile(PathBuf),
    #[error("failed to read Claude credential {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to open credential lock {path}: {source}")]
    OpenLock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to lock credential store {path}: {source}")]
    Lock {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Claude credential changed concurrently for {config_dir}; reload and retry")]
    ConcurrentUpdate { config_dir: PathBuf },
    #[error("failed to serialize Claude credential: {0}")]
    Serialize(#[source] serde_json::Error),
    #[error("failed to write Claude credential {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[cfg(target_os = "macos")]
    #[error("macOS username is unavailable for Claude Keychain lookup")]
    UsernameUnavailable,
    #[cfg(target_os = "macos")]
    #[error("failed to read Claude credential from Keychain: {0}")]
    KeychainRead(String),
    #[cfg(target_os = "macos")]
    #[error("failed to update Claude credential in Keychain: {0}")]
    KeychainWrite(String),
}

fn credential_fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn read_credential_file(path: &Path) -> Result<Vec<u8>, CredentialError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path).map_err(|source| CredentialError::Read {
        path: path.to_owned(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| CredentialError::Read {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(CredentialError::UnsafeCredentialFile(path.to_owned()));
    }
    if metadata.len() > MAX_CREDENTIAL_BYTES {
        return Err(CredentialError::CredentialFileTooLarge(path.to_owned()));
    }
    set_private_open_file_permissions(&file, path)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::take(&mut file, MAX_CREDENTIAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| CredentialError::Read {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() as u64 > MAX_CREDENTIAL_BYTES {
        return Err(CredentialError::CredentialFileTooLarge(path.to_owned()));
    }
    Ok(bytes)
}

#[cfg(target_os = "macos")]
fn keychain_service_for_dir(config_dir: &Path) -> String {
    let digest = format!(
        "{:x}",
        Sha256::digest(config_dir.to_string_lossy().as_bytes())
    );
    format!("{CLAUDE_KEYCHAIN_SERVICE_PREFIX}{}", &digest[..8])
}

fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), CredentialError> {
    let parent = path
        .parent()
        .ok_or_else(|| CredentialError::UnsafeCredentialFile(path.to_owned()))?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp_path = parent.join(format!(
        ".credentials.json.rayline.{}.{}.tmp",
        std::process::id(),
        nonce
    ));
    let write_result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp_path)
            .map_err(|source| CredentialError::Write {
                path: temp_path.clone(),
                source,
            })?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|source| CredentialError::Write {
                path: temp_path.clone(),
                source,
            })?;
        #[cfg(windows)]
        if path.exists() {
            fs::remove_file(path).map_err(|source| CredentialError::Write {
                path: path.to_owned(),
                source,
            })?;
        }
        fs::rename(&temp_path, path).map_err(|source| CredentialError::Write {
            path: path.to_owned(),
            source,
        })?;
        set_private_file_permissions(path)
    })();
    if write_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    write_result
}

fn set_private_file_permissions(path: &Path) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| {
            CredentialError::Write {
                path: path.to_owned(),
                source,
            }
        })?;
    }
    Ok(())
}

fn set_private_open_file_permissions(file: &File, path: &Path) -> Result<(), CredentialError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|source| CredentialError::Write {
                path: path.to_owned(),
                source,
            })?;
    }
    Ok(())
}

/// Keychain access through /usr/bin/security, the same tool Claude Code uses.
///
/// Claude Code creates and maintains its credential items via the `security`
/// CLI, which leaves them in the `apple-tool:` keychain partition. Touching
/// those items through the native SecItem API from a locally built (ad-hoc
/// signed) binary knocks them out of that partition, after which every
/// `security` secret read — i.e. every Claude Code process start — prompts
/// for the login keychain password, and "Always Allow" cannot repair a
/// partition mismatch. Going through the same CLI keeps the items in the
/// partition Claude Code relies on.
#[cfg(target_os = "macos")]
mod keychain {
    use std::ffi::OsString;
    use std::io::Write as _;
    use std::process::{Command, Stdio};

    use zeroize::Zeroizing;

    /// errSecItemNotFound surfaces as exit code 44 from /usr/bin/security.
    const EXIT_ITEM_NOT_FOUND: i32 = 44;

    /// Overridable for tests only; a process able to set our environment can
    /// already read the keychain as this user, so this adds no new exposure.
    fn security_bin() -> OsString {
        std::env::var_os("RAYLINE_SECURITY_CLI")
            .unwrap_or_else(|| OsString::from("/usr/bin/security"))
    }

    /// Read the generic-password secret, or `None` when the item is missing.
    pub(super) fn read_password(service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        let output = Command::new(security_bin())
            .args(["find-generic-password", "-s", service, "-a", account, "-w"])
            .stdin(Stdio::null())
            .output()
            .map_err(|error| format!("failed to run security(1): {error}"))?;
        if output.status.code() == Some(EXIT_ITEM_NOT_FOUND) {
            return Ok(None);
        }
        if !output.status.success() {
            // stderr carries only the OSStatus message, never the secret.
            return Err(format!(
                "security find-generic-password failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(Some(decode_find_output(output.stdout)))
    }

    /// Create or update (`-U`) the generic-password item. The command line is
    /// fed through `security -i` on stdin so the secret never appears in the
    /// process argument list.
    pub(super) fn write_password(
        service: &str,
        account: &str,
        secret: &[u8],
    ) -> Result<(), String> {
        let secret = std::str::from_utf8(secret)
            .map_err(|_| "credential payload is not UTF-8".to_owned())?;
        let mut line = Zeroizing::new(String::with_capacity(secret.len() + 64));
        line.push_str("add-generic-password -U -s ");
        push_quoted(&mut line, service)?;
        line.push_str(" -a ");
        push_quoted(&mut line, account)?;
        line.push_str(" -w ");
        push_quoted(&mut line, secret)?;
        line.push('\n');

        let mut child = Command::new(security_bin())
            .arg("-i")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("failed to run security(1): {error}"))?;
        let stdin_result = child
            .stdin
            .take()
            .ok_or_else(|| "security(1) stdin unavailable".to_owned())
            .and_then(|mut stdin| {
                stdin
                    .write_all(line.as_bytes())
                    .map_err(|error| format!("failed to write to security(1): {error}"))
            });
        let output = child
            .wait_with_output()
            .map_err(|error| format!("failed to wait for security(1): {error}"))?;
        stdin_result?;
        if !output.status.success() {
            return Err(format!(
                "security add-generic-password failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(())
    }

    /// Quote an argument for the `security -i` command parser, which accepts
    /// double-quoted strings with backslash escapes (verified empirically
    /// against macOS 15). Newlines would terminate the command line early, so
    /// they are rejected; serialized JSON never contains raw control bytes.
    pub(super) fn push_quoted(line: &mut String, value: &str) -> Result<(), String> {
        if value.chars().any(|c| c == '\n' || c == '\r') {
            return Err("keychain value must not contain newlines".to_owned());
        }
        line.push('"');
        for c in value.chars() {
            if c == '"' || c == '\\' {
                line.push('\\');
            }
            line.push(c);
        }
        line.push('"');
        Ok(())
    }

    /// `find-generic-password -w` prints printable-ASCII secrets raw and
    /// anything else hex-encoded, each with a trailing newline. A JSON
    /// document always starts with `{` or `[` — not a hex digit — so raw
    /// output is never misread as hex.
    pub(super) fn decode_find_output(mut stdout: Vec<u8>) -> Vec<u8> {
        if stdout.last() == Some(&b'\n') {
            stdout.pop();
        }
        let is_hex =
            !stdout.is_empty() && stdout.len() % 2 == 0 && stdout.iter().all(u8::is_ascii_hexdigit);
        if !is_hex {
            return stdout;
        }
        stdout
            .chunks(2)
            .map(|pair| {
                let hi = (pair[0] as char).to_digit(16).unwrap_or(0) as u8;
                let lo = (pair[1] as char).to_digit(16).unwrap_or(0) as u8;
                (hi << 4) | lo
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential_json(access_token: &str, refresh_token: &str, expires_at: i64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access_token,
                "refreshToken": refresh_token,
                "expiresAt": expires_at,
                "scopes": ["user:inference"]
            },
            "preserved": {"value": true}
        }))
        .expect("credential JSON")
    }

    fn write_test_credential(dir: &Path, bytes: &[u8]) {
        let path = dir.join(CREDENTIAL_FILE_NAME);
        fs::write(&path, bytes).expect("write credential");
        set_private_file_permissions(&path).expect("permissions");
    }

    #[test]
    fn file_credentials_round_trip_without_losing_unknown_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_test_credential(dir.path(), &credential_json("access-a", "refresh-a", 1000));
        let store = CredentialStore::new(dir.path()).expect("store");
        let mut document = store.load().expect("load");

        document
            .apply_refresh(
                "access-b".to_owned(),
                Some("refresh-b".to_owned()),
                2000,
                Some(3000),
                Some(vec!["user:inference".to_owned(), "user:profile".to_owned()]),
            )
            .expect("apply refresh");
        store
            .save_if_unchanged(&mut document)
            .expect("save refreshed credential");

        let saved: Value =
            serde_json::from_slice(&fs::read(dir.path().join(CREDENTIAL_FILE_NAME)).expect("read"))
                .expect("saved JSON");
        assert_eq!(saved["claudeAiOauth"]["accessToken"], "access-b");
        assert_eq!(saved["claudeAiOauth"]["refreshToken"], "refresh-b");
        assert_eq!(saved["preserved"]["value"], true);
    }

    #[test]
    fn compare_and_swap_rejects_a_concurrent_credential_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        write_test_credential(dir.path(), &credential_json("access-a", "refresh-a", 1000));
        let store = CredentialStore::new(dir.path()).expect("store");
        let mut document = store.load().expect("load");
        write_test_credential(
            dir.path(),
            &credential_json("access-other", "refresh-other", 1000),
        );

        document
            .apply_refresh("access-b".to_owned(), None, 2000, None, None)
            .expect("apply refresh");
        assert!(matches!(
            store.save_if_unchanged(&mut document),
            Err(CredentialError::ConcurrentUpdate { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn file_credentials_reject_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("source.json");
        fs::write(&source, credential_json("access-a", "refresh-a", 1000)).expect("source");
        symlink(&source, dir.path().join(CREDENTIAL_FILE_NAME)).expect("symlink");

        let store = CredentialStore::new(dir.path()).expect("store");
        assert!(matches!(
            store.load(),
            Err(CredentialError::Read { .. }) | Err(CredentialError::UnsafeCredentialFile(_))
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn non_default_directory_named_claude_cannot_use_legacy_keychain() {
        let parent = tempfile::tempdir().expect("tempdir");
        let dir = parent.path().join(".claude");
        fs::create_dir(&dir).expect("config dir");

        let store = CredentialStore::new(&dir).expect("store");
        assert!(!store.allow_legacy_keychain);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn find_output_decoding_handles_raw_and_hex() {
        let raw: &[u8] = br#"{"v":2}"#;
        assert_eq!(keychain::decode_find_output(b"{\"v\":2}\n".to_vec()), raw);
        // `security find-generic-password -w` hex-encodes non-printable data;
        // 7b2276223a327d is the hex spelling of {"v":2}.
        assert_eq!(
            keychain::decode_find_output(b"7b2276223a327d\n".to_vec()),
            raw
        );
        assert_eq!(
            keychain::decode_find_output(b"7b2276223a327d".to_vec()),
            raw
        );
        assert_eq!(keychain::decode_find_output(Vec::new()), b"");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn security_quoting_escapes_and_rejects_newlines() {
        let quote = |value: &str| {
            let mut line = String::new();
            keychain::push_quoted(&mut line, value).map(|()| line)
        };
        assert_eq!(quote(r#"a"b\c"#).expect("quote"), r#""a\"b\\c""#);
        assert!(quote("a\nb").is_err());
        assert!(quote("a\rb").is_err());
    }

    // Regression for the keychain popup storm: rayline must drive the real
    // items only through the security CLI. One test covers every fake-CLI
    // path so the RAYLINE_SECURITY_CLI override is set exactly once.
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_access_goes_through_the_security_cli() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("security");
        fs::write(
            &fake,
            concat!(
                "#!/bin/sh\n",
                "dir=\"$(dirname \"$0\")\"\n",
                "if [ \"$1\" = \"-i\" ]; then cat > \"$dir/last-stdin\"; exit 0; fi\n",
                "if [ \"$1\" = \"find-generic-password\" ]; then\n",
                "  cat \"$dir/find-output\" 2>/dev/null\n",
                "  exit \"$(cat \"$dir/find-exit\" 2>/dev/null || echo 0)\"\n",
                "fi\n",
                "exit 1\n",
            ),
        )
        .expect("write fake security");
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).expect("chmod");
        // SAFETY: tests in this workspace run single-threaded
        // (`--test-threads=1`), so no other thread reads the environment
        // concurrently with this mutation.
        unsafe { std::env::set_var("RAYLINE_SECURITY_CLI", &fake) };

        let result = (|| {
            // Write path: the secret travels via `security -i` stdin, quoted
            // for its parser — never through the argument list.
            keychain::write_password("svc", "acct", br#"{"k":"a\"b"}"#)?;
            let sent = fs::read_to_string(dir.path().join("last-stdin"))
                .map_err(|error| error.to_string())?;
            let expected = r#"add-generic-password -U -s "svc" -a "acct" -w "{\"k\":\"a\\\"b\"}""#;
            if sent != format!("{expected}\n") {
                return Err(format!("unexpected security -i command: {sent:?}"));
            }

            // Read path: raw output round-trips.
            fs::write(dir.path().join("find-output"), b"{\"v\":2}\n")
                .map_err(|error| error.to_string())?;
            if keychain::read_password("svc", "acct")? != Some(br#"{"v":2}"#.to_vec()) {
                return Err("raw read mismatch".to_owned());
            }

            // Missing item (exit 44) maps to None, not an error.
            fs::write(dir.path().join("find-exit"), "44").map_err(|error| error.to_string())?;
            if keychain::read_password("svc", "acct")?.is_some() {
                return Err("missing item should read as None".to_owned());
            }

            // Any other failure surfaces as an error.
            fs::write(dir.path().join("find-exit"), "51").map_err(|error| error.to_string())?;
            if keychain::read_password("svc", "acct").is_ok() {
                return Err("failing read should surface an error".to_owned());
            }
            Ok(())
        })();

        // SAFETY: same single-threaded test environment as the set_var above.
        unsafe { std::env::remove_var("RAYLINE_SECURITY_CLI") };
        result.expect("fake security CLI round trip");
    }

    #[test]
    fn secret_debug_output_is_redacted() {
        assert_eq!(
            format!("{:?}", SecretString::new("do-not-print")),
            "SecretString([REDACTED])"
        );
    }
}
