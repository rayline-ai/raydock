use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "windows"))]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine;
use rand::Rng;
use serde_json::{Map, Value};
use url::{Host, Url};

use crate::auth_callback_page;

const AUTH_HTTP_TIMEOUT_SECONDS: u64 = 30;
const TOKEN_REFRESH_MARGIN_SECONDS: f64 = 300.0;
const WEB_CALLBACK_TIMEOUT_SECONDS: u64 = 300;
const RAYLINE_SESSION_AUTH_KIND: &str = "rayline_session";
const LEGACY_FIREBASE_AUTH_KIND: &str = "firebase";
const PROD_CLI_AUTH_URL: &str = "https://platform.rayline.ai/cli-auth";
const PROD_ACCOUNT_URL: &str = "https://platform.rayline.ai";
const SECURE_TOKEN_URL: &str = "https://securetoken.googleapis.com/v1/token";
const GOOGLE_DEVICE_CODE_URL: &str = "https://oauth2.googleapis.com/device/code";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const IDENTITY_TOOLKIT_URL: &str =
    "https://identitytoolkit.googleapis.com/v1/accounts:signInWithIdp";
const IDENTITY_TOOLKIT_CUSTOM_TOKEN_URL: &str =
    "https://identitytoolkit.googleapis.com/v1/accounts:signInWithCustomToken";
pub(crate) const PROD_ENV: &str = "prod";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusRequest {
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthTokenRequest {
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLogoutRequest {
    pub env_name: Option<String>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLoginRequest {
    pub env_name: Option<String>,
    pub root_env_explicit: bool,
    pub no_browser: bool,
    pub paste: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeLoginRequest {
    pub env_name: Option<String>,
    pub auth_token: Option<String>,
    pub root_env_explicit: bool,
    pub name: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeKeyRequest {
    pub env_name: Option<String>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ClaudeLogoutRequest {
    pub env_name: Option<String>,
    pub root_env_explicit: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthTokenOutcome {
    Available(String),
    NotLoggedIn,
}

#[derive(Debug)]
pub enum AuthTokenError {
    RefreshFailed(String),
    WriteFailed(io::Error),
}

#[derive(Debug)]
pub enum AuthLoginError {
    InvalidPaste(String),
    LoginFailed(String),
    RefreshFailed(AuthTokenError),
    UnknownEnvironment(String),
    WriteFailed(io::Error),
}

#[derive(Debug)]
pub enum ClaudeLoginError {
    Auth(AuthTokenError),
    MintFailed(String),
    NotLoggedIn(String),
    WriteFailed(io::Error),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HostedEnvironment {
    pub name: String,
    pub credential_key: String,
    pub router_url: String,
    pub cli_auth_url: String,
    pub account_url: Option<String>,
    pub auth_kind: HostedAuthKind,
    pub firebase_api_key: Option<String>,
    pub google_device_client_id: Option<String>,
    pub google_device_client_secret: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostedAuthKind {
    RaylineSession,
    FirebaseLegacy,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum HostedEnvironmentError {
    InvalidName(String),
    Unknown {
        env_name: String,
        settings_path: Option<PathBuf>,
    },
    MissingField {
        env_name: String,
        settings_path: PathBuf,
        field: &'static str,
    },
    InvalidUrl {
        env_name: String,
        settings_path: PathBuf,
        field: &'static str,
        value: String,
    },
    InvalidAuthKind {
        env_name: String,
        settings_path: PathBuf,
        value: String,
    },
}

impl std::fmt::Display for AuthTokenError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RefreshFailed(message) => formatter.write_str(message),
            Self::WriteFailed(error) => write!(formatter, "failed to update credentials: {error}"),
        }
    }
}

impl std::error::Error for AuthTokenError {}

impl std::fmt::Display for AuthLoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPaste(message) => formatter.write_str(message),
            Self::LoginFailed(message) => formatter.write_str(message),
            Self::RefreshFailed(error) => error.fmt(formatter),
            Self::UnknownEnvironment(env_name) => {
                write!(formatter, "Unknown environment: {env_name}")
            }
            Self::WriteFailed(error) => write!(formatter, "failed to update credentials: {error}"),
        }
    }
}

impl std::error::Error for AuthLoginError {}

impl std::fmt::Display for ClaudeLoginError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(error) => error.fmt(formatter),
            Self::MintFailed(message) => formatter.write_str(message),
            Self::NotLoggedIn(env_name) => {
                write!(
                    formatter,
                    "Not logged in to {env_name}. Run: {} auth login",
                    crate::CLI_BIN
                )
            }
            Self::WriteFailed(error) => write!(formatter, "failed to update credentials: {error}"),
        }
    }
}

impl std::error::Error for ClaudeLoginError {}

impl From<AuthTokenError> for ClaudeLoginError {
    fn from(error: AuthTokenError) -> Self {
        Self::Auth(error)
    }
}

impl std::fmt::Display for HostedEnvironmentError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(env_name) => write!(
                formatter,
                "Invalid Rayline environment '{env_name}'. Use only ASCII letters, digits, '_' or '-'."
            ),
            Self::Unknown {
                env_name,
                settings_path,
            } => write!(
                formatter,
                "Unknown Rayline environment '{env_name}'. Define it in {}.",
                settings_path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "~/.config/rayline/settings.json".to_owned())
            ),
            Self::MissingField {
                env_name,
                settings_path,
                field,
            } => write!(
                formatter,
                "Rayline environment '{env_name}' in {} is missing required field '{field}'.",
                settings_path.display()
            ),
            Self::InvalidUrl {
                env_name,
                settings_path,
                field,
                value,
            } => write!(
                formatter,
                "Rayline environment '{env_name}' in {} has invalid URL field '{field}': {value}",
                settings_path.display()
            ),
            Self::InvalidAuthKind {
                env_name,
                settings_path,
                value,
            } => write!(
                formatter,
                "Rayline environment '{env_name}' in {} has invalid auth_kind: {value}",
                settings_path.display()
            ),
        }
    }
}

impl std::error::Error for HostedEnvironmentError {}

impl StatusRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl AuthTokenRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl AuthLogoutRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl AuthLoginRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl ClaudeLoginRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl ClaudeKeyRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

impl ClaudeLogoutRequest {
    pub fn should_forward_for_invalid_envvar(&self) -> bool {
        should_forward_for_invalid_envvar(self.root_env_explicit)
    }
}

pub fn is_valid_root_env(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

pub(crate) fn render_status(request: &StatusRequest) -> Result<String, HostedEnvironmentError> {
    let home = dirs::home_dir();
    let env_name = resolve_env(request.env_name.as_deref(), home.as_deref());
    let hosted = resolve_hosted_environment(&env_name, home.as_deref())?;
    let env_token = env_auth_token();
    let token = request
        .auth_token
        .as_deref()
        .or(env_token.as_ref().map(|(token, _source)| token.as_str()));
    let token_source = if request
        .auth_token
        .as_ref()
        .is_some_and(|token| !token.is_empty())
    {
        Some("--auth-token")
    } else {
        env_token.as_ref().map(|(_token, source)| *source)
    };
    let now = unix_now_secs();

    match home {
        Some(home) => Ok(render_status_from_home_with_source(
            &env_name,
            &home,
            token,
            token_source,
            Some(hosted.auth_kind),
            now,
        )),
        None => Ok(format!("Not logged in. Run: {}\n", auth_command())),
    }
}

pub async fn resolve_auth_token(
    request: &AuthTokenRequest,
) -> Result<AuthTokenOutcome, AuthTokenError> {
    if let Some(value) = request.auth_token.as_deref() {
        return Ok(AuthTokenOutcome::Available(value.to_owned()));
    }
    if let Some((value, _source)) = env_auth_token() {
        return Ok(AuthTokenOutcome::Available(value));
    }

    let home = dirs::home_dir();
    let env_name = resolve_env(request.env_name.as_deref(), home.as_deref());
    let Some(home) = home else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };

    resolve_auth_token_from_home(&env_name, &home, unix_now_secs()).await
}

pub async fn resolve_auth_token_from_home(
    env_name: &str,
    home: &Path,
    now: f64,
) -> Result<AuthTokenOutcome, AuthTokenError> {
    resolve_auth_token_from_home_with_endpoint(env_name, home, now, SECURE_TOKEN_URL).await
}

pub async fn resolve_auth_token_from_home_with_endpoint(
    env_name: &str,
    home: &Path,
    now: f64,
    secure_token_url: &str,
) -> Result<AuthTokenOutcome, AuthTokenError> {
    let hosted = resolve_hosted_environment(env_name, Some(home))
        .map_err(|error| AuthTokenError::RefreshFailed(error.to_string()))?;
    match hosted.auth_kind {
        HostedAuthKind::RaylineSession => {
            let router_url = hosted.router_url.clone();
            resolve_session_token_from_home_with_refresher(
                env_name,
                home,
                now,
                move |refresh_token, _env_name| async move {
                    refresh_rayline_session(&router_url, &refresh_token).await
                },
            )
            .await
        }
        HostedAuthKind::FirebaseLegacy => {
            let firebase_api_key = hosted.firebase_api_key.clone().ok_or_else(|| {
                AuthTokenError::RefreshFailed(format!(
                    "Rayline environment '{env_name}' is missing required field 'firebase_api_key'."
                ))
            })?;
            resolve_legacy_firebase_token_from_home_with_refresher(
                env_name,
                home,
                now,
                move |refresh_token, _env_name| async move {
                    refresh_firebase_token(&refresh_token, &firebase_api_key, secure_token_url)
                        .await
                },
            )
            .await
        }
    }
}

async fn resolve_session_token_from_home_with_refresher<F, Fut>(
    env_name: &str,
    home: &Path,
    now: f64,
    refresh: F,
) -> Result<AuthTokenOutcome, AuthTokenError>
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<SessionToken, AuthTokenError>>,
{
    let Some(credentials) = read_json(&credentials_file(home)) else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };
    let Some(env_data) = credentials
        .get("environments")
        .and_then(Value::as_object)
        .and_then(|envs| envs.get(env_name))
        .and_then(Value::as_object)
    else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };

    if env_data
        .get("authKind")
        .and_then(value_as_str)
        .is_some_and(|kind| kind != RAYLINE_SESSION_AUTH_KIND)
    {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    }

    let access_token = env_data
        .get("accessToken")
        .and_then(value_as_str)
        .unwrap_or_default();
    let access_expires_at = env_data
        .get("accessTokenExpiresAtMs")
        .and_then(value_as_f64)
        .map(|value| value / 1000.0)
        .unwrap_or(0.0);

    if !access_token.is_empty() && access_expires_at - now > TOKEN_REFRESH_MARGIN_SECONDS {
        return Ok(AuthTokenOutcome::Available(access_token.to_owned()));
    }

    let Some(refresh_token) = env_data.get("refreshToken").and_then(value_as_str) else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };
    if refresh_token.is_empty() {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    }

    let stored_subject = env_data
        .get("subject")
        .and_then(value_as_str)
        .unwrap_or_default()
        .to_owned();
    let stored_email = env_data
        .get("email")
        .and_then(value_as_str)
        .unwrap_or_default()
        .to_owned();

    let mut refreshed = refresh(refresh_token.to_owned(), env_name.to_owned()).await?;
    if refreshed.subject.is_empty() {
        refreshed.subject = stored_subject;
    }
    if refreshed.email.is_empty() {
        refreshed.email = stored_email;
    }
    if refreshed.subject.is_empty() {
        return Err(AuthTokenError::RefreshFailed(
            "Token refresh failed: missing stored subject".to_owned(),
        ));
    }
    save_session_credentials_from_home(env_name, home, &refreshed, now)
        .map_err(AuthTokenError::WriteFailed)?;

    Ok(AuthTokenOutcome::Available(refreshed.access_token))
}

async fn resolve_legacy_firebase_token_from_home_with_refresher<F, Fut>(
    env_name: &str,
    home: &Path,
    now: f64,
    refresh: F,
) -> Result<AuthTokenOutcome, AuthTokenError>
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<RefreshedToken, AuthTokenError>>,
{
    let Some(credentials) = read_json(&credentials_file(home)) else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };
    let Some(env_data) = credentials
        .get("environments")
        .and_then(Value::as_object)
        .and_then(|envs| envs.get(env_name))
        .and_then(Value::as_object)
    else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };

    let Some(refresh_token) = env_data.get("refresh_token").and_then(value_as_str) else {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    };
    if refresh_token.is_empty() {
        return Ok(AuthTokenOutcome::NotLoggedIn);
    }

    let id_token = env_data
        .get("id_token")
        .and_then(value_as_str)
        .unwrap_or_default();
    let expires_at = env_data
        .get("id_token_expires_at")
        .and_then(value_as_f64)
        .unwrap_or(0.0);

    if !id_token.is_empty() && expires_at - now > TOKEN_REFRESH_MARGIN_SECONDS {
        return Ok(AuthTokenOutcome::Available(id_token.to_owned()));
    }

    let refreshed = refresh(refresh_token.to_owned(), env_name.to_owned()).await?;
    let mut updated = credentials;
    let env_data = updated
        .get_mut("environments")
        .and_then(Value::as_object_mut)
        .and_then(|envs| envs.get_mut(env_name))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| {
            AuthTokenError::RefreshFailed(
                "Stored credentials changed while refreshing token".to_owned(),
            )
        })?;

    env_data.insert(
        "id_token".to_owned(),
        Value::String(refreshed.id_token.clone()),
    );
    env_data.insert(
        "refresh_token".to_owned(),
        Value::String(refreshed.refresh_token),
    );
    env_data.insert(
        "id_token_expires_at".to_owned(),
        numeric_value(now + refreshed.expires_in as f64),
    );
    write_json_atomic(&credentials_file(home), &updated).map_err(AuthTokenError::WriteFailed)?;

    Ok(AuthTokenOutcome::Available(refreshed.id_token))
}

