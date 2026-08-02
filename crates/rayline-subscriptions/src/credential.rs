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
                security_framework::passwords::set_generic_password(service, account, &serialized)
                    .map_err(|error| CredentialError::KeychainWrite(error.to_string()))?;
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
                security_framework::passwords::get_generic_password(service, account)
                    .map_err(|error| CredentialError::KeychainRead(error.to_string()))
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
            let Ok(bytes) = security_framework::passwords::get_generic_password(&service, &account)
            else {
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

    #[test]
    fn secret_debug_output_is_redacted() {
        assert_eq!(
            format!("{:?}", SecretString::new("do-not-print")),
            "SecretString([REDACTED])"
        );
    }
}