pub async fn logout(request: &AuthLogoutRequest) -> io::Result<String> {
    let home = dirs::home_dir();
    let env_name = resolve_env(request.env_name.as_deref(), home.as_deref());
    let Some(home) = home else {
        return Ok(format!("Not logged in to {env_name}\n"));
    };

    let revoke_result =
        session_revoke_request(&env_name, &home).map(|(router_url, token)| async move {
            revoke_rayline_session(&router_url, &token).await
        });
    let revoke_error = match revoke_result {
        Some(revoke) => revoke.await.err(),
        None => None,
    };

    let mut message = logout_from_home(&env_name, &home)?;
    if let Some(error) = revoke_error {
        message.push_str(&format!("Remote session revoke failed: {error}\n"));
    }
    Ok(message)
}

pub fn logout_from_home(env_name: &str, home: &Path) -> io::Result<String> {
    let credentials_path = credentials_file(home);
    let Some(mut credentials) = read_json(&credentials_path) else {
        return Ok(format!("Not logged in to {env_name}\n"));
    };
    let removed_login = credentials
        .get_mut("environments")
        .and_then(Value::as_object_mut)
        .is_some_and(|envs| envs.remove(env_name).is_some());
    // The router key minted under this login stays usable after the account
    // credentials are gone; drop it too so a later login as a different
    // account cannot keep routing on the previous account's key.
    let removed_key = credentials
        .get_mut("router_keys")
        .and_then(Value::as_object_mut)
        .is_some_and(|keys| keys.remove(env_name).is_some());
    if !removed_login && !removed_key {
        return Ok(format!("Not logged in to {env_name}\n"));
    }

    write_json_atomic(&credentials_path, &credentials)?;
    let mut message = if removed_login {
        format!("Logged out ({env_name})\n")
    } else {
        format!("Not logged in to {env_name}\n")
    };
    if removed_key {
        message.push_str(&format!("Cleared {env_name} router key.\n"));
    }
    Ok(message)
}

pub async fn auth_login(request: &AuthLoginRequest) -> Result<String, AuthLoginError> {
    let home = dirs::home_dir().ok_or_else(|| {
        AuthLoginError::WriteFailed(io::Error::new(
            io::ErrorKind::NotFound,
            "home directory not found",
        ))
    })?;
    let env_name = resolve_env(request.env_name.as_deref(), Some(&home));
    let hosted = resolve_hosted_environment(&env_name, Some(&home))
        .map_err(|error| AuthLoginError::LoginFailed(error.to_string()))?;

    match hosted.auth_kind {
        HostedAuthKind::RaylineSession => {
            auth_login_session(request, &env_name, &home, &hosted).await
        }
        HostedAuthKind::FirebaseLegacy => {
            auth_login_legacy_firebase(request, &env_name, &home, &hosted).await
        }
    }
}

async fn auth_login_session(
    request: &AuthLoginRequest,
    env_name: &str,
    home: &Path,
    hosted: &HostedEnvironment,
) -> Result<String, AuthLoginError> {
    let session = if request.paste {
        run_session_paste_flow(hosted).await?
    } else if request.no_browser || is_headless() {
        if !request.no_browser {
            eprintln!("  No local browser detected (SSH session). Using device-code login.");
        }
        run_rayline_device_login(hosted).await?
    } else {
        run_web_callback_session_flow(hosted).await?
    };

    let cleared_router_key =
        save_session_credentials_from_home(env_name, home, &session, unix_now_secs())
            .map_err(AuthLoginError::WriteFailed)?;
    let mut message = login_success_message(env_name, &session.email);
    if cleared_router_key {
        message.push_str(&router_key_cleared_note(env_name));
    }
    Ok(message)
}

async fn auth_login_legacy_firebase(
    request: &AuthLoginRequest,
    env_name: &str,
    home: &Path,
    hosted: &HostedEnvironment,
) -> Result<String, AuthLoginError> {
    if request.paste {
        let (refresh_token, fragment_email) = run_legacy_paste_flow(hosted)?;
        let firebase_api_key = firebase_api_key(hosted)?;
        return auth_login_refresh_token_from_home_with_refresher(
            env_name,
            home,
            refresh_token,
            fragment_email,
            unix_now_secs(),
            move |refresh_token, _env_name| async move {
                refresh_firebase_token(&refresh_token, &firebase_api_key, SECURE_TOKEN_URL).await
            },
        )
        .await;
    }

    if request.no_browser || is_headless() {
        if !request.no_browser {
            eprintln!("  No local browser detected (SSH session). Using device-code login.");
        }
        let token = run_legacy_device_login(hosted).await?;
        let cleared_router_key = save_env_credentials_from_home(
            env_name,
            home,
            &token.refreshed,
            &token.email,
            unix_now_secs(),
        )
        .map_err(AuthLoginError::WriteFailed)?;
        let mut message = login_success_message(env_name, &token.email);
        if cleared_router_key {
            message.push_str(&router_key_cleared_note(env_name));
        }
        return Ok(message);
    }

    let (refresh_token, fragment_email) = run_web_callback_legacy_flow(hosted).await?;
    let firebase_api_key = firebase_api_key(hosted)?;
    auth_login_refresh_token_from_home_with_refresher(
        env_name,
        home,
        refresh_token,
        fragment_email,
        unix_now_secs(),
        move |refresh_token, _env_name| async move {
            refresh_firebase_token(&refresh_token, &firebase_api_key, SECURE_TOKEN_URL).await
        },
    )
    .await
}

async fn auth_login_refresh_token_from_home_with_refresher<F, Fut>(
    env_name: &str,
    home: &Path,
    refresh_token: String,
    fragment_email: String,
    now: f64,
    refresh: F,
) -> Result<String, AuthLoginError>
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<RefreshedToken, AuthTokenError>>,
{
    let refreshed = refresh(refresh_token, env_name.to_owned())
        .await
        .map_err(AuthLoginError::RefreshFailed)?;
    let jwt_email = extract_email_from_token(&refreshed.id_token).unwrap_or_default();
    let email = if jwt_email.is_empty() {
        fragment_email
    } else {
        jwt_email
    };
    let cleared_router_key =
        save_env_credentials_from_home(env_name, home, &refreshed, &email, now)
            .map_err(AuthLoginError::WriteFailed)?;
    let mut message = login_success_message(env_name, &email);
    if cleared_router_key {
        message.push_str(&router_key_cleared_note(env_name));
    }
    Ok(message)
}

async fn run_session_paste_flow(
    hosted: &HostedEnvironment,
) -> Result<SessionToken, AuthLoginError> {
    let state = random_state();
    let (code_verifier, code_challenge) = generate_pkce();
    let auth_url = cli_auth_url_with_callback(
        hosted,
        &state,
        Some("http://127.0.0.1/"),
        Some(&code_challenge),
    )?;
    eprintln!();
    eprintln!("  To sign in, open this URL in any browser:");
    eprintln!();
    eprintln!("  {auth_url}");
    eprintln!();
    eprintln!("  After sign-in, copy the final http://127.0.0.1/... URL from");
    eprintln!("  the browser address bar and paste it here.");
    eprintln!();
    eprint!("  Paste the callback URL: ");
    io::stderr().flush().map_err(AuthLoginError::WriteFailed)?;

    let mut pasted = String::new();
    io::stdin()
        .read_line(&mut pasted)
        .map_err(AuthLoginError::WriteFailed)?;
    let code = parse_paste_callback_url(pasted.trim(), &state)?;
    exchange_cli_code_for_session(hosted, &code, &code_verifier).await
}

fn run_legacy_paste_flow(hosted: &HostedEnvironment) -> Result<(String, String), AuthLoginError> {
    let state = random_state();
    let auth_url = cli_auth_url(hosted, &state)?;
    eprintln!();
    eprintln!("  To sign in, open this URL in any browser:");
    eprintln!();
    eprintln!("  {auth_url}");
    eprintln!();
    eprintln!("  Sign in with Google or Okta SSO. When you see the success page,");
    eprintln!("  copy the full URL (including #rt=...) and paste it here.");
    eprintln!();
    eprint!("  Paste the success URL: ");
    io::stderr().flush().map_err(AuthLoginError::WriteFailed)?;

    let mut pasted = String::new();
    io::stdin()
        .read_line(&mut pasted)
        .map_err(AuthLoginError::WriteFailed)?;
    parse_legacy_paste_success_url(pasted.trim(), &state)
}

async fn run_web_callback_session_flow(
    hosted: &HostedEnvironment,
) -> Result<SessionToken, AuthLoginError> {
    let (code, code_verifier) = run_web_callback_code_flow(hosted).await?;
    exchange_cli_code_for_session(hosted, &code, &code_verifier).await
}

async fn run_web_callback_legacy_flow(
    hosted: &HostedEnvironment,
) -> Result<(String, String), AuthLoginError> {
    let (code, code_verifier) = run_web_callback_code_flow(hosted).await?;
    exchange_cli_code_for_legacy_firebase(hosted, &code, &code_verifier).await
}

async fn run_web_callback_code_flow(
    hosted: &HostedEnvironment,
) -> Result<(String, String), AuthLoginError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
        AuthLoginError::LoginFailed(format!("Failed to start local login callback: {error}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!(
                "Failed to read local login callback port: {error}"
            ))
        })?
        .port();
    let callback_url = format!("http://127.0.0.1:{port}/");
    let state = random_state();
    let (code_verifier, code_challenge) = generate_pkce();
    let auth_url =
        cli_auth_url_with_callback(hosted, &state, Some(&callback_url), Some(&code_challenge))?;

    eprintln!("  Opening browser for authentication ({})...", hosted.name);
    eprintln!("  If it doesn't open, visit this URL:\n");
    eprintln!("  {auth_url}\n");
    eprintln!("  Waiting for login (timeout: {WEB_CALLBACK_TIMEOUT_SECONDS}s)...");
    open_browser(&auth_url);

    // The browser navigates the loopback to `/?code=...&state=...`; we receive a
    // one-time, PKCE-bound code (never a credential), then redeem it over TLS.
    let code = tokio::task::spawn_blocking(move || wait_for_callback(listener, &state))
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!("Login callback failed: {error}"))
        })??;

    Ok((code, code_verifier))
}

/// Generate a PKCE (RFC 7636) verifier and its S256 challenge.
/// verifier = base64url(32 random bytes); challenge = base64url(sha256(verifier)).
fn generate_pkce() -> (String, String) {
    use base64::Engine as _;
    use sha2::Digest as _;
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let digest = sha2::Sha256::digest(verifier.as_bytes());
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest);
    (verifier, challenge)
}

async fn exchange_cli_code_for_session(
    hosted: &HostedEnvironment,
    code: &str,
    code_verifier: &str,
) -> Result<SessionToken, AuthLoginError> {
    let url = format!("{}/v1/auth/cli/token", hosted.router_url);
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("CLI code exchange failed: {error}"))
    })?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "code": code,
            "codeVerifier": code_verifier,
            "responseType": RAYLINE_SESSION_AUTH_KIND
        }))
        .send()
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!("CLI code exchange failed: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthLoginError::LoginFailed(http_error_message(
            "CLI code exchange failed",
            status.as_u16(),
            &body,
        )));
    }
    let data: Value = response.json().await.map_err(|error| {
        AuthLoginError::LoginFailed(format!("CLI code exchange failed: {error}"))
    })?;
    parse_session_token_response(&data, "CLI code exchange").map_err(AuthLoginError::LoginFailed)
}

/// Redeem the one-time loopback code at the router's legacy CLI-auth broker,
/// then exchange the returned Firebase custom token for credentials. Returns
/// `(refresh_token, email)` so the caller's existing refresh+save path is
/// unchanged.
async fn exchange_cli_code_for_legacy_firebase(
    hosted: &HostedEnvironment,
    code: &str,
    code_verifier: &str,
) -> Result<(String, String), AuthLoginError> {
    let url = format!("{}/v1/auth/cli/token", hosted.router_url);
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("CLI code exchange failed: {error}"))
    })?;
    let response = client
        .post(url)
        .json(&serde_json::json!({"code": code, "codeVerifier": code_verifier}))
        .send()
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!("CLI code exchange failed: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthLoginError::LoginFailed(format!(
            "CLI code exchange failed: HTTP {}",
            status.as_u16()
        )));
    }
    let data: Value = response.json().await.map_err(|error| {
        AuthLoginError::LoginFailed(format!("CLI code exchange failed: {error}"))
    })?;
    let custom_token = required_login_string(&data, "customToken")?;
    let email = data
        .get("email")
        .and_then(value_as_str)
        .unwrap_or_default()
        .to_owned();
    // Okta/SSO users are minted in a Firebase tenant; signInWithCustomToken must
    // run in the same tenant or the exchange fails.
    let tenant_id = data.get("tenantId").and_then(value_as_str);
    let firebase_api_key = firebase_api_key(hosted)?;
    let refreshed = sign_in_with_custom_token(&custom_token, &firebase_api_key, tenant_id).await?;
    Ok((refreshed.refresh_token, email))
}

/// Exchange a Firebase custom token for an id/refresh token pair via the
/// Identity Toolkit `signInWithCustomToken` endpoint. `tenant_id` scopes the
/// exchange to a Firebase tenant for Okta/OIDC SSO users.
async fn sign_in_with_custom_token(
    custom_token: &str,
    firebase_api_key: &str,
    tenant_id: Option<&str>,
) -> Result<RefreshedToken, AuthLoginError> {
    let endpoint =
        validated_firebase_endpoint(IDENTITY_TOOLKIT_CUSTOM_TOKEN_URL).map_err(|error| {
            AuthLoginError::LoginFailed(format!("Custom-token sign-in failed: {error}"))
        })?;
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("Custom-token sign-in failed: {error}"))
    })?;
    let mut request_body = serde_json::json!({"token": custom_token, "returnSecureToken": true});
    if let Some(tenant_id) = tenant_id {
        request_body["tenantId"] = Value::String(tenant_id.to_owned());
    }
    let response = client
        .post(endpoint)
        .query(&[("key", firebase_api_key)])
        .json(&request_body)
        .send()
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!(
                "Custom-token sign-in failed: {}",
                reqwest_error_message(error)
            ))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthLoginError::LoginFailed(format!(
            "Custom-token sign-in failed: HTTP {}",
            status.as_u16()
        )));
    }
    let data: Value = response.json().await.map_err(|error| {
        AuthLoginError::LoginFailed(format!(
            "Custom-token sign-in failed: {}",
            reqwest_error_message(error)
        ))
    })?;
    Ok(RefreshedToken {
        id_token: required_login_string(&data, "idToken")?,
        refresh_token: required_login_string(&data, "refreshToken")?,
        expires_in: data.get("expiresIn").and_then(value_as_i64).unwrap_or(3600),
    })
}

async fn run_rayline_device_login(
    hosted: &HostedEnvironment,
) -> Result<SessionToken, AuthLoginError> {
    let device = request_rayline_device_session(hosted).await?;
    write_rayline_device_login_prompt(&device)?;
    poll_rayline_device_session(hosted, &device).await
}

async fn request_rayline_device_session(
    hosted: &HostedEnvironment,
) -> Result<RaylineDeviceSession, AuthLoginError> {
    let url = format!("{}/v1/auth/cli/device/start", hosted.router_url);
    let response = auth_http_client()
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!("Device login start failed: {error}"))
        })?
        .post(url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!("Device login start failed: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthLoginError::LoginFailed(http_error_message(
            "Device login start failed",
            status.as_u16(),
            &body,
        )));
    }
    let data: Value = response.json().await.map_err(|error| {
        AuthLoginError::LoginFailed(format!("Device login start failed: {error}"))
    })?;
    Ok(RaylineDeviceSession {
        session_id: required_login_string(&data, "sessionId")?,
        user_code: required_login_string(&data, "userCode")?,
        verification_uri: required_login_string(&data, "verificationUri")?,
        interval_seconds: data
            .get("intervalSeconds")
            .and_then(value_as_i64)
            .unwrap_or(5)
            .max(1),
        expires_in_seconds: data
            .get("expiresInSeconds")
            .and_then(value_as_i64)
            .unwrap_or(600)
            .max(1),
    })
}

fn write_rayline_device_login_prompt(device: &RaylineDeviceSession) -> Result<(), AuthLoginError> {
    write_auth_message(&rayline_device_login_prompt(device)).map_err(AuthLoginError::WriteFailed)
}

fn rayline_device_login_prompt(device: &RaylineDeviceSession) -> String {
    format!(
        "\n  Visit:  {}\n  Code:   {}\n\n  Waiting for approval (timeout: {}s)...\n",
        device.verification_uri, device.user_code, device.expires_in_seconds
    )
}

async fn poll_rayline_device_session(
    hosted: &HostedEnvironment,
    device: &RaylineDeviceSession,
) -> Result<SessionToken, AuthLoginError> {
    let url = format!("{}/v1/auth/cli/device/poll", hosted.router_url);
    let started = Instant::now();
    let timeout = Duration::from_secs(device.expires_in_seconds as u64);
    let mut interval = device.interval_seconds as u64;
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("Device login polling failed: {error}"))
    })?;

    loop {
        if started.elapsed() >= timeout {
            return Err(AuthLoginError::LoginFailed(format!(
                "Login timed out before approval. Run `{} auth login --no-browser` to try again.",
                crate::CLI_BIN
            )));
        }

        let response = client
            .post(&url)
            .json(&serde_json::json!({ "sessionId": device.session_id.as_str() }))
            .send()
            .await
            .map_err(|error| {
                AuthLoginError::LoginFailed(format!("Device login polling failed: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthLoginError::LoginFailed(http_error_message(
                "Device login polling failed",
                status.as_u16(),
                &body,
            )));
        }

        let data: Value = serde_json::from_str(&body).map_err(|error| {
            AuthLoginError::LoginFailed(format!("Device login polling failed: {error}"))
        })?;
        match data
            .get("status")
            .and_then(value_as_str)
            .unwrap_or_default()
        {
            "approved" => {
                return parse_session_token_response(&data, "Device login polling")
                    .map_err(AuthLoginError::LoginFailed);
            }
            "pending" => {
                interval = data
                    .get("intervalSeconds")
                    .and_then(value_as_i64)
                    .unwrap_or(interval as i64)
                    .max(1) as u64;
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
            "expired" => {
                return Err(AuthLoginError::LoginFailed(format!(
                    "Login code expired before approval. Run `{} auth login --no-browser` to try again.",
                    crate::CLI_BIN
                )));
            }
            other => {
                return Err(AuthLoginError::LoginFailed(format!(
                    "Device login polling failed: unexpected status '{}'",
                    if other.is_empty() { "(missing)" } else { other }
                )));
            }
        }
    }
}

async fn run_legacy_device_login(hosted: &HostedEnvironment) -> Result<LoginToken, AuthLoginError> {
    let code = request_device_code(hosted).await?;
    write_device_login_prompt(&code)?;

    let access_token = poll_device_token(hosted, &code).await?;
    exchange_for_firebase(&access_token, hosted).await
}

fn write_device_login_prompt(code: &DeviceCode) -> Result<(), AuthLoginError> {
    write_auth_message(&device_login_prompt(code)).map_err(AuthLoginError::WriteFailed)
}

fn device_login_prompt(code: &DeviceCode) -> String {
    format!(
        "\n  Visit:  {}\n  Code:   {}\n\n  Waiting for approval (timeout: {}s)...\n",
        code.verification_url, code.user_code, code.expires_in
    )
}

pub fn write_auth_message(message: &str) -> io::Result<()> {
    write_interactive_message(message)
}

#[cfg(unix)]
fn write_interactive_message(message: &str) -> io::Result<()> {
    let output = terminal_output_text(message);
    match fs::OpenOptions::new().write(true).open("/dev/tty") {
        Ok(mut tty) => tty.write_all(output.as_bytes()),
        Err(_) => {
            eprint!("{output}");
            Ok(())
        }
    }
}

#[cfg(not(unix))]
fn write_interactive_message(message: &str) -> io::Result<()> {
    let output = terminal_output_text(message);
    print!("{output}");
    Ok(())
}

fn terminal_output_text(message: &str) -> String {
    message.chars().collect()
}

fn auth_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(AUTH_HTTP_TIMEOUT_SECONDS))
        .build()
}

async fn request_device_code(hosted: &HostedEnvironment) -> Result<DeviceCode, AuthLoginError> {
    let client_id = oauth_device_client_id(hosted)?;
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("Device code request failed: {error}"))
    })?;
    let response = client
        .post(GOOGLE_DEVICE_CODE_URL)
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", "openid email profile"),
        ])
        .send()
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!("Device code request failed: {error}"))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthLoginError::LoginFailed(format!(
            "Device code request failed: HTTP {}",
            status.as_u16()
        )));
    }
    let data: Value = response.json().await.map_err(|error| {
        AuthLoginError::LoginFailed(format!("Device code request failed: {error}"))
    })?;
    parse_device_code_response(&data)
}

async fn poll_device_token(
    hosted: &HostedEnvironment,
    code: &DeviceCode,
) -> Result<String, AuthLoginError> {
    let client_id = oauth_device_client_id(hosted)?;
    let client_secret = oauth_device_client_secret(hosted)?;
    let started = Instant::now();
    let mut interval = code.interval.max(1) as u64;
    let timeout = Duration::from_secs(code.expires_in.max(1) as u64);
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("Device-flow polling failed: {error}"))
    })?;

    loop {
        if started.elapsed() >= timeout {
            return Err(AuthLoginError::LoginFailed(format!(
                "Login timed out before approval. Run `{} auth login --no-browser` to try again.",
                crate::CLI_BIN
            )));
        }

        let response = client
            .post(GOOGLE_TOKEN_URL)
            .form(&[
                ("client_id", client_id.as_str()),
                ("client_secret", client_secret.as_str()),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await
            .map_err(|error| {
                AuthLoginError::LoginFailed(format!("Device-flow polling failed: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        match parse_device_token_response(status.is_success(), status.as_u16(), &body)? {
            DeviceTokenPoll::AccessToken(token) => return Ok(token),
            DeviceTokenPoll::Pending => {
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
            DeviceTokenPoll::SlowDown => {
                interval += 5;
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }
}

fn parse_device_code_response(data: &Value) -> Result<DeviceCode, AuthLoginError> {
    Ok(DeviceCode {
        verification_url: required_login_string(data, "verification_url")?,
        user_code: required_login_string(data, "user_code")?,
        device_code: required_login_string(data, "device_code")?,
        interval: data.get("interval").and_then(value_as_i64).unwrap_or(5),
        expires_in: data.get("expires_in").and_then(value_as_i64).unwrap_or(300),
    })
}

fn parse_device_token_response(
    status_success: bool,
    status_code: u16,
    body: &str,
) -> Result<DeviceTokenPoll, AuthLoginError> {
    if status_success {
        let data: Value = serde_json::from_str(body).map_err(|error| {
            AuthLoginError::LoginFailed(format!("Device-flow polling failed: {error}"))
        })?;
        return Ok(DeviceTokenPoll::AccessToken(required_login_string(
            &data,
            "access_token",
        )?));
    }

    let error = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(value_as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default();
    match error.as_str() {
        "authorization_pending" => Ok(DeviceTokenPoll::Pending),
        "slow_down" => Ok(DeviceTokenPoll::SlowDown),
        "access_denied" => Err(AuthLoginError::LoginFailed(
            "Login denied by user.".to_owned(),
        )),
        "expired_token" => Err(AuthLoginError::LoginFailed(format!(
            "Login code expired before approval. Run `{} auth login --no-browser` to try again.",
            crate::CLI_BIN
        ))),
        _ => Err(AuthLoginError::LoginFailed(format!(
            "Device-flow polling failed: HTTP {status_code}"
        ))),
    }
}

async fn exchange_for_firebase(
    access_token: &str,
    hosted: &HostedEnvironment,
) -> Result<LoginToken, AuthLoginError> {
    let endpoint = validated_firebase_endpoint(IDENTITY_TOOLKIT_URL).map_err(|error| {
        AuthLoginError::LoginFailed(format!("Firebase token exchange failed: {error}"))
    })?;
    let client = auth_http_client().map_err(|error| {
        AuthLoginError::LoginFailed(format!("Firebase token exchange failed: {error}"))
    })?;
    let firebase_api_key = firebase_api_key(hosted)?;
    let response = client
        .post(endpoint)
        .query(&[("key", firebase_api_key.as_str())])
        .json(&serde_json::json!({
            "postBody": format!("access_token={access_token}&providerId=google.com"),
            "requestUri": "http://localhost",
            "returnIdpCredential": true,
            "returnSecureToken": true,
        }))
        .send()
        .await
        .map_err(|error| {
            AuthLoginError::LoginFailed(format!(
                "Firebase token exchange failed: {}",
                reqwest_error_message(error)
            ))
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthLoginError::LoginFailed(format!(
            "Firebase token exchange failed: HTTP {}",
            status.as_u16()
        )));
    }
    let data: Value = response.json().await.map_err(|error| {
        AuthLoginError::LoginFailed(format!(
            "Firebase token exchange failed: {}",
            reqwest_error_message(error)
        ))
    })?;
    Ok(LoginToken {
        email: data
            .get("email")
            .and_then(value_as_str)
            .unwrap_or_default()
            .to_owned(),
        refreshed: RefreshedToken {
            id_token: required_login_string(&data, "idToken")?,
            refresh_token: required_login_string(&data, "refreshToken")?,
            expires_in: data.get("expiresIn").and_then(value_as_i64).unwrap_or(3600),
        },
    })
}

pub async fn claude_login(request: &ClaudeLoginRequest) -> Result<String, ClaudeLoginError> {
    let home = dirs::home_dir();
    let env_name = resolve_env(request.env_name.as_deref(), home.as_deref());
    let token_request = AuthTokenRequest {
        env_name: Some(env_name.clone()),
        auth_token: request.auth_token.clone(),
        root_env_explicit: request.root_env_explicit,
    };
    let token = match resolve_auth_token(&token_request).await? {
        AuthTokenOutcome::Available(token) => token,
        AuthTokenOutcome::NotLoggedIn => return Err(ClaudeLoginError::NotLoggedIn(env_name)),
    };
    let Some(home) = home else {
        return Err(ClaudeLoginError::WriteFailed(io::Error::new(
            io::ErrorKind::NotFound,
            "home directory not found",
        )));
    };
    let hosted = resolve_hosted_environment(&env_name, Some(&home))
        .map_err(|error| ClaudeLoginError::MintFailed(error.to_string()))?;
    let router_url = hosted.router_url.clone();

    claude_login_from_home_with_minter(
        &env_name,
        &home,
        &request.name,
        &token,
        unix_now_secs(),
        |_env_name, name, token| async move {
            mint_router_key_at(&router_url, &name, &token).await
        },
    )
    .await
}

pub async fn claude_login_from_home_with_minter<F, Fut>(
    env_name: &str,
    home: &Path,
    name: &str,
    token: &str,
    now: f64,
    mint: F,
) -> Result<String, ClaudeLoginError>
where
    F: FnOnce(String, String, String) -> Fut,
    Fut: std::future::Future<Output = Result<String, ClaudeLoginError>>,
{
    let api_key = mint(env_name.to_owned(), name.to_owned(), token.to_owned()).await?;
    save_claude_key_from_home(env_name, home, &api_key, now)?;

    let mut message = format!(
        "Stored {env_name} router key (prefix: {}…).\n",
        api_key.chars().take(12).collect::<String>()
    );
    if let Some(account_url) = resolve_hosted_environment(env_name, Some(home))
        .ok()
        .and_then(|hosted| hosted.account_url)
    {
        message.push_str(&format!("Manage keys at {account_url}\n"));
    }
    Ok(message)
}

pub fn load_claude_key(request: &ClaudeKeyRequest) -> Result<String, String> {
    let home = dirs::home_dir();
    let env_name = resolve_env(request.env_name.as_deref(), home.as_deref());
    let Some(home) = home else {
        return Err(missing_router_key_message(&env_name));
    };

    load_claude_key_from_home(&env_name, &home).ok_or_else(|| missing_router_key_message(&env_name))
}

pub fn load_claude_key_from_home(env_name: &str, home: &Path) -> Option<String> {
    let credentials = read_json(&credentials_file(home))?;
    credentials
        .get("router_keys")?
        .as_object()?
        .get(env_name)?
        .as_object()?
        .get("apiKey")
        .or_else(|| {
            credentials
                .get("router_keys")?
                .as_object()?
                .get(env_name)?
                .as_object()?
                .get("api_key")
        })
        .and_then(value_as_str)
        .filter(|key| !key.is_empty())
        .map(ToOwned::to_owned)
}

pub fn save_claude_key_from_home(
    env_name: &str,
    home: &Path,
    api_key: &str,
    now: f64,
) -> Result<(), ClaudeLoginError> {
    let credentials_path = credentials_file(home);
    let mut credentials = read_json(&credentials_path).unwrap_or_else(|| Value::Object(Map::new()));
    if !credentials.is_object() {
        credentials = Value::Object(Map::new());
    }
    let subject = stored_env_subject(&credentials, env_name);
    let root = credentials
        .as_object_mut()
        .expect("credentials value was normalized to an object");

    if !root.get("router_keys").is_some_and(Value::is_object) {
        root.insert("router_keys".to_owned(), Value::Object(Map::new()));
    }
    let router_keys = root
        .get_mut("router_keys")
        .and_then(Value::as_object_mut)
        .expect("router_keys was normalized to an object");

    let mut entry = Map::new();
    entry.insert("apiKey".to_owned(), Value::String(api_key.to_owned()));
    if let Some(subject) = subject {
        entry.insert("subject".to_owned(), Value::String(subject));
    }
    entry.insert("saved_at".to_owned(), Value::String(utc_timestamp(now)));
    router_keys.insert(env_name.to_owned(), Value::Object(entry));

    write_json_atomic(&credentials_path, &credentials).map_err(ClaudeLoginError::WriteFailed)
}

pub fn clear_claude_key(request: &ClaudeLogoutRequest) -> io::Result<String> {
    let home = dirs::home_dir();
    let env_name = resolve_env(request.env_name.as_deref(), home.as_deref());
    let Some(home) = home else {
        return Ok(format!("No {env_name} router key was stored.\n"));
    };

    clear_claude_key_from_home(&env_name, &home)
}

pub fn clear_claude_key_from_home(env_name: &str, home: &Path) -> io::Result<String> {
    let credentials_path = credentials_file(home);
    let Some(mut credentials) = read_json(&credentials_path) else {
        return Ok(format!("No {env_name} router key was stored.\n"));
    };
    let Some(keys) = credentials
        .get_mut("router_keys")
        .and_then(Value::as_object_mut)
    else {
        return Ok(format!("No {env_name} router key was stored.\n"));
    };
    if keys.remove(env_name).is_none() {
        return Ok(format!("No {env_name} router key was stored.\n"));
    }

    write_json_atomic(&credentials_path, &credentials)?;
    Ok(format!("Cleared {env_name} router key.\n"))
}

fn missing_router_key_message(env_name: &str) -> String {
    format!(
        "No {} router key for {env_name}. Run: {} auth login",
        crate::DISPLAY_NAME,
        crate::CLI_BIN
    )
}

pub fn render_status_from_home(
    env_name: &str,
    home: &Path,
    env_token: Option<&str>,
    now: f64,
) -> String {
    render_status_from_home_with_source(
        env_name,
        home,
        env_token,
        env_token.map(|_| "RAYLINE_AUTH_TOKEN"),
        None,
        now,
    )
}

fn render_status_from_home_with_source(
    env_name: &str,
    home: &Path,
    env_token: Option<&str>,
    env_token_source: Option<&str>,
    expected_auth_kind: Option<HostedAuthKind>,
    now: f64,
) -> String {
    let Some(status) = stored_status(
        env_name,
        home,
        env_token,
        env_token_source,
        expected_auth_kind,
    ) else {
        return format!("Not logged in. Run: {}\n", auth_command());
    };

    let mut output = String::new();
    output.push_str(&format!("  Email:       {}\n", status.email));
    // Only surface the environment when it's not the default prod target;
    // showing "Environment: prod" is noise for the common case.
    if status.env_name != "prod" {
        output.push_str(&format!("  Environment: {}\n", status.env_name));
    }
    output.push_str(&format!("  Auth source: {}\n", status.auth_source));

    if !status.logged_in_at.is_empty() {
        output.push_str(&format!("  Logged in:   {}\n", status.logged_in_at));
    }

    if status.env_token {
        output.push_str("  Token:       provided via env var (expiry unknown)\n");
    } else {
        let remaining = status.expires_at - now;
        if remaining > 0.0 {
            let ttl = remaining as i64;
            output.push_str(&format!("  Token TTL:   {}m {}s\n", ttl / 60, ttl % 60));

            let refresh_in = remaining - status.refresh_margin;
            if refresh_in > 0.0 {
                let refresh = refresh_in as i64;
                output.push_str(&format!(
                    "  Auto-refresh in: {}m {}s\n",
                    refresh / 60,
                    refresh % 60
                ));
            } else {
                output.push_str("  Auto-refresh: on next request\n");
            }
        } else {
            output.push_str("  Token:       expired (will refresh on next use)\n");
        }
    }

    if let Some(default_model) = default_model(home) {
        output.push_str(&format!("  Model:       {default_model}\n"));
    }

    output
}

pub(crate) fn resolve_env(env_override: Option<&str>, _home: Option<&Path>) -> String {
    if let Some(env_override) = env_override {
        return env_override.to_owned();
    }
    PROD_ENV.to_owned()
}

pub(crate) fn should_forward_for_invalid_envvar(_root_env_explicit: bool) -> bool {
    false
}

pub(crate) fn resolve_hosted_environment(
    env_name: &str,
    home: Option<&Path>,
) -> Result<HostedEnvironment, HostedEnvironmentError> {
    if !is_valid_root_env(env_name) {
        return Err(HostedEnvironmentError::InvalidName(env_name.to_owned()));
    }
    if env_name == PROD_ENV {
        return Ok(prod_hosted_environment());
    }
    let Some(home) = home else {
        return Err(HostedEnvironmentError::Unknown {
            env_name: env_name.to_owned(),
            settings_path: None,
        });
    };
    let settings_path = settings_file(home);
    let Some(settings) = read_settings(home) else {
        return Err(HostedEnvironmentError::Unknown {
            env_name: env_name.to_owned(),
            settings_path: Some(settings_path),
        });
    };
    let Some(entry) = settings
        .get("environments")
        .and_then(Value::as_object)
        .and_then(|envs| envs.get(env_name))
        .and_then(Value::as_object)
    else {
        return Err(HostedEnvironmentError::Unknown {
            env_name: env_name.to_owned(),
            settings_path: Some(settings_path),
        });
    };

    configured_hosted_environment(env_name, &settings_path, entry)
}

fn prod_hosted_environment() -> HostedEnvironment {
    HostedEnvironment {
        name: PROD_ENV.to_owned(),
        credential_key: PROD_ENV.to_owned(),
        router_url: crate::ROUTER_PROD_URL.to_owned(),
        cli_auth_url: PROD_CLI_AUTH_URL.to_owned(),
        account_url: Some(PROD_ACCOUNT_URL.to_owned()),
        auth_kind: HostedAuthKind::RaylineSession,
        firebase_api_key: None,
        google_device_client_id: None,
        google_device_client_secret: None,
    }
}

fn configured_hosted_environment(
    env_name: &str,
    settings_path: &Path,
    entry: &Map<String, Value>,
) -> Result<HostedEnvironment, HostedEnvironmentError> {
    let auth_kind = configured_auth_kind(env_name, settings_path, entry)?;
    let firebase_api_key = match auth_kind {
        HostedAuthKind::FirebaseLegacy => Some(required_env_string(
            env_name,
            settings_path,
            entry,
            "firebase_api_key",
        )?),
        HostedAuthKind::RaylineSession => optional_env_string(entry, "firebase_api_key"),
    };

    Ok(HostedEnvironment {
        name: env_name.to_owned(),
        credential_key: env_name.to_owned(),
        router_url: required_env_url(env_name, settings_path, entry, "router_url")?,
        cli_auth_url: required_env_url(env_name, settings_path, entry, "cli_auth_url")?,
        account_url: optional_env_url(env_name, settings_path, entry, "account_url")?,
        auth_kind,
        firebase_api_key,
        google_device_client_id: optional_env_string(entry, "google_device_client_id"),
        google_device_client_secret: optional_env_string(entry, "google_device_client_secret"),
    })
}

fn configured_auth_kind(
    env_name: &str,
    settings_path: &Path,
    entry: &Map<String, Value>,
) -> Result<HostedAuthKind, HostedEnvironmentError> {
    let Some(raw) = entry
        .get("auth_kind")
        .or_else(|| entry.get("authKind"))
        .and_then(value_as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(if entry.get("firebase_api_key").is_some() {
            HostedAuthKind::FirebaseLegacy
        } else {
            HostedAuthKind::RaylineSession
        });
    };

    match raw {
        RAYLINE_SESSION_AUTH_KIND | "session" => Ok(HostedAuthKind::RaylineSession),
        LEGACY_FIREBASE_AUTH_KIND | "firebase_legacy" | "legacy_firebase" => {
            Ok(HostedAuthKind::FirebaseLegacy)
        }
        value => Err(HostedEnvironmentError::InvalidAuthKind {
            env_name: env_name.to_owned(),
            settings_path: settings_path.to_owned(),
            value: value.to_owned(),
        }),
    }
}

fn required_env_string(
    env_name: &str,
    settings_path: &Path,
    entry: &Map<String, Value>,
    field: &'static str,
) -> Result<String, HostedEnvironmentError> {
    entry
        .get(field)
        .and_then(value_as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| HostedEnvironmentError::MissingField {
            env_name: env_name.to_owned(),
            settings_path: settings_path.to_owned(),
            field,
        })
}

fn optional_env_string(entry: &Map<String, Value>, field: &'static str) -> Option<String> {
    entry
        .get(field)
        .and_then(value_as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn required_env_url(
    env_name: &str,
    settings_path: &Path,
    entry: &Map<String, Value>,
    field: &'static str,
) -> Result<String, HostedEnvironmentError> {
    validate_env_url(
        env_name,
        settings_path,
        field,
        required_env_string(env_name, settings_path, entry, field)?,
    )
}

fn optional_env_url(
    env_name: &str,
    settings_path: &Path,
    entry: &Map<String, Value>,
    field: &'static str,
) -> Result<Option<String>, HostedEnvironmentError> {
    optional_env_string(entry, field)
        .map(|value| validate_env_url(env_name, settings_path, field, value))
        .transpose()
}

fn validate_env_url(
    env_name: &str,
    settings_path: &Path,
    field: &'static str,
    value: String,
) -> Result<String, HostedEnvironmentError> {
    let value = value.trim_end_matches('/').to_owned();
    let valid = Url::parse(&value).is_ok_and(|url| match (url.scheme(), url.host()) {
        ("https", Some(_)) => true,
        ("http", Some(host)) => is_local_http_env_host(host),
        _ => false,
    });
    if valid {
        Ok(value)
    } else {
        Err(HostedEnvironmentError::InvalidUrl {
            env_name: env_name.to_owned(),
            settings_path: settings_path.to_owned(),
            field,
            value,
        })
    }
}

fn is_local_http_env_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(host) => is_local_http_env_domain(host),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

fn is_local_http_env_domain(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.len() > ".localhost".len() && host.to_ascii_lowercase().ends_with(".localhost")
}

fn validated_firebase_endpoint(endpoint: &str) -> Result<String, String> {
    let endpoint = endpoint.trim();
    let url =
        Url::parse(endpoint).map_err(|error| format!("invalid Firebase endpoint URL: {error}"))?;
    let valid = match (url.scheme(), url.host()) {
        ("https", Some(_)) => true,
        ("http", Some(host)) => is_local_http_env_host(host),
        _ => false,
    };
    if valid {
        Ok(endpoint.to_owned())
    } else {
        Err("Firebase endpoint must use HTTPS unless it targets loopback HTTP".to_owned())
    }
}

fn cli_auth_url(hosted: &HostedEnvironment, state: &str) -> Result<String, AuthLoginError> {
    cli_auth_url_with_callback(hosted, state, None, None)
}

fn cli_auth_url_with_callback(
    hosted: &HostedEnvironment,
    state: &str,
    callback_url: Option<&str>,
    code_challenge: Option<&str>,
) -> Result<String, AuthLoginError> {
    let base_url = env::var("RAYLINE_CLI_AUTH_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| hosted.cli_auth_url.clone());

    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("state", state);
    if let Some(callback_url) = callback_url {
        query.append_pair("callback", callback_url);
    }
    // The presence of `challenge` is what tells a dual-mode web page to use the
    // PKCE code flow (GET navigation back to the loopback) instead of the legacy
    // refresh-token POST that older CLIs still rely on.
    if let Some(code_challenge) = code_challenge {
        query.append_pair("challenge", code_challenge);
    }
    Ok(format!("{base_url}?{}", query.finish()))
}

fn parse_paste_callback_url(pasted: &str, expected_state: &str) -> Result<String, AuthLoginError> {
    if pasted.starts_with('?') {
        return Err(AuthLoginError::InvalidPaste(
            "Please paste the full callback URL, not just the ?query string.".to_owned(),
        ));
    }

    let parsed = Url::parse(pasted).map_err(|_| {
        AuthLoginError::InvalidPaste(
            "Please paste the full callback URL (including http://127.0.0.1/...).".to_owned(),
        )
    })?;
    let code = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
        .unwrap_or_default();
    let state = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
        .unwrap_or_default();
    if state != expected_state {
        return Err(AuthLoginError::InvalidPaste(
            "State mismatch: the pasted URL does not belong to this login session.".to_owned(),
        ));
    }
    if code.is_empty() {
        return Err(AuthLoginError::InvalidPaste(
            "Could not extract login code from the pasted URL.".to_owned(),
        ));
    }
    Ok(code)
}

fn parse_legacy_paste_success_url(
    pasted: &str,
    expected_state: &str,
) -> Result<(String, String), AuthLoginError> {
    if pasted.starts_with('#') {
        return Err(AuthLoginError::InvalidPaste(
            "Please paste the full URL from the success page, not just the #fragment.".to_owned(),
        ));
    }

    let parsed = Url::parse(pasted).map_err(|_| {
        AuthLoginError::InvalidPaste(
            "Please paste the full URL from the success page (including https://...).".to_owned(),
        )
    })?;
    let pasted_state = parsed
        .query_pairs()
        .find_map(|(key, value)| (key == "state").then(|| value.into_owned()));
    if pasted_state.as_deref() != Some(expected_state) {
        return Err(AuthLoginError::InvalidPaste(
            "State mismatch: the pasted URL does not belong to this login session.".to_owned(),
        ));
    }

    let fragment = parsed.fragment().unwrap_or_default();
    let mut refresh_token = String::new();
    let mut email = String::new();
    for (key, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
        if key == "rt" {
            refresh_token = value.into_owned();
        } else if key == "em" {
            email = value.into_owned();
        }
    }
    if refresh_token.is_empty() {
        return Err(AuthLoginError::InvalidPaste(
            "Could not extract refresh token from the pasted URL.".to_owned(),
        ));
    }
    Ok((refresh_token, email))
}

fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<String, AuthLoginError> {
    listener.set_nonblocking(true).map_err(|error| {
        AuthLoginError::LoginFailed(format!("Failed to configure login callback: {error}"))
    })?;
    let deadline = Instant::now() + Duration::from_secs(WEB_CALLBACK_TIMEOUT_SECONDS);

    loop {
        match listener.accept() {
            Ok((mut stream, _addr)) => {
                // Winsock `accept()` returns a socket that inherits the
                // listener's non-blocking mode (set above), which makes
                // `read_http_request`'s `set_read_timeout` ineffective and lets
                // `read` return `WouldBlock` before the browser's GET arrives.
                // Reset to blocking. Unix accepts are already blocking, so this
                // is Windows-only to keep mac/Linux untouched.
                #[cfg(target_os = "windows")]
                let _ = stream.set_nonblocking(false);
                if let Some(code) = handle_callback_connection(&mut stream, expected_state)? {
                    return Ok(code);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(AuthLoginError::LoginFailed(format!(
                        "Login timed out ({WEB_CALLBACK_TIMEOUT_SECONDS}s). Try again."
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(AuthLoginError::LoginFailed(format!(
                    "Login callback failed: {error}"
                )));
            }
        }
    }
}

/// Handle one loopback connection. The dual-mode web page navigates the browser
/// to `http://127.0.0.1:<port>/?code=...&state=...` — a top-level GET, so there
/// is no CORS preflight and no credential in the request, only a one-time code.
/// Returns `Some(code)` once a matching-state code arrives.
fn handle_callback_connection(
    stream: &mut TcpStream,
    expected_state: &str,
) -> Result<Option<String>, AuthLoginError> {
    let request = read_http_request(stream)?;
    match request.method.as_str() {
        "GET" => match parse_callback_query(&request.target, expected_state) {
            Ok(Some(code)) => {
                respond_html(stream, 200, "OK", &auth_callback_page::success_html())
                    .map_err(AuthLoginError::WriteFailed)?;
                Ok(Some(code))
            }
            Ok(None) => {
                respond_html(stream, 200, "OK", &auth_callback_page::waiting_html())
                    .map_err(AuthLoginError::WriteFailed)?;
                Ok(None)
            }
            Err(CallbackQueryError::StateMismatch) => {
                respond_html(
                    stream,
                    400,
                    "Bad Request",
                    &auth_callback_page::error_html(
                        "State mismatch: this sign-in does not match the terminal session.",
                    ),
                )
                .map_err(AuthLoginError::WriteFailed)?;
                Ok(None)
            }
        },
        _ => {
            respond_html(
                stream,
                405,
                "Method Not Allowed",
                &auth_callback_page::error_html("Unsupported callback request."),
            )
            .map_err(AuthLoginError::WriteFailed)?;
            Ok(None)
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, AuthLoginError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(AuthLoginError::WriteFailed)?;

    let mut buffer = Vec::new();
    let mut temp = [0u8; 1024];
    loop {
        let read = stream
            .read(&mut temp)
            .map_err(AuthLoginError::WriteFailed)?;
        if read == 0 {
            return Err(AuthLoginError::LoginFailed(
                "Login callback closed before sending headers.".to_owned(),
            ));
        }
        buffer.extend_from_slice(&temp[..read]);
        if buffer.len() > 64 * 1024 {
            return Err(AuthLoginError::LoginFailed(
                "Login callback request was too large.".to_owned(),
            ));
        }
        if find_bytes(&buffer, b"\r\n\r\n").is_some() {
            break;
        }
    }

    // The loopback GET carries everything in the request line
    // (`GET /?code=...&state=... HTTP/1.1`); any body is ignored.
    let header_text = String::from_utf8_lossy(&buffer);
    let request_line = header_text.lines().next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    Ok(HttpRequest { method, target })
}

/// Parse the loopback GET target (`/?code=...&state=...`). Returns `Ok(None)`
/// when no code is present yet (a bare `/` or a favicon probe) so the wait loop
/// keeps polling, and `Err(StateMismatch)` when a code arrives under the wrong
/// state (a stale or foreign sign-in tab).
fn parse_callback_query(
    target: &str,
    expected_state: &str,
) -> Result<Option<String>, CallbackQueryError> {
    let query = target.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code = String::new();
    let mut state = String::new();
    for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
        if key == "code" {
            code = value.into_owned();
        } else if key == "state" {
            state = value.into_owned();
        }
    }
    if code.is_empty() {
        return Ok(None);
    }
    if state != expected_state {
        return Err(CallbackQueryError::StateMismatch);
    }
    Ok(Some(code))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn respond_html(stream: &mut TcpStream, code: u16, reason: &str, html: &str) -> io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}",
        html.len()
    )
}

fn save_session_credentials_from_home(
    env_name: &str,
    home: &Path,
    session: &SessionToken,
    now: f64,
) -> io::Result<bool> {
    let credentials_path = credentials_file(home);
    let mut credentials = read_json(&credentials_path).unwrap_or_else(|| Value::Object(Map::new()));
    if !credentials.is_object() {
        credentials = Value::Object(Map::new());
    }

    let previous_subject = stored_env_subject(&credentials, env_name);
    let previous_auth_kind = stored_env_auth_kind(&credentials, env_name);
    let root = credentials
        .as_object_mut()
        .expect("credentials value was normalized to an object");
    root.insert("version".to_owned(), Value::from(2));
    if !root.get("environments").is_some_and(Value::is_object) {
        root.insert("environments".to_owned(), Value::Object(Map::new()));
    }
    let environments = root
        .get_mut("environments")
        .and_then(Value::as_object_mut)
        .expect("environments was normalized to an object");

    let logged_in_at = environments
        .get(env_name)
        .and_then(|entry| entry.get("logged_in_at"))
        .and_then(value_as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| utc_timestamp(now));

    let mut entry = Map::new();
    entry.insert(
        "authKind".to_owned(),
        Value::String(RAYLINE_SESSION_AUTH_KIND.to_owned()),
    );
    entry.insert("subject".to_owned(), Value::String(session.subject.clone()));
    entry.insert("email".to_owned(), Value::String(session.email.clone()));
    entry.insert(
        "accessToken".to_owned(),
        Value::String(session.access_token.clone()),
    );
    entry.insert(
        "accessTokenExpiresAtMs".to_owned(),
        numeric_value(session.access_token_expires_at_ms),
    );
    entry.insert(
        "refreshToken".to_owned(),
        Value::String(session.refresh_token.clone()),
    );
    entry.insert(
        "refreshTokenExpiresAtMs".to_owned(),
        numeric_value(session.refresh_token_expires_at_ms),
    );
    entry.insert("logged_in_at".to_owned(), Value::String(logged_in_at));
    environments.insert(env_name.to_owned(), Value::Object(entry));

    let same_session_account = previous_auth_kind.as_deref() == Some(RAYLINE_SESSION_AUTH_KIND)
        && previous_subject
            .as_deref()
            .is_some_and(|previous| previous == session.subject);
    let cleared_router_key = !same_session_account
        && root
            .get_mut("router_keys")
            .and_then(Value::as_object_mut)
            .is_some_and(|keys| keys.remove(env_name).is_some());

    write_json_atomic(&credentials_path, &credentials)?;
    Ok(cleared_router_key)
}

/// Persist fresh legacy Firebase credentials for `env_name`. Returns `true` when the
/// stored router key for that environment was dropped because this login
/// could not be proven to match the account it was minted under.
fn save_env_credentials_from_home(
    env_name: &str,
    home: &Path,
    refreshed: &RefreshedToken,
    email: &str,
    now: f64,
) -> io::Result<bool> {
    let credentials_path = credentials_file(home);
    let mut credentials = read_json(&credentials_path).unwrap_or_else(|| Value::Object(Map::new()));
    if !credentials.is_object() {
        credentials = Value::Object(Map::new());
    }
    let root = credentials
        .as_object_mut()
        .expect("credentials value was normalized to an object");
    root.insert("version".to_owned(), Value::from(1));
    if !root.get("environments").is_some_and(Value::is_object) {
        root.insert("environments".to_owned(), Value::Object(Map::new()));
    }
    let environments = root
        .get_mut("environments")
        .and_then(Value::as_object_mut)
        .expect("environments was normalized to an object");

    // A stored router key belongs to whoever was logged in when it was
    // minted. Keep it only when this login is provably the same account,
    // compared by the token's stable subject (uid) — emails can collide
    // across auth providers/tenants, and uid survives an email rename.
    let previous_sub = environments
        .get(env_name)
        .and_then(|entry| entry.get("id_token"))
        .and_then(value_as_str)
        .and_then(extract_sub_from_token);
    let same_account = match (previous_sub, extract_sub_from_token(&refreshed.id_token)) {
        (Some(previous), Some(current)) => previous == current,
        _ => false,
    };

    let mut entry = Map::new();
    entry.insert(
        "refresh_token".to_owned(),
        Value::String(refreshed.refresh_token.clone()),
    );
    entry.insert(
        "id_token".to_owned(),
        Value::String(refreshed.id_token.clone()),
    );
    entry.insert(
        "id_token_expires_at".to_owned(),
        numeric_value(now + refreshed.expires_in as f64),
    );
    entry.insert("email".to_owned(), Value::String(email.to_owned()));
    entry.insert("logged_in_at".to_owned(), Value::String(utc_timestamp(now)));
    environments.insert(env_name.to_owned(), Value::Object(entry));

    let cleared_router_key = !same_account
        && root
            .get_mut("router_keys")
            .and_then(Value::as_object_mut)
            .is_some_and(|keys| keys.remove(env_name).is_some());

    write_json_atomic(&credentials_path, &credentials)?;
    Ok(cleared_router_key)
}

fn router_key_cleared_note(env_name: &str) -> String {
    format!(
        "Cleared {env_name} router key from a previous login; the next `{} claude` run will provision a new one.\n",
        crate::CLI_BIN
    )
}

fn stored_env_subject(credentials: &Value, env_name: &str) -> Option<String> {
    let env_data = credentials
        .get("environments")?
        .as_object()?
        .get(env_name)?
        .as_object()?;
    env_data
        .get("subject")
        .and_then(value_as_str)
        .filter(|subject| !subject.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            env_data
                .get("id_token")
                .and_then(value_as_str)
                .and_then(extract_sub_from_token)
        })
}

fn stored_env_auth_kind(credentials: &Value, env_name: &str) -> Option<String> {
    credentials
        .get("environments")?
        .as_object()?
        .get(env_name)?
        .as_object()?
        .get("authKind")
        .and_then(value_as_str)
        .filter(|kind| !kind.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_email_from_token(id_token: &str) -> Option<String> {
    decode_token_payload(id_token).and_then(|payload| {
        payload
            .get("email")
            .and_then(value_as_str)
            .map(ToOwned::to_owned)
    })
}

fn extract_sub_from_token(id_token: &str) -> Option<String> {
    decode_token_payload(id_token).and_then(|payload| {
        payload
            .get("sub")
            .and_then(value_as_str)
            .filter(|sub| !sub.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn random_state() -> String {
    use std::fmt::Write as _;

    let mut bytes = [0u8; 16];
    rand::rng().fill(&mut bytes);
    let mut state = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut state, "{byte:02x}").expect("writing to String cannot fail");
    }
    state
}

fn login_success_message(env_name: &str, email: &str) -> String {
    format!(
        "Logged in as {} ({env_name})\n",
        if email.is_empty() { "(unknown)" } else { email }
    )
}

fn is_headless() -> bool {
    env::var("SSH_CONNECTION").is_ok()
        && env::var("DISPLAY").is_err()
        && env::var("WAYLAND_DISPLAY").is_err()
        && env::var("BROWSER").is_err()
}

fn oauth_device_client_id(hosted: &HostedEnvironment) -> Result<String, AuthLoginError> {
    hosted
        .google_device_client_id
        .clone()
        .ok_or_else(|| missing_device_credential(&hosted.name, "google_device_client_id"))
}

fn oauth_device_client_secret(hosted: &HostedEnvironment) -> Result<String, AuthLoginError> {
    hosted
        .google_device_client_secret
        .clone()
        .ok_or_else(|| missing_device_credential(&hosted.name, "google_device_client_secret"))
}

fn missing_device_credential(env_name: &str, field: &str) -> AuthLoginError {
    AuthLoginError::LoginFailed(format!(
        "Device-flow OAuth client not configured for {env_name}: missing {field}"
    ))
}

fn required_login_string(data: &Value, key: &str) -> Result<String, AuthLoginError> {
    data.get(key)
        .and_then(value_as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| AuthLoginError::LoginFailed(format!("Login response missing {key}")))
}

fn required_response_string(data: &Value, key: &str, context: &str) -> Result<String, String> {
    data.get(key)
        .and_then(value_as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{context} failed: missing {key}"))
}

fn required_response_f64(data: &Value, key: &str, context: &str) -> Result<f64, String> {
    data.get(key)
        .and_then(value_as_f64)
        .ok_or_else(|| format!("{context} failed: missing {key}"))
}

fn parse_session_token_response(data: &Value, context: &str) -> Result<SessionToken, String> {
    Ok(SessionToken {
        access_token: required_response_string(data, "accessToken", context)?,
        refresh_token: required_response_string(data, "refreshToken", context)?,
        access_token_expires_at_ms: required_response_f64(data, "accessTokenExpiresAtMs", context)?,
        refresh_token_expires_at_ms: required_response_f64(
            data,
            "refreshTokenExpiresAtMs",
            context,
        )?,
        subject: required_response_string(data, "subject", context)?,
        email: data
            .get("email")
            .and_then(value_as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

fn parse_session_refresh_response(data: &Value, context: &str) -> Result<SessionToken, String> {
    Ok(SessionToken {
        access_token: required_response_string(data, "accessToken", context)?,
        refresh_token: required_response_string(data, "refreshToken", context)?,
        access_token_expires_at_ms: required_response_f64(data, "accessTokenExpiresAtMs", context)?,
        refresh_token_expires_at_ms: required_response_f64(
            data,
            "refreshTokenExpiresAtMs",
            context,
        )?,
        subject: data
            .get("subject")
            .and_then(value_as_str)
            .unwrap_or_default()
            .to_owned(),
        email: data
            .get("email")
            .and_then(value_as_str)
            .unwrap_or_default()
            .to_owned(),
    })
}

async fn refresh_rayline_session(
    router_base_url: &str,
    refresh_token: &str,
) -> Result<SessionToken, AuthTokenError> {
    let url = format!("{router_base_url}/v1/auth/cli/refresh");
    let response = auth_http_client()
        .map_err(|error| AuthTokenError::RefreshFailed(format!("Token refresh failed: {error}")))?
        .post(url)
        .json(&serde_json::json!({ "refreshToken": refresh_token }))
        .send()
        .await
        .map_err(|error| {
            AuthTokenError::RefreshFailed(format!(
                "Token refresh failed: {}",
                reqwest_error_message(error)
            ))
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthTokenError::RefreshFailed(http_error_message(
            "Token refresh failed",
            status.as_u16(),
            &body,
        )));
    }

    let data: Value = response.json().await.map_err(|error| {
        AuthTokenError::RefreshFailed(format!(
            "Token refresh failed: {}",
            reqwest_error_message(error)
        ))
    })?;
    parse_session_refresh_response(&data, "Token refresh").map_err(AuthTokenError::RefreshFailed)
}

fn session_revoke_request(env_name: &str, home: &Path) -> Option<(String, String)> {
    let credentials = read_json(&credentials_file(home))?;
    let env_data = credentials
        .get("environments")?
        .as_object()?
        .get(env_name)?
        .as_object()?;
    if env_data
        .get("authKind")
        .and_then(value_as_str)
        .is_some_and(|kind| kind != RAYLINE_SESSION_AUTH_KIND)
    {
        return None;
    }
    let token = env_data
        .get("refreshToken")
        .and_then(value_as_str)
        .or_else(|| env_data.get("accessToken").and_then(value_as_str))
        .filter(|token| !token.is_empty())?
        .to_owned();
    let hosted = resolve_hosted_environment(env_name, Some(home)).ok()?;
    Some((hosted.router_url, token))
}

async fn revoke_rayline_session(router_base_url: &str, token: &str) -> Result<(), String> {
    let url = format!("{router_base_url}/v1/auth/cli/revoke");
    let response = auth_http_client()
        .map_err(|error| format!("failed to build HTTP client: {error}"))?
        .post(url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "reason": "cli_logout" }))
        .send()
        .await
        .map_err(|error| format!("failed to reach router: {}", reqwest_error_message(error)))?;

    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        let body = response.text().await.unwrap_or_default();
        Err(http_error_message(
            "session revoke failed",
            status.as_u16(),
            &body,
        ))
    }
}

fn http_error_message(context: &str, status_code: u16, body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        format!("{context}: HTTP {status_code}")
    } else {
        format!("{context}: HTTP {status_code}: {body}")
    }
}

fn env_auth_token() -> Option<(String, &'static str)> {
    for source in ["RAYLINE_AUTH_TOKEN", "RAYLINE_ID_TOKEN"] {
        if let Ok(token) = env::var(source) {
            if !token.is_empty() {
                return Some((token, source));
            }
        }
    }
    None
}

fn firebase_api_key(hosted: &HostedEnvironment) -> Result<String, AuthLoginError> {
    hosted.firebase_api_key.clone().ok_or_else(|| {
        AuthLoginError::LoginFailed(format!(
            "Legacy Firebase auth is not configured for {}: missing firebase_api_key",
            hosted.name
        ))
    })
}

fn open_browser(url: &str) {
    // macOS/Linux pass the URL as a single argv to `open`/`xdg-open`, so the URL
    // is never shell-parsed and these platforms already work — keep them as-is.
    // Windows is the broken case: `cmd /C start` treats `&` as a command
    // separator and `%` as variable expansion, truncating the OAuth URL at the
    // first `&` and dropping the `callback`/`challenge` params so the loopback
    // never fires. `opener::open` uses ShellExecuteW there, passing the URL
    // untouched. We deliberately use `open` and not `open_browser`: the latter
    // honors `$BROWSER` first (commonly set under Git Bash/MSYS) and would spawn
    // that value instead of ShellExecuteW, reintroducing the same launch failure.
    // Best-effort either way: the URL is also printed for manual fallback.
    #[cfg(target_os = "windows")]
    {
        let _ = opener::open(url);
    }
    #[cfg(not(target_os = "windows"))]
    {
        let mut command = browser_open_command(url);
        let _ = command.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    }
}

#[cfg(not(target_os = "windows"))]
fn browser_open_command(url: &str) -> Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(url);
        command
    }
    #[cfg(target_os = "linux")]
    {
        let mut command = Command::new("xdg-open");
        command.arg(url);
        command
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let mut command = Command::new("true");
        command.arg(url);
        command
    }
}

async fn refresh_firebase_token(
    refresh_token: &str,
    firebase_api_key: &str,
    secure_token_url: &str,
) -> Result<RefreshedToken, AuthTokenError> {
    let endpoint = validated_firebase_endpoint(secure_token_url)
        .map_err(|error| AuthTokenError::RefreshFailed(format!("Token refresh failed: {error}")))?;
    let client = auth_http_client()
        .map_err(|error| AuthTokenError::RefreshFailed(format!("Token refresh failed: {error}")))?;
    let response = client
        .post(endpoint)
        .query(&[("key", firebase_api_key)])
        .form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|error| {
            AuthTokenError::RefreshFailed(format!(
                "Token refresh failed: {}",
                reqwest_error_message(error)
            ))
        })?;

    let status = response.status();
    if !status.is_success() {
        return Err(AuthTokenError::RefreshFailed(format!(
            "Token refresh failed: HTTP {}",
            status.as_u16()
        )));
    }

    let data: Value = response.json().await.map_err(|error| {
        AuthTokenError::RefreshFailed(format!(
            "Token refresh failed: {}",
            reqwest_error_message(error)
        ))
    })?;
    Ok(RefreshedToken {
        id_token: required_string(&data, "id_token")?,
        refresh_token: required_string(&data, "refresh_token")?,
        expires_in: data
            .get("expires_in")
            .and_then(value_as_i64)
            .unwrap_or(3600),
    })
}

fn reqwest_error_message(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

pub(crate) async fn mint_router_key(
    env_name: &str,
    home: &Path,
    name: &str,
    bearer_token: &str,
) -> Result<String, ClaudeLoginError> {
    let hosted = resolve_hosted_environment(env_name, Some(home))
        .map_err(|error| ClaudeLoginError::MintFailed(error.to_string()))?;
    mint_router_key_at(&hosted.router_url, name, bearer_token).await
}

async fn mint_router_key_at(
    router_base_url: &str,
    name: &str,
    bearer_token: &str,
) -> Result<String, ClaudeLoginError> {
    let url = format!("{router_base_url}/v1/keys");
    let response = auth_http_client()
        .map_err(|error| ClaudeLoginError::MintFailed(format!("Failed to reach router: {error}")))?
        .post(url)
        .bearer_auth(bearer_token)
        .json(&serde_json::json!({ "name": name }))
        .send()
        .await
        .map_err(|error| {
            ClaudeLoginError::MintFailed(format!("Failed to reach router: {error}"))
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(router_key_http_error(status.as_u16(), &body));
    }

    let body: Value = response
        .json()
        .await
        .map_err(|error| ClaudeLoginError::MintFailed(format!("Unexpected response: {error}")))?;
    if let Some(error) = body.get("error") {
        return Err(ClaudeLoginError::MintFailed(format!(
            "Server error: {}",
            json_value_for_message(error)
        )));
    }
    body.get("apiKey")
        .and_then(value_as_str)
        .filter(|api_key| !api_key.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            ClaudeLoginError::MintFailed(format!(
                "Unexpected response: {}",
                json_value_for_message(&body)
            ))
        })
}

fn router_key_http_error(status_code: u16, body: &str) -> ClaudeLoginError {
    ClaudeLoginError::MintFailed(format!(
        "{} router key request failed ({status_code}): {body}",
        crate::DISPLAY_NAME
    ))
}

/// Mint a fresh router key for `env_name` and persist it, returning the key.
///
/// Shared by auth login and the on-demand provisioning the `claude` run path
/// performs when no key is stored yet.
pub(crate) async fn provision_router_key(
    env_name: &str,
    home: &Path,
    name: &str,
    bearer_token: &str,
) -> Result<String, ClaudeLoginError> {
    let api_key = mint_router_key(env_name, home, name, bearer_token).await?;
    save_claude_key_from_home(env_name, home, &api_key, unix_now_secs())?;
    Ok(api_key)
}

fn required_string(data: &Value, key: &str) -> Result<String, AuthTokenError> {
    data.get(key)
        .and_then(value_as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            AuthTokenError::RefreshFailed(format!("Token refresh failed: missing {key}"))
        })
}

fn stored_status(
    env_name: &str,
    home: &Path,
    env_token: Option<&str>,
    env_token_source: Option<&str>,
    expected_auth_kind: Option<HostedAuthKind>,
) -> Option<StoredStatus> {
    if env_token.is_some_and(|token| !token.is_empty()) {
        return Some(StoredStatus {
            email: "(env-var token)".to_owned(),
            expires_at: 0.0,
            env_name: env_name.to_owned(),
            logged_in_at: String::new(),
            refresh_margin: TOKEN_REFRESH_MARGIN_SECONDS,
            auth_source: env_token_source.unwrap_or("RAYLINE_AUTH_TOKEN").to_owned(),
            env_token: true,
        });
    }

    let credentials = read_json(&credentials_file(home))?;
    let env_data = credentials
        .get("environments")?
        .as_object()?
        .get(env_name)?
        .as_object()?;

    let auth_kind = env_data
        .get("authKind")
        .and_then(value_as_str)
        .unwrap_or(LEGACY_FIREBASE_AUTH_KIND);
    match expected_auth_kind {
        Some(HostedAuthKind::RaylineSession) if auth_kind != RAYLINE_SESSION_AUTH_KIND => {
            return None;
        }
        Some(HostedAuthKind::FirebaseLegacy) if auth_kind == RAYLINE_SESSION_AUTH_KIND => {
            return None;
        }
        _ => {}
    }
    let expires_at = if auth_kind == RAYLINE_SESSION_AUTH_KIND {
        env_data
            .get("accessTokenExpiresAtMs")
            .and_then(value_as_f64)
            .map(|value| value / 1000.0)
            .unwrap_or(0.0)
    } else {
        env_data
            .get("id_token_expires_at")
            .and_then(value_as_f64)
            .unwrap_or(0.0)
    };

    Some(StoredStatus {
        email: env_data
            .get("email")
            .and_then(value_as_str)
            .unwrap_or_default()
            .to_owned(),
        expires_at,
        env_name: env_name.to_owned(),
        logged_in_at: env_data
            .get("logged_in_at")
            .and_then(value_as_str)
            .unwrap_or_default()
            .to_owned(),
        refresh_margin: TOKEN_REFRESH_MARGIN_SECONDS,
        auth_source: auth_kind.to_owned(),
        env_token: false,
    })
}

fn decode_token_payload(token: &str) -> Option<Value> {
    let parts = token.split('.').collect::<Vec<_>>();
    if parts.len() < 3 {
        return None;
    }
    let payload = parts[1].trim_end_matches('=');
    let mut padded_payload = payload.to_owned();
    padded_payload.push_str(&"=".repeat((4 - padded_payload.len() % 4) % 4));
    let decoded = base64::engine::general_purpose::URL_SAFE
        .decode(padded_payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&decoded).ok()?;
    claims.is_object().then_some(claims)
}

fn default_model(home: &Path) -> Option<String> {
    read_settings(home)?
        .get("default_model")
        .and_then(value_as_str)
        .map(ToOwned::to_owned)
}

pub(crate) fn read_settings(home: &Path) -> Option<Value> {
    let settings = settings_file(home);
    if settings.exists() {
        return read_json(&settings);
    }

    let legacy = legacy_settings_file(home);
    if legacy.exists() {
        return read_json(&legacy);
    }

    None
}

/// Like [`read_settings`], but a settings file that exists and cannot be read
/// or parsed is an error naming the file, not "no settings". The error carries
/// only the path and the parser's position, never the file's contents.
pub(crate) fn read_settings_checked(home: &Path) -> io::Result<Option<(PathBuf, Value)>> {
    for path in [settings_file(home), legacy_settings_file(home)] {
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(io::Error::new(
                    error.kind(),
                    format!("cannot read {}: {error}", path.display()),
                ));
            }
        };
        let value = serde_json::from_str(&contents).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not valid JSON: {error}", path.display()),
            )
        })?;
        return Ok(Some((path, value)));
    }
    Ok(None)
}

/// Persist `value` as the canonical settings file (`~/.config/<brand>/
/// settings.json`), atomically. Writes always target the canonical path even
/// when only the legacy location existed, migrating it forward on first write.
pub(crate) fn write_settings(home: &Path, value: &Value) -> io::Result<()> {
    write_json_atomic(&settings_file(home), value)
}

fn read_json(path: &Path) -> Option<Value> {
    let contents = fs::read_to_string(path).ok()?;
    serde_json::from_str(&contents).ok()
}

fn write_json_atomic(path: &Path, value: &Value) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    }

    let tmp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("credentials.json"),
        std::process::id()
    ));
    let data = serde_json::to_string_pretty(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;

    if let Err(error) = fs::write(&tmp_path, data) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        if let Err(error) = fs::set_permissions(&tmp_path, fs::Permissions::from_mode(0o600)) {
            let _ = fs::remove_file(&tmp_path);
            return Err(error);
        }
    }

    if let Err(error) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    Ok(())
}

fn value_as_str(value: &Value) -> Option<&str> {
    value.as_str()
}

fn value_as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<f64>().ok()))
}

fn value_as_i64(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
}

fn numeric_value(value: f64) -> Value {
    serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn json_value_for_message(value: &Value) -> String {
    match value {
        Value::String(message) => message.clone(),
        _ => value.to_string(),
    }
}

fn utc_timestamp(now: f64) -> String {
    let seconds = now.floor() as i64;
    let days = seconds.div_euclid(86_400);
    let seconds_of_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = seconds_of_day % 3_600 / 60;
    let second = seconds_of_day % 60;

    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_unix_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_unix_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_phase = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_phase + 2) / 5 + 1;
    let month = month_phase + if month_phase < 10 { 3 } else { -9 };
    let year = year + if month <= 2 { 1 } else { 0 };

    (year, month, day)
}

fn credentials_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("credentials.json")
}

fn settings_file(home: &Path) -> PathBuf {
    home.join(".config")
        .join(crate::CONFIG_DIR)
        .join("settings.json")
}

fn legacy_settings_file(home: &Path) -> PathBuf {
    home.join(crate::DOT_CONFIG_DIR).join("settings.json")
}

fn auth_command() -> String {
    format!("{} auth login", crate::CLI_BIN)
}

pub(crate) fn unix_now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

struct StoredStatus {
    email: String,
    expires_at: f64,
    env_name: String,
    logged_in_at: String,
    refresh_margin: f64,
    auth_source: String,
    env_token: bool,
}

struct RefreshedToken {
    id_token: String,
    refresh_token: String,
    expires_in: i64,
}

struct SessionToken {
    access_token: String,
    refresh_token: String,
    access_token_expires_at_ms: f64,
    refresh_token_expires_at_ms: f64,
    subject: String,
    email: String,
}

struct LoginToken {
    refreshed: RefreshedToken,
    email: String,
}

#[derive(Debug, Eq, PartialEq)]
struct RaylineDeviceSession {
    session_id: String,
    user_code: String,
    verification_uri: String,
    interval_seconds: i64,
    expires_in_seconds: i64,
}

#[derive(Debug, Eq, PartialEq)]
struct DeviceCode {
    verification_url: String,
    user_code: String,
    device_code: String,
    interval: i64,
    expires_in: i64,
}

#[derive(Debug, Eq, PartialEq)]
enum DeviceTokenPoll {
    AccessToken(String),
    Pending,
    SlowDown,
}

struct HttpRequest {
    method: String,
    target: String,
}

#[derive(Debug, Eq, PartialEq)]
enum CallbackQueryError {
    StateMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time went backwards")
            .as_nanos();
        env::temp_dir().join(format!(
            "rayline-status-{name}-{}-{unique}",
            std::process::id()
        ))
    }

    #[test]
    fn checked_settings_read_reports_a_bad_file_instead_of_dropping_it() {
        let home = temp_home("settings-checked");
        assert!(
            read_settings_checked(&home)
                .expect("missing is fine")
                .is_none()
        );
        let path = settings_file(&home);
        fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        for bad in [
            "{\"hybrid\": {\"routing\": {\"value_usd\": 1e999}}}",
            "{\"hybrid\": {\"routing\": {\"policy\": \"utility\",}}}",
            "",
        ] {
            fs::write(&path, bad).expect("write");
            let error = read_settings_checked(&home).expect_err(bad);
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            let message = error.to_string();
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(!message.contains("utility"), "{message}");
            // The lenient reader other commands use is unchanged.
            assert!(read_settings(&home).is_none());
        }
        fs::write(&path, "{\"hybrid\": {\"routing\": {\"value_usd\": 1}}}").expect("write");
        let (found, value) = read_settings_checked(&home)
            .expect("valid")
            .expect("present");
        assert_eq!(found, path);
        assert_eq!(value["hybrid"]["routing"]["value_usd"], 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod");
            // Root reads it anyway; only assert where the permission applies.
            if fs::read(&path).is_err() {
                let message = read_settings_checked(&home)
                    .expect_err("unreadable")
                    .to_string();
                assert!(message.contains("cannot read"), "{message}");
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).expect("chmod");
        }
        fs::remove_file(&path).expect("remove");
        let legacy = legacy_settings_file(&home);
        fs::create_dir_all(legacy.parent().expect("parent")).expect("mkdir");
        fs::write(&legacy, "{").expect("write");
        let message = read_settings_checked(&home)
            .expect_err("legacy malformed")
            .to_string();
        assert!(message.contains(&legacy.display().to_string()), "{message}");
        let _ = fs::remove_dir_all(&home);
    }

    fn sample_session(subject: &str, access_token: &str, refresh_token: &str) -> SessionToken {
        SessionToken {
            access_token: access_token.to_owned(),
            refresh_token: refresh_token.to_owned(),
            access_token_expires_at_ms: 3_600_000.0,
            refresh_token_expires_at_ms: 86_400_000.0,
            subject: subject.to_owned(),
            email: format!("{subject}@example.com"),
        }
    }

    fn fake_jwt_with_subject(subject: &str) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
            serde_json::json!({ "sub": subject, "email": format!("{subject}@example.com") })
                .to_string(),
        );
        format!("{header}.{payload}.sig")
    }

    #[test]
    fn render_status_ignores_empty_env_token() {
        let home = temp_home("empty-token");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");

        write_json_atomic(
            &credentials_file(&home),
            &serde_json::json!({
                "environments": {
                    "dev": {
                        "email": "dev@example.com",
                        "id_token_expires_at": 3600.0,
                        "logged_in_at": "2026-06-19T08:00:00Z"
                    }
                }
            }),
        )
        .expect("write credentials");

        let output = render_status_from_home("dev", &home, Some(""), 0.0);

        assert!(output.contains("Email:       dev@example.com"));
        assert!(output.contains("Auth source: firebase"));
        assert!(!output.contains("RAYLINE_ID_TOKEN"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_env_ignores_stale_default_env() {
        let home = temp_home("default-env");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        write_settings(&home, &serde_json::json!({"default_env": "dev"})).expect("write settings");

        assert_eq!(resolve_env(None, Some(&home)), "prod");

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_hosted_environment_reads_builtin_prod() {
        let hosted = resolve_hosted_environment(&resolve_env(None, None), None).expect("prod env");

        assert_eq!(hosted.name, "prod");
        assert_eq!(hosted.router_url, crate::ROUTER_PROD_URL);
        assert_eq!(hosted.cli_auth_url, PROD_CLI_AUTH_URL);
        assert_eq!(hosted.auth_kind, HostedAuthKind::RaylineSession);
        assert!(hosted.firebase_api_key.is_none());
        assert!(hosted.google_device_client_id.is_none());
        assert!(hosted.google_device_client_secret.is_none());
    }

    #[test]
    fn resolve_hosted_environment_reads_configured_env() {
        let home = temp_home("configured-env");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        write_settings(
            &home,
            &serde_json::json!({
                "environments": {
                    "foo": {
                        "router_url": "https://router.example.test/",
                        "cli_auth_url": "https://platform.example.test/cli-auth/",
                        "account_url": "https://platform.example.test/",
                        "firebase_api_key": "firebase-key",
                        "google_device_client_id": "client-id",
                        "google_device_client_secret": "client-secret"
                    }
                }
            }),
        )
        .expect("write settings");

        let hosted = resolve_hosted_environment("foo", Some(&home)).expect("configured env");

        assert_eq!(hosted.name, "foo");
        assert_eq!(hosted.credential_key, "foo");
        assert_eq!(hosted.router_url, "https://router.example.test");
        assert_eq!(
            hosted.cli_auth_url,
            "https://platform.example.test/cli-auth"
        );
        assert_eq!(
            hosted.account_url.as_deref(),
            Some("https://platform.example.test")
        );
        assert_eq!(hosted.auth_kind, HostedAuthKind::FirebaseLegacy);
        assert_eq!(hosted.firebase_api_key.as_deref(), Some("firebase-key"));
        assert_eq!(hosted.google_device_client_id.as_deref(), Some("client-id"));
        assert_eq!(
            hosted.google_device_client_secret.as_deref(),
            Some("client-secret")
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn validate_env_url_accepts_https_hosts() {
        let settings_path = Path::new("/tmp/rayline-settings.json");

        let url = validate_env_url(
            "foo",
            settings_path,
            "router_url",
            "https://router.example.test/".to_owned(),
        )
        .expect("https URL");

        assert_eq!(url, "https://router.example.test");
    }

    #[test]
    fn validate_env_url_accepts_http_loopback_hosts() {
        let settings_path = Path::new("/tmp/rayline-settings.json");
        let cases = [
            ("http://localhost:8787/", "http://localhost:8787"),
            ("http://api.localhost:8787/", "http://api.localhost:8787"),
            ("http://127.0.0.1:8787/", "http://127.0.0.1:8787"),
            ("http://[::1]:8787/", "http://[::1]:8787"),
        ];

        for (input, expected) in cases {
            let url = validate_env_url("foo", settings_path, "router_url", input.to_owned())
                .expect("loopback HTTP URL");

            assert_eq!(url, expected);
        }
    }

    #[test]
    fn validate_env_url_rejects_http_non_loopback_hosts() {
        let settings_path = Path::new("/tmp/rayline-settings.json");

        let error = validate_env_url(
            "foo",
            settings_path,
            "router_url",
            "http://router.example.test/".to_owned(),
        )
        .expect_err("non-loopback HTTP URL");

        assert!(matches!(
            &error,
            HostedEnvironmentError::InvalidUrl { field, value, .. }
                if *field == "router_url" && value == "http://router.example.test"
        ));
        assert!(error.to_string().contains("invalid URL field 'router_url'"));
        assert!(error.to_string().contains("http://router.example.test"));
    }

    #[test]
    fn resolve_hosted_environment_rejects_unknown_env() {
        let home = temp_home("unknown-env");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");

        let error = resolve_hosted_environment("foo", Some(&home)).expect_err("unknown env");

        assert!(
            error
                .to_string()
                .contains("Unknown Rayline environment 'foo'")
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn resolve_hosted_environment_names_missing_required_field() {
        let home = temp_home("missing-field");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        write_settings(
            &home,
            &serde_json::json!({
                "environments": {
                    "foo": {
                        "router_url": "https://router.example.test",
                        "firebase_api_key": "firebase-key"
                    }
                }
            }),
        )
        .expect("write settings");

        let error = resolve_hosted_environment("foo", Some(&home)).expect_err("missing field");

        assert!(error.to_string().contains("cli_auth_url"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn configured_env_requires_device_fields_for_device_flow() {
        let home = temp_home("device-fields");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        write_settings(
            &home,
            &serde_json::json!({
                "environments": {
                    "foo": {
                        "router_url": "https://router.example.test",
                        "cli_auth_url": "https://platform.example.test/cli-auth",
                        "firebase_api_key": "firebase-key"
                    }
                }
            }),
        )
        .expect("write settings");
        let hosted = resolve_hosted_environment("foo", Some(&home)).expect("configured env");

        let error = oauth_device_client_id(&hosted).expect_err("missing device client id");

        assert!(error.to_string().contains("google_device_client_id"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn logout_clears_default_prod_without_hosted_environment() {
        let home = temp_home("logout-prod");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        let refreshed = RefreshedToken {
            id_token: "id-token".to_owned(),
            refresh_token: "refresh-token".to_owned(),
            expires_in: 3600,
        };
        save_env_credentials_from_home("prod", &home, &refreshed, "prod@example.com", 0.0)
            .expect("save credentials");
        save_claude_key_from_home("prod", &home, "rk_prod", 0.0).expect("save router key");

        let output = logout_from_home("prod", &home).expect("logout");

        assert!(output.contains("Logged out (prod)"));
        assert!(output.contains("Cleared prod router key."));
        assert!(stored_status("prod", &home, None, None, None).is_none());
        assert!(load_claude_key_from_home("prod", &home).is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn render_status_rejects_legacy_credentials_for_session_environment() {
        let home = temp_home("status-session-env");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        let legacy = RefreshedToken {
            id_token: fake_jwt_with_subject("sub-one"),
            refresh_token: "legacy-refresh".to_owned(),
            expires_in: 3600,
        };
        save_env_credentials_from_home("prod", &home, &legacy, "sub-one@example.com", 0.0)
            .expect("save legacy credentials");

        let output = render_status_from_home_with_source(
            "prod",
            &home,
            None,
            None,
            Some(HostedAuthKind::RaylineSession),
            0.0,
        );

        assert!(output.contains("Not logged in"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn configured_env_storage_is_namespaced() {
        let home = temp_home("storage");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        let refreshed = RefreshedToken {
            id_token: "id-token".to_owned(),
            refresh_token: "refresh-token".to_owned(),
            expires_in: 3600,
        };

        save_env_credentials_from_home("foo", &home, &refreshed, "foo@example.com", 0.0)
            .expect("save credentials");
        save_claude_key_from_home("foo", &home, "rk_foo", 0.0).expect("save router key");

        assert!(stored_status("foo", &home, None, None, None).is_some());
        assert!(stored_status("prod", &home, None, None, None).is_none());
        assert_eq!(
            load_claude_key_from_home("foo", &home).as_deref(),
            Some("rk_foo")
        );
        assert!(load_claude_key_from_home("prod", &home).is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn session_credentials_use_opaque_shape_and_clear_stale_router_key() {
        let home = temp_home("session-storage");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");

        let first = sample_session("sub-one", "rls_first", "rlr_first");
        save_session_credentials_from_home("prod", &home, &first, 0.0).expect("save first session");
        save_claude_key_from_home("prod", &home, "rlk-first", 1.0).expect("save router key");

        let credentials = read_json(&credentials_file(&home)).expect("credentials");
        let key = credentials
            .get("router_keys")
            .and_then(Value::as_object)
            .and_then(|keys| keys.get("prod"))
            .and_then(Value::as_object)
            .expect("router key");
        assert_eq!(key.get("apiKey").and_then(value_as_str), Some("rlk-first"));
        assert_eq!(key.get("subject").and_then(value_as_str), Some("sub-one"));

        let second = sample_session("sub-two", "rls_second", "rlr_second");
        let cleared = save_session_credentials_from_home("prod", &home, &second, 2.0)
            .expect("save second session");
        assert!(cleared);
        assert!(load_claude_key_from_home("prod", &home).is_none());

        let credentials = read_json(&credentials_file(&home)).expect("credentials");
        let env_data = credentials
            .get("environments")
            .and_then(Value::as_object)
            .and_then(|envs| envs.get("prod"))
            .and_then(Value::as_object)
            .expect("prod credentials");
        assert_eq!(
            env_data.get("authKind").and_then(value_as_str),
            Some(RAYLINE_SESSION_AUTH_KIND)
        );
        assert_eq!(
            env_data.get("subject").and_then(value_as_str),
            Some("sub-two")
        );
        assert_eq!(
            env_data.get("accessToken").and_then(value_as_str),
            Some("rls_second")
        );
        assert_eq!(
            env_data.get("refreshToken").and_then(value_as_str),
            Some("rlr_second")
        );

        let output = render_status_from_home("prod", &home, None, 10.0);
        assert!(output.contains("Auth source: rayline_session"));

        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn session_login_clears_legacy_router_key_even_for_same_subject() {
        let home = temp_home("session-clears-legacy-key");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");
        let legacy = RefreshedToken {
            id_token: fake_jwt_with_subject("sub-one"),
            refresh_token: "legacy-refresh".to_owned(),
            expires_in: 3600,
        };
        save_env_credentials_from_home("prod", &home, &legacy, "sub-one@example.com", 0.0)
            .expect("save legacy credentials");
        save_claude_key_from_home("prod", &home, "rlk-legacy", 1.0)
            .expect("save legacy router key");

        let session = sample_session("sub-one", "rls_session", "rlr_session");
        let cleared = save_session_credentials_from_home("prod", &home, &session, 2.0)
            .expect("save session credentials");

        assert!(cleared);
        assert!(load_claude_key_from_home("prod", &home).is_none());

        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn resolve_session_token_refreshes_expired_access_token() {
        let home = temp_home("session-refresh");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");

        let expired = SessionToken {
            access_token: "rls_old".to_owned(),
            refresh_token: "rlr_old".to_owned(),
            access_token_expires_at_ms: 1_000.0,
            refresh_token_expires_at_ms: 86_400_000.0,
            subject: "sub-one".to_owned(),
            email: "one@example.com".to_owned(),
        };
        save_session_credentials_from_home("prod", &home, &expired, 0.0)
            .expect("save expired session");

        let outcome = resolve_session_token_from_home_with_refresher(
            "prod",
            &home,
            10.0,
            |refresh, _env| async move {
                assert_eq!(refresh, "rlr_old");
                Ok(SessionToken {
                    access_token: "rls_new".to_owned(),
                    refresh_token: "rlr_new".to_owned(),
                    access_token_expires_at_ms: 3_600_000.0,
                    refresh_token_expires_at_ms: 86_400_000.0,
                    subject: "sub-one".to_owned(),
                    email: "one@example.com".to_owned(),
                })
            },
        )
        .await
        .expect("refresh");

        assert_eq!(outcome, AuthTokenOutcome::Available("rls_new".to_owned()));
        let credentials = read_json(&credentials_file(&home)).expect("credentials");
        let env_data = credentials
            .get("environments")
            .and_then(Value::as_object)
            .and_then(|envs| envs.get("prod"))
            .and_then(Value::as_object)
            .expect("prod credentials");
        assert_eq!(
            env_data.get("accessToken").and_then(value_as_str),
            Some("rls_new")
        );
        assert_eq!(
            env_data.get("refreshToken").and_then(value_as_str),
            Some("rlr_new")
        );

        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn resolve_session_token_refresh_preserves_identity_when_response_omits_it() {
        let home = temp_home("session-refresh-identity");
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(&home).expect("create temp home");

        let expired = SessionToken {
            access_token: "rls_old".to_owned(),
            refresh_token: "rlr_old".to_owned(),
            access_token_expires_at_ms: 1_000.0,
            refresh_token_expires_at_ms: 86_400_000.0,
            subject: "sub-one".to_owned(),
            email: "one@example.com".to_owned(),
        };
        save_session_credentials_from_home("prod", &home, &expired, 0.0)
            .expect("save expired session");

        let outcome = resolve_session_token_from_home_with_refresher(
            "prod",
            &home,
            10.0,
            |refresh, _env| async move {
                assert_eq!(refresh, "rlr_old");
                Ok(SessionToken {
                    access_token: "rls_new".to_owned(),
                    refresh_token: "rlr_new".to_owned(),
                    access_token_expires_at_ms: 3_600_000.0,
                    refresh_token_expires_at_ms: 86_400_000.0,
                    subject: String::new(),
                    email: String::new(),
                })
            },
        )
        .await
        .expect("refresh");

        assert_eq!(outcome, AuthTokenOutcome::Available("rls_new".to_owned()));
        let credentials = read_json(&credentials_file(&home)).expect("credentials");
        let env_data = credentials
            .get("environments")
            .and_then(Value::as_object)
            .and_then(|envs| envs.get("prod"))
            .and_then(Value::as_object)
            .expect("prod credentials");
        assert_eq!(
            env_data.get("subject").and_then(value_as_str),
            Some("sub-one")
        );
        assert_eq!(
            env_data.get("email").and_then(value_as_str),
            Some("one@example.com")
        );

        let _ = fs::remove_dir_all(&home);
    }
}
