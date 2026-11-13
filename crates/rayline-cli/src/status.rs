use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
#[cfg(not(target_os = "windows"))]
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rand::Rng;
use serde_json::Value;
use url::Url;

use crate::auth_callback_page;
use crate::config::{self, ConfigError};
use crate::credentials::{self, Account, Credentials, CredentialsError, RouterKey, Session};

const AUTH_HTTP_TIMEOUT_SECONDS: u64 = 30;
const TOKEN_REFRESH_MARGIN_SECONDS: i64 = 300;
const WEB_CALLBACK_TIMEOUT_SECONDS: u64 = 300;
const SESSION_RESPONSE_TYPE: &str = "rayline_session";
const PROD_LOGIN_URL: &str = "https://platform.rayline.ai/cli-auth";
const ROUTER_KEY_NAME: &str = "rayline-cli";
pub(crate) const PROD_ENV: &str = "prod";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusRequest {
    /// The hosted environment the loaded config selects.
    pub hosted: HostedEnvironment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthTokenRequest {
    /// The hosted environment the loaded config selects.
    pub hosted: HostedEnvironment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLogoutRequest {
    /// The hosted environment the loaded config selects.
    pub hosted: HostedEnvironment,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthLoginRequest {
    /// The hosted environment the loaded config selects.
    pub hosted: HostedEnvironment,
    pub no_browser: bool,
    pub paste: bool,
}

/// How `auth login` gets the session: the browser callback by default,
/// device-code with `--no-browser`, a pasted callback URL with `--paste`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LoginFlow {
    Browser,
    Device,
    Paste,
}

#[derive(Debug)]
pub enum AuthError {
    HomeNotFound(crate::HomeError),
    Config(ConfigError),
    Credentials(CredentialsError),
    InvalidPaste(String),
    LoginFailed(String),
    MintFailed(String),
    RefreshFailed(String),
    /// Logout removed the local session, but revoking it remotely failed:
    /// the environment name and the revoke error.
    RevokeFailed {
        env_name: String,
        error: String,
    },
    SignedOut(String),
    WriteFailed(io::Error),
}

/// A hosted Rayline environment. `env` is `None` for the built-in prod, or
/// the `--env` name whose `environments.<name>` entry the config holds.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostedEnvironment {
    pub env: Option<String>,
    pub base_url: String,
    pub login_url: String,
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HomeNotFound(error) => error.fmt(formatter),
            Self::Config(error) => error.fmt(formatter),
            Self::Credentials(error) => error.fmt(formatter),
            Self::InvalidPaste(message)
            | Self::LoginFailed(message)
            | Self::MintFailed(message)
            | Self::RefreshFailed(message) => formatter.write_str(message),
            Self::RevokeFailed { env_name, error } => write!(
                formatter,
                "Logged out of {env_name} on this machine, but revoking the session failed: {error}"
            ),
            Self::SignedOut(env_name) => write!(
                formatter,
                "Not logged in to {env_name}. Run: {}",
                auth_command()
            ),
            Self::WriteFailed(error) => write!(formatter, "failed to update credentials: {error}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<ConfigError> for AuthError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<CredentialsError> for AuthError {
    fn from(error: CredentialsError) -> Self {
        Self::Credentials(error)
    }
}

impl AuthLoginRequest {
    fn flow(&self) -> LoginFlow {
        if self.paste {
            LoginFlow::Paste
        } else if self.no_browser {
            LoginFlow::Device
        } else {
            LoginFlow::Browser
        }
    }
}

impl HostedEnvironment {
    /// The name shown to the user: the `--env` name, or `prod`.
    pub(crate) fn name(&self) -> &str {
        self.env.as_deref().unwrap_or(PROD_ENV)
    }

    /// The `credentials.json` slot: the top level for prod,
    /// `environments.<name>` otherwise.
    pub(crate) fn slot(&self) -> credentials::Slot<'_> {
        self.env.as_deref()
    }
}

pub fn is_valid_root_env(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn home_dir() -> Result<PathBuf, AuthError> {
    crate::home().map_err(AuthError::HomeNotFound)
}

pub(crate) fn render_status(request: &StatusRequest) -> Result<String, AuthError> {
    let home = home_dir()?;
    let hosted = &request.hosted;
    render_status_from_home(hosted, &home, unix_now())
}

fn render_status_from_home(
    hosted: &HostedEnvironment,
    home: &Path,
    now: i64,
) -> Result<String, AuthError> {
    let credentials = credentials::load(home)?;
    let Some(account) = credentials.account(hosted.slot()) else {
        return Ok(format!("Not logged in. Run: {}\n", auth_command()));
    };
    let session = &account.session;

    let mut output = String::new();
    output.push_str(&format!("  Email:       {}\n", session.email));
    if let Some(env) = &hosted.env {
        output.push_str(&format!("  Environment: {env}\n"));
    }
    let signed_in_at =
        credentials::format_timestamp(session.signed_in_at).map_err(AuthError::WriteFailed)?;
    output.push_str(&format!("  Logged in:   {signed_in_at}\n"));

    let remaining = session.access_token_expires_at - now;
    if remaining > 0 {
        output.push_str(&format!(
            "  Token TTL:   {}m {}s\n",
            remaining / 60,
            remaining % 60
        ));
        let refresh_in = remaining - TOKEN_REFRESH_MARGIN_SECONDS;
        if refresh_in > 0 {
            output.push_str(&format!(
                "  Auto-refresh in: {}m {}s\n",
                refresh_in / 60,
                refresh_in % 60
            ));
        } else {
            output.push_str("  Auto-refresh: on next request\n");
        }
    } else {
        output.push_str("  Token:       expired (will refresh on next use)\n");
    }

    Ok(output)
}

/// `auth token`: the session's access token, refreshed first when it expires
/// within the refresh margin. A signed-out slot is an error.
pub(crate) async fn auth_token(request: &AuthTokenRequest) -> Result<String, AuthError> {
    let home = home_dir()?;
    let hosted = &request.hosted;
    access_token(hosted, &home)
        .await?
        .ok_or_else(|| AuthError::SignedOut(hosted.name().to_owned()))
}

pub(crate) async fn access_token(
    hosted: &HostedEnvironment,
    home: &Path,
) -> Result<Option<String>, AuthError> {
    access_token_at(hosted, home, unix_now()).await
}

async fn access_token_at(
    hosted: &HostedEnvironment,
    home: &Path,
    now: i64,
) -> Result<Option<String>, AuthError> {
    let credentials = credentials::load(home)?;
    let Some(account) = credentials.account(hosted.slot()) else {
        return Ok(None);
    };
    let session = &account.session;
    if session.access_token_expires_at - now > TOKEN_REFRESH_MARGIN_SECONDS {
        return Ok(Some(session.access_token.clone()));
    }

    let used_refresh_token = session.refresh_token.clone();
    let tokens = refresh_rayline_session(&hosted.base_url, &used_refresh_token).await?;

    // Refresh tokens rotate, so write the new session back. Re-read the file
    // first: only the session of this slot changes.
    let mut credentials = credentials::load(home)?;
    let Some(account) = credentials.account_mut(hosted.slot()) else {
        return Err(AuthError::SignedOut(hosted.name().to_owned()));
    };
    if account.session.refresh_token != used_refresh_token {
        return Err(AuthError::RefreshFailed(format!(
            "Token refresh failed: {} changed while refreshing. Run the command again.",
            credentials::credentials_file(home).display()
        )));
    }
    let access_token = tokens.access_token.clone();
    tokens.apply_to(&mut account.session);
    credentials::save(home, &credentials).map_err(AuthError::WriteFailed)?;
    Ok(Some(access_token))
}

/// The router key a harness run uses. A signed-out slot signs in on the spot
/// with the browser flow when stdin and stdout are a terminal, and is an
/// error otherwise.
pub(crate) async fn router_key(
    hosted: &HostedEnvironment,
    home: &Path,
) -> Result<String, AuthError> {
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    router_key_with(hosted, home, interactive).await
}

async fn router_key_with(
    hosted: &HostedEnvironment,
    home: &Path,
    interactive: bool,
) -> Result<String, AuthError> {
    if let Some(api_key) = stored_router_key(hosted, home)? {
        return Ok(api_key);
    }
    if !interactive {
        return Err(AuthError::SignedOut(hosted.name().to_owned()));
    }
    eprintln!("Signing in to {}...", crate::DISPLAY_NAME);
    let message = login(hosted, home, LoginFlow::Browser).await?;
    write_auth_message(&message).map_err(AuthError::WriteFailed)?;
    stored_router_key(hosted, home)?.ok_or_else(|| AuthError::SignedOut(hosted.name().to_owned()))
}

/// The router key stored for `hosted`, without signing in.
pub(crate) fn stored_router_key(
    hosted: &HostedEnvironment,
    home: &Path,
) -> Result<Option<String>, AuthError> {
    Ok(credentials::load(home)?
        .account(hosted.slot())
        .map(|account| account.router_key.api_key.clone()))
}

pub async fn logout(request: &AuthLogoutRequest) -> Result<String, AuthError> {
    let home = home_dir()?;
    let hosted = &request.hosted;
    logout_from_home(hosted, &home).await
}

async fn logout_from_home(hosted: &HostedEnvironment, home: &Path) -> Result<String, AuthError> {
    let mut credentials = credentials::load(home)?;
    let Some(account) = credentials.remove_account(hosted.slot()) else {
        return Ok(format!("Not logged in to {}\n", hosted.name()));
    };
    credentials::save(home, &credentials).map_err(AuthError::WriteFailed)?;

    revoke_rayline_session(&hosted.base_url, &account.session.refresh_token)
        .await
        .map_err(|error| AuthError::RevokeFailed {
            env_name: hosted.name().to_owned(),
            error,
        })?;
    Ok(format!("Logged out ({})\n", hosted.name()))
}

pub async fn auth_login(request: &AuthLoginRequest) -> Result<String, AuthError> {
    let home = home_dir()?;
    let hosted = &request.hosted;
    login(hosted, &home, request.flow()).await
}

async fn login(
    hosted: &HostedEnvironment,
    home: &Path,
    flow: LoginFlow,
) -> Result<String, AuthError> {
    let grant = match flow {
        LoginFlow::Paste => run_session_paste_flow(hosted).await?,
        LoginFlow::Device => run_rayline_device_login(hosted).await?,
        LoginFlow::Browser => run_web_callback_session_flow(hosted).await?,
    };
    save_login(hosted, home, grant, unix_now()).await
}

/// Mint the router key with the new session and write `session` and
/// `router_key` to the slot in one write. A file that fails to load as
/// invalid is replaced: signing in is how the user repairs it.
async fn save_login(
    hosted: &HostedEnvironment,
    home: &Path,
    grant: SessionGrant,
    now: i64,
) -> Result<String, AuthError> {
    let api_key = mint_router_key(&hosted.base_url, &grant.tokens.access_token).await?;
    let (mut credentials, replaced) = match credentials::load(home) {
        Ok(credentials) => (credentials, None),
        Err(CredentialsError::Invalid { path, .. }) => (Credentials::default(), Some(path)),
        Err(error) => return Err(error.into()),
    };
    let message = login_success_message(hosted.name(), &grant.email);
    let router_key = RouterKey {
        api_key,
        subject: grant.subject.clone(),
        saved_at: now,
    };
    let session = Session {
        email: grant.email,
        subject: grant.subject,
        access_token: grant.tokens.access_token,
        access_token_expires_at: grant.tokens.access_token_expires_at,
        refresh_token: grant.tokens.refresh_token,
        refresh_token_expires_at: grant.tokens.refresh_token_expires_at,
        signed_in_at: now,
    };
    credentials.set_account(
        hosted.slot(),
        Account {
            session,
            router_key,
        },
    );
    credentials::save(home, &credentials).map_err(AuthError::WriteFailed)?;
    Ok(match replaced {
        Some(path) => format!("{message}Replaced invalid {}.\n", path.display()),
        None => message,
    })
}

async fn run_session_paste_flow(hosted: &HostedEnvironment) -> Result<SessionGrant, AuthError> {
    let state = random_state();
    let (code_verifier, code_challenge) = generate_pkce();
    let auth_url = login_page_url(hosted, &state, "http://127.0.0.1/", &code_challenge);
    eprintln!();
    eprintln!("  To sign in, open this URL in any browser:");
    eprintln!();
    eprintln!("  {auth_url}");
    eprintln!();
    eprintln!("  After sign-in, copy the final http://127.0.0.1/... URL from");
    eprintln!("  the browser address bar and paste it here.");
    eprintln!();
    eprint!("  Paste the callback URL: ");
    io::stderr().flush().map_err(AuthError::WriteFailed)?;

    let mut pasted = String::new();
    io::stdin()
        .read_line(&mut pasted)
        .map_err(AuthError::WriteFailed)?;
    let code = parse_paste_callback_url(pasted.trim(), &state)?;
    exchange_cli_code_for_session(hosted, &code, &code_verifier).await
}

async fn run_web_callback_session_flow(
    hosted: &HostedEnvironment,
) -> Result<SessionGrant, AuthError> {
    let (code, code_verifier) = run_web_callback_code_flow(hosted).await?;
    exchange_cli_code_for_session(hosted, &code, &code_verifier).await
}

async fn run_web_callback_code_flow(
    hosted: &HostedEnvironment,
) -> Result<(String, String), AuthError> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
        AuthError::LoginFailed(format!("Failed to start local login callback: {error}"))
    })?;
    let port = listener
        .local_addr()
        .map_err(|error| {
            AuthError::LoginFailed(format!("Failed to read local login callback port: {error}"))
        })?
        .port();
    let callback_url = format!("http://127.0.0.1:{port}/");
    let state = random_state();
    let (code_verifier, code_challenge) = generate_pkce();
    let auth_url = login_page_url(hosted, &state, &callback_url, &code_challenge);

    eprintln!(
        "  Opening browser for authentication ({})...",
        hosted.name()
    );
    eprintln!("  If it doesn't open, visit this URL:\n");
    eprintln!("  {auth_url}\n");
    eprintln!("  Waiting for login (timeout: {WEB_CALLBACK_TIMEOUT_SECONDS}s)...");
    open_browser(&auth_url);

    // The browser navigates the loopback to `/?code=...&state=...`; we receive a
    // one-time, PKCE-bound code (never a credential), then redeem it over TLS.
    let code = tokio::task::spawn_blocking(move || wait_for_callback(listener, &state))
        .await
        .map_err(|error| AuthError::LoginFailed(format!("Login callback failed: {error}")))??;

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
) -> Result<SessionGrant, AuthError> {
    let url = format!("{}/v1/auth/cli/token", hosted.base_url);
    let client = auth_http_client()
        .map_err(|error| AuthError::LoginFailed(format!("CLI code exchange failed: {error}")))?;
    let response = client
        .post(url)
        .json(&serde_json::json!({
            "code": code,
            "codeVerifier": code_verifier,
            "responseType": SESSION_RESPONSE_TYPE
        }))
        .send()
        .await
        .map_err(|error| AuthError::LoginFailed(format!("CLI code exchange failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthError::LoginFailed(http_error_message(
            "CLI code exchange failed",
            status.as_u16(),
            &body,
        )));
    }
    let data: Value = response
        .json()
        .await
        .map_err(|error| AuthError::LoginFailed(format!("CLI code exchange failed: {error}")))?;
    parse_session_grant(&data, "CLI code exchange").map_err(AuthError::LoginFailed)
}

async fn run_rayline_device_login(hosted: &HostedEnvironment) -> Result<SessionGrant, AuthError> {
    let device = request_rayline_device_session(hosted).await?;
    write_rayline_device_login_prompt(&device)?;
    poll_rayline_device_session(hosted, &device).await
}

async fn request_rayline_device_session(
    hosted: &HostedEnvironment,
) -> Result<RaylineDeviceSession, AuthError> {
    let url = format!("{}/v1/auth/cli/device/start", hosted.base_url);
    let response = auth_http_client()
        .map_err(|error| AuthError::LoginFailed(format!("Device login start failed: {error}")))?
        .post(url)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|error| AuthError::LoginFailed(format!("Device login start failed: {error}")))?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthError::LoginFailed(http_error_message(
            "Device login start failed",
            status.as_u16(),
            &body,
        )));
    }
    let data: Value = response
        .json()
        .await
        .map_err(|error| AuthError::LoginFailed(format!("Device login start failed: {error}")))?;
    Ok(RaylineDeviceSession {
        session_id: required_login_string(&data, "sessionId")?,
        user_code: required_login_string(&data, "userCode")?,
        verification_uri: required_login_string(&data, "verificationUri")?,
        expires_in_seconds: required_login_seconds(&data, "expiresInSeconds")?,
    })
}

fn write_rayline_device_login_prompt(device: &RaylineDeviceSession) -> Result<(), AuthError> {
    write_auth_message(&rayline_device_login_prompt(device)).map_err(AuthError::WriteFailed)
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
) -> Result<SessionGrant, AuthError> {
    let url = format!("{}/v1/auth/cli/device/poll", hosted.base_url);
    let started = Instant::now();
    let timeout = Duration::from_secs(device.expires_in_seconds);
    let client = auth_http_client()
        .map_err(|error| AuthError::LoginFailed(format!("Device login polling failed: {error}")))?;

    loop {
        if started.elapsed() >= timeout {
            return Err(AuthError::LoginFailed(format!(
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
                AuthError::LoginFailed(format!("Device login polling failed: {error}"))
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(AuthError::LoginFailed(http_error_message(
                "Device login polling failed",
                status.as_u16(),
                &body,
            )));
        }

        let data: Value = serde_json::from_str(&body).map_err(|error| {
            AuthError::LoginFailed(format!("Device login polling failed: {error}"))
        })?;
        match data
            .get("status")
            .and_then(value_as_str)
            .unwrap_or_default()
        {
            "approved" => {
                return parse_session_grant(&data, "Device login polling")
                    .map_err(AuthError::LoginFailed);
            }
            "pending" => {
                let interval = required_login_seconds(&data, "intervalSeconds")?;
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
            "expired" => {
                return Err(AuthError::LoginFailed(format!(
                    "Login code expired before approval. Run `{} auth login --no-browser` to try again.",
                    crate::CLI_BIN
                )));
            }
            other => {
                return Err(AuthError::LoginFailed(format!(
                    "Device login polling failed: unexpected status '{}'",
                    if other.is_empty() { "(missing)" } else { other }
                )));
            }
        }
    }
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

/// The environment the loaded config selects: the built-in prod without
/// `--env`, else the config's `environments.<name>` entry, which the load
/// checked exists.
pub(crate) fn hosted_environment(settings: &config::Settings) -> HostedEnvironment {
    match &settings.env {
        None => prod_hosted_environment(),
        Some(name) => configured_hosted_environment(name, &settings.environments[name]),
    }
}

fn prod_hosted_environment() -> HostedEnvironment {
    HostedEnvironment {
        env: None,
        base_url: crate::ROUTER_PROD_URL.to_owned(),
        login_url: PROD_LOGIN_URL.to_owned(),
    }
}

fn configured_hosted_environment(
    name: &str,
    environment: &config::Environment,
) -> HostedEnvironment {
    HostedEnvironment {
        env: Some(name.to_owned()),
        base_url: environment
            .base_url
            .as_str()
            .trim_end_matches('/')
            .to_owned(),
        login_url: environment.login_url.to_string(),
    }
}

fn login_page_url(
    hosted: &HostedEnvironment,
    state: &str,
    callback_url: &str,
    code_challenge: &str,
) -> String {
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair("state", state);
    query.append_pair("callback", callback_url);
    query.append_pair("challenge", code_challenge);
    format!("{}?{}", hosted.login_url, query.finish())
}

fn parse_paste_callback_url(pasted: &str, expected_state: &str) -> Result<String, AuthError> {
    if pasted.starts_with('?') {
        return Err(AuthError::InvalidPaste(
            "Please paste the full callback URL, not just the ?query string.".to_owned(),
        ));
    }

    let parsed = Url::parse(pasted).map_err(|_| {
        AuthError::InvalidPaste(
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
        return Err(AuthError::InvalidPaste(
            "State mismatch: the pasted URL does not belong to this login session.".to_owned(),
        ));
    }
    if code.is_empty() {
        return Err(AuthError::InvalidPaste(
            "Could not extract login code from the pasted URL.".to_owned(),
        ));
    }
    Ok(code)
}

fn wait_for_callback(listener: TcpListener, expected_state: &str) -> Result<String, AuthError> {
    listener.set_nonblocking(true).map_err(|error| {
        AuthError::LoginFailed(format!("Failed to configure login callback: {error}"))
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
                    return Err(AuthError::LoginFailed(format!(
                        "Login timed out ({WEB_CALLBACK_TIMEOUT_SECONDS}s). Try again."
                    )));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                return Err(AuthError::LoginFailed(format!(
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
) -> Result<Option<String>, AuthError> {
    let request = read_http_request(stream)?;
    match request.method.as_str() {
        "GET" => match parse_callback_query(&request.target, expected_state) {
            Ok(Some(code)) => {
                respond_html(stream, 200, "OK", &auth_callback_page::success_html())
                    .map_err(AuthError::WriteFailed)?;
                Ok(Some(code))
            }
            Ok(None) => {
                respond_html(stream, 200, "OK", &auth_callback_page::waiting_html())
                    .map_err(AuthError::WriteFailed)?;
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
                .map_err(AuthError::WriteFailed)?;
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
            .map_err(AuthError::WriteFailed)?;
            Ok(None)
        }
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, AuthError> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(AuthError::WriteFailed)?;

    let mut buffer = Vec::new();
    let mut temp = [0u8; 1024];
    loop {
        let read = stream.read(&mut temp).map_err(AuthError::WriteFailed)?;
        if read == 0 {
            return Err(AuthError::LoginFailed(
                "Login callback closed before sending headers.".to_owned(),
            ));
        }
        buffer.extend_from_slice(&temp[..read]);
        if buffer.len() > 64 * 1024 {
            return Err(AuthError::LoginFailed(
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
    format!("Logged in as {email} ({env_name})\n")
}

fn required_login_string(data: &Value, key: &str) -> Result<String, AuthError> {
    data.get(key)
        .and_then(value_as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| AuthError::LoginFailed(format!("Login response missing {key}")))
}

fn required_login_seconds(data: &Value, key: &str) -> Result<u64, AuthError> {
    let value = data
        .get(key)
        .ok_or_else(|| AuthError::LoginFailed(format!("Login response missing {key}")))?;
    value
        .as_u64()
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| {
            AuthError::LoginFailed(format!(
                "Login response {key} must be a positive whole number of seconds, got {value}"
            ))
        })
}

fn required_response_string(data: &Value, key: &str, context: &str) -> Result<String, String> {
    data.get(key)
        .and_then(value_as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("{context} failed: missing {key}"))
}

fn required_response_seconds(data: &Value, key: &str, context: &str) -> Result<i64, String> {
    data.get(key)
        .and_then(Value::as_f64)
        .filter(|milliseconds| milliseconds.is_finite())
        .map(|milliseconds| (milliseconds / 1000.0).floor() as i64)
        .ok_or_else(|| format!("{context} failed: missing {key}"))
}

fn parse_session_tokens(data: &Value, context: &str) -> Result<SessionTokens, String> {
    Ok(SessionTokens {
        access_token: required_response_string(data, "accessToken", context)?,
        access_token_expires_at: required_response_seconds(
            data,
            "accessTokenExpiresAtMs",
            context,
        )?,
        refresh_token: required_response_string(data, "refreshToken", context)?,
        refresh_token_expires_at: required_response_seconds(
            data,
            "refreshTokenExpiresAtMs",
            context,
        )?,
    })
}

fn parse_session_grant(data: &Value, context: &str) -> Result<SessionGrant, String> {
    Ok(SessionGrant {
        email: required_response_string(data, "email", context)?,
        subject: required_response_string(data, "subject", context)?,
        tokens: parse_session_tokens(data, context)?,
    })
}

async fn refresh_rayline_session(
    base_url: &str,
    refresh_token: &str,
) -> Result<SessionTokens, AuthError> {
    let url = format!("{base_url}/v1/auth/cli/refresh");
    let response = auth_http_client()
        .map_err(|error| AuthError::RefreshFailed(format!("Token refresh failed: {error}")))?
        .post(url)
        .json(&serde_json::json!({ "refreshToken": refresh_token }))
        .send()
        .await
        .map_err(|error| {
            AuthError::RefreshFailed(format!(
                "Token refresh failed: {}",
                reqwest_error_message(error)
            ))
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthError::RefreshFailed(http_error_message(
            "Token refresh failed",
            status.as_u16(),
            &body,
        )));
    }

    let data: Value = response.json().await.map_err(|error| {
        AuthError::RefreshFailed(format!(
            "Token refresh failed: {}",
            reqwest_error_message(error)
        ))
    })?;
    parse_session_tokens(&data, "Token refresh").map_err(AuthError::RefreshFailed)
}

async fn revoke_rayline_session(base_url: &str, token: &str) -> Result<(), String> {
    let url = format!("{base_url}/v1/auth/cli/revoke");
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

fn reqwest_error_message(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

async fn mint_router_key(base_url: &str, access_token: &str) -> Result<String, AuthError> {
    let url = format!("{base_url}/v1/keys");
    let response = auth_http_client()
        .map_err(|error| AuthError::MintFailed(format!("Failed to reach router: {error}")))?
        .post(url)
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "name": ROUTER_KEY_NAME }))
        .send()
        .await
        .map_err(|error| {
            AuthError::MintFailed(format!(
                "Failed to reach router: {}",
                reqwest_error_message(error)
            ))
        })?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(AuthError::MintFailed(http_error_message(
            &format!("{} router key request failed", crate::DISPLAY_NAME),
            status.as_u16(),
            &body,
        )));
    }

    let body: Value = response.json().await.map_err(|error| {
        AuthError::MintFailed(format!(
            "Unexpected router key response: {}",
            reqwest_error_message(error)
        ))
    })?;
    if let Some(error) = body.get("error") {
        return Err(AuthError::MintFailed(format!(
            "Router key request failed: {}",
            json_value_for_message(error)
        )));
    }
    body.get("apiKey")
        .and_then(value_as_str)
        .filter(|api_key| !api_key.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| {
            AuthError::MintFailed("Unexpected router key response: missing apiKey".to_owned())
        })
}

pub(crate) fn write_json_atomic(path: &Path, value: &Value) -> io::Result<()> {
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

fn json_value_for_message(value: &Value) -> String {
    match value {
        Value::String(message) => message.clone(),
        _ => value.to_string(),
    }
}

fn auth_command() -> String {
    format!("{} auth login", crate::CLI_BIN)
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs() as i64)
}

/// A new session from sign-in.
struct SessionGrant {
    email: String,
    subject: String,
    tokens: SessionTokens,
}

/// The token half of a session, as sign-in and refresh return it.
struct SessionTokens {
    access_token: String,
    access_token_expires_at: i64,
    refresh_token: String,
    refresh_token_expires_at: i64,
}

impl SessionTokens {
    fn apply_to(self, session: &mut Session) {
        session.access_token = self.access_token;
        session.access_token_expires_at = self.access_token_expires_at;
        session.refresh_token = self.refresh_token;
        session.refresh_token_expires_at = self.refresh_token_expires_at;
    }
}

#[derive(Debug, Eq, PartialEq)]
struct RaylineDeviceSession {
    session_id: String,
    user_code: String,
    verification_uri: String,
    expires_in_seconds: u64,
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
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;

    const ACCESS_TOKEN: &str = "access-secret-value";
    const REFRESH_TOKEN: &str = "refresh-secret-value";
    const API_KEY: &str = "rlk-secret-value";
    const NOW: i64 = 1_794_477_600;
    const LATER: i64 = NOW + 3_600;
    /// Nothing listens here, so a request a test must not make fails.
    const UNREACHABLE: &str = "http://127.0.0.1:9";

    /// One request the fake API received.
    #[derive(Clone, Debug)]
    struct Received {
        method: String,
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    /// A fake Rayline API on `127.0.0.1:0`. It answers each path with a fixed
    /// status and JSON body (404 for any other path) and records every request.
    struct FakeApi {
        base_url: String,
        received: Arc<Mutex<Vec<Received>>>,
    }

    impl FakeApi {
        fn start(routes: &[(&'static str, u16, Value)]) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let routes: HashMap<&'static str, (u16, Value)> = routes
                .iter()
                .map(|(path, status, body)| (*path, (*status, body.clone())))
                .collect();
            let received = Arc::new(Mutex::new(Vec::new()));
            let log = Arc::clone(&received);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else {
                        return;
                    };
                    let request = read_request(&mut stream);
                    let (status, body) = routes
                        .get(request.path.as_str())
                        .cloned()
                        .unwrap_or((404, json!({ "error": "not found" })));
                    log.lock().unwrap().push(request);
                    let body = body.to_string();
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} Fake\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                }
            });
            Self { base_url, received }
        }

        fn received(&self) -> Vec<Received> {
            self.received.lock().unwrap().clone()
        }

        fn paths(&self) -> Vec<String> {
            self.received()
                .into_iter()
                .map(|request| format!("{} {}", request.method, request.path))
                .collect()
        }
    }

    fn read_request(stream: &mut TcpStream) -> Received {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "connection closed before the headers");
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(end) = find_bytes(&buffer, b"\r\n\r\n") {
                break end + 4;
            }
        };
        let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
        let mut lines = head.lines();
        let mut request_line = lines.next().unwrap().split_whitespace();
        let method = request_line.next().unwrap().to_owned();
        let path = request_line.next().unwrap().to_owned();
        let mut authorization = None;
        let mut length = 0;
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                match name.trim().to_ascii_lowercase().as_str() {
                    "authorization" => authorization = Some(value.trim().to_owned()),
                    "content-length" => length = value.trim().parse().unwrap(),
                    _ => {}
                }
            }
        }
        while buffer.len() < header_end + length {
            let read = stream.read(&mut chunk).unwrap();
            assert!(read > 0, "connection closed before the body");
            buffer.extend_from_slice(&chunk[..read]);
        }
        let body = if length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&buffer[header_end..header_end + length]).unwrap()
        };
        Received {
            method,
            path,
            authorization,
            body,
        }
    }

    fn temp_home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("rayline-status-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&home);
        fs::create_dir_all(home.join(".config").join(crate::CONFIG_DIR)).unwrap();
        home
    }

    fn settings_file(home: &Path) -> PathBuf {
        home.join(".config")
            .join(crate::CONFIG_DIR)
            .join("settings.json")
    }

    fn write_settings_json(home: &Path, settings: &Value) {
        fs::write(settings_file(home), settings.to_string()).unwrap();
    }

    fn hosted(env: Option<&str>, base_url: &str) -> HostedEnvironment {
        HostedEnvironment {
            env: env.map(ToOwned::to_owned),
            base_url: base_url.to_owned(),
            login_url: format!("{base_url}/cli-auth"),
        }
    }

    fn account(access_token_expires_at: i64) -> Account {
        Account {
            session: Session {
                email: "you@example.com".to_owned(),
                subject: "uid_test".to_owned(),
                access_token: ACCESS_TOKEN.to_owned(),
                access_token_expires_at,
                refresh_token: REFRESH_TOKEN.to_owned(),
                refresh_token_expires_at: NOW + 86_400,
                signed_in_at: NOW - 60,
            },
            router_key: RouterKey {
                api_key: API_KEY.to_owned(),
                subject: "uid_test".to_owned(),
                saved_at: NOW - 60,
            },
        }
    }

    fn save_account(home: &Path, slot: credentials::Slot<'_>, account: Account) {
        let mut stored = credentials::load(home).unwrap();
        stored.set_account(slot, account);
        credentials::save(home, &stored).unwrap();
    }

    fn stored(home: &Path, slot: credentials::Slot<'_>) -> Option<Account> {
        credentials::load(home).unwrap().account(slot).cloned()
    }

    fn credentials_text(home: &Path) -> Option<String> {
        fs::read_to_string(credentials::credentials_file(home)).ok()
    }

    fn write_credentials_text(home: &Path, text: &str) {
        fs::write(credentials::credentials_file(home), text).unwrap();
    }

    fn session_tokens_body(access_token: &str, refresh_token: &str) -> Value {
        json!({
            "accessToken": access_token,
            "accessTokenExpiresAtMs": LATER * 1000,
            "refreshToken": refresh_token,
            "refreshTokenExpiresAtMs": (LATER + 86_400) * 1000,
        })
    }

    /// The device flow: start, an approved poll, then the router key mint.
    fn device_login_routes(keys_status: u16, keys_body: Value) -> Vec<(&'static str, u16, Value)> {
        let mut approved = session_tokens_body("new-access", "new-refresh");
        approved["status"] = json!("approved");
        approved["email"] = json!("new@example.com");
        approved["subject"] = json!("uid_new");
        vec![
            (
                "/v1/auth/cli/device/start",
                200,
                json!({
                    "sessionId": "device-session",
                    "userCode": "ABCD-EFGH",
                    "verificationUri": "https://platform.example.test/device",
                    "intervalSeconds": 1,
                    "expiresInSeconds": 60,
                }),
            ),
            ("/v1/auth/cli/device/poll", 200, approved),
            ("/v1/keys", keys_status, keys_body),
        ]
    }

    /// The environment a startup load with `--env env` selects.
    fn resolve(env: Option<&str>, home: &Path) -> Result<HostedEnvironment, ConfigError> {
        config::load_in(
            home,
            &config::LoadOptions {
                mode: config::Mode::Cloud,
                env,
                harness_protocol: None,
            },
        )
        .map(|settings| hosted_environment(&settings))
    }

    #[test]
    fn no_env_is_the_built_in_prod() {
        let home = temp_home("resolve-prod");
        // A settings entry named prod is only for `--env prod`.
        write_settings_json(
            &home,
            &json!({"environments": {"prod": {
                "base_url": "https://other.example.test",
                "login_url": "https://other.example.test/cli-auth"
            }}}),
        );
        let prod = resolve(None, &home).unwrap();
        assert_eq!(
            prod,
            HostedEnvironment {
                env: None,
                base_url: "https://api.rayline.ai".to_owned(),
                login_url: "https://platform.rayline.ai/cli-auth".to_owned(),
            }
        );
        assert_eq!((prod.name(), prod.slot()), ("prod", None));

        let explicit = resolve(Some("prod"), &home).unwrap();
        assert_eq!(explicit.base_url, "https://other.example.test");
        assert_eq!((explicit.name(), explicit.slot()), ("prod", Some("prod")));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn an_env_is_exactly_its_base_url_and_login_url() {
        let home = temp_home("resolve-dev");
        write_settings_json(
            &home,
            &json!({"environments": {"dev": {
                "base_url": "http://127.0.0.1:8080/",
                "login_url": "https://dev.example.test/cli-auth"
            }}}),
        );
        let dev = resolve(Some("dev"), &home).unwrap();
        assert_eq!(
            dev,
            HostedEnvironment {
                env: Some("dev".to_owned()),
                base_url: "http://127.0.0.1:8080".to_owned(),
                login_url: "https://dev.example.test/cli-auth".to_owned(),
            }
        );
        assert_eq!((dev.name(), dev.slot()), ("dev", Some("dev")));
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn a_wrong_settings_file_is_a_load_error_for_every_env() {
        let home = temp_home("resolve-errors");
        let key_error = |env: Option<&str>| match resolve(env, &home) {
            Err(ConfigError::Key { key, .. }) => key,
            other => panic!("{other:?}"),
        };

        assert_eq!(key_error(Some("dev")), "environments.dev");
        write_settings_json(
            &home,
            &json!({"environments": {"dev": {
                "base_url": "https://api-dev.example.test",
                "login_url": "https://dev.example.test/cli-auth",
                "router_url": "https://api-dev.example.test"
            }}}),
        );
        assert_eq!(key_error(Some("dev")), "environments.dev.router_url");
        assert_eq!(key_error(None), "environments.dev.router_url");
        write_settings_json(&home, &json!({"default_model": "arc-sage"}));
        assert_eq!(key_error(None), "default_model");

        fs::write(settings_file(&home), "{\"secret-value\"").unwrap();
        let error = resolve(None, &home).unwrap_err();
        assert!(matches!(error, ConfigError::Parse { .. }), "{error:?}");
        assert!(!error.to_string().contains("secret-value"), "{error}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn status_shows_the_selected_environments_slot() {
        let home = temp_home("status");
        let prod = hosted(None, UNREACHABLE);
        let dev = hosted(Some("dev"), UNREACHABLE);
        let signed_out = format!("Not logged in. Run: {} auth login\n", crate::CLI_BIN);
        assert_eq!(
            render_status_from_home(&prod, &home, NOW).unwrap(),
            signed_out
        );

        save_account(&home, None, account(LATER));
        let output = render_status_from_home(&prod, &home, NOW).unwrap();
        assert!(
            output.contains("  Email:       you@example.com\n"),
            "{output}"
        );
        assert!(
            output.contains("  Logged in:   2026-11-12T09:59:00Z\n"),
            "{output}"
        );
        assert!(output.contains("  Token TTL:   60m 0s\n"), "{output}");
        assert!(!output.contains("Environment:"), "{output}");
        assert!(
            !output.contains(ACCESS_TOKEN) && !output.contains(API_KEY),
            "{output}"
        );
        assert_eq!(
            render_status_from_home(&dev, &home, NOW).unwrap(),
            signed_out
        );

        save_account(&home, Some("dev"), account(NOW - 1));
        let output = render_status_from_home(&dev, &home, NOW).unwrap();
        assert!(output.contains("  Environment: dev\n"), "{output}");
        assert!(output.contains("  Token:       expired"), "{output}");
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn status_of_an_invalid_credentials_file_is_a_load_error() {
        let home = temp_home("status-invalid");
        write_credentials_text(&home, r#"{"session":{"api_key":"rlk-secret-value"}}"#);
        let error = render_status_from_home(&hosted(None, UNREACHABLE), &home, NOW).unwrap_err();
        assert!(
            matches!(
                error,
                AuthError::Credentials(CredentialsError::Invalid { .. })
            ),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains("auth login") && !message.contains("secret-value"),
            "{message}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_valid_token_needs_no_network() {
        let home = temp_home("token-valid");
        save_account(&home, None, account(LATER));
        let token = access_token_at(&hosted(None, UNREACHABLE), &home, NOW).await;
        assert_eq!(token.unwrap().as_deref(), Some(ACCESS_TOKEN));
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn an_expiring_token_is_refreshed_and_only_its_session_rewritten() {
        let api = FakeApi::start(&[(
            "/v1/auth/cli/refresh",
            200,
            session_tokens_body("new-access", "new-refresh"),
        )]);
        let home = temp_home("token-refresh");
        save_account(&home, None, account(NOW + 60));
        save_account(&home, Some("dev"), account(NOW + 60));
        let dev_before = stored(&home, Some("dev")).unwrap();

        let token = access_token_at(&hosted(None, &api.base_url), &home, NOW).await;
        assert_eq!(token.unwrap().as_deref(), Some("new-access"));

        let requests = api.received();
        assert_eq!(api.paths(), ["POST /v1/auth/cli/refresh"]);
        assert_eq!(requests[0].body, json!({ "refreshToken": REFRESH_TOKEN }));
        let prod = stored(&home, None).unwrap();
        let mut expected = account(NOW + 60);
        expected.session.access_token = "new-access".to_owned();
        expected.session.access_token_expires_at = LATER;
        expected.session.refresh_token = "new-refresh".to_owned();
        expected.session.refresh_token_expires_at = LATER + 86_400;
        assert_eq!(prod, expected);
        assert_eq!(stored(&home, Some("dev")).unwrap(), dev_before);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_failed_refresh_leaves_the_file_unchanged() {
        let api = FakeApi::start(&[(
            "/v1/auth/cli/refresh",
            401,
            json!({ "error": "invalid_grant" }),
        )]);
        let home = temp_home("token-refresh-fails");
        save_account(&home, None, account(NOW - 1));
        let before = credentials_text(&home);

        let error = access_token_at(&hosted(None, &api.base_url), &home, NOW)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::RefreshFailed(_)), "{error:?}");
        assert!(error.to_string().contains("HTTP 401"), "{error}");
        assert_eq!(credentials_text(&home), before);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn signed_out_has_no_token_and_an_invalid_file_is_an_error() {
        let home = temp_home("token-signed-out");
        let prod = hosted(None, UNREACHABLE);
        assert_eq!(access_token_at(&prod, &home, NOW).await.unwrap(), None);
        save_account(&home, Some("dev"), account(LATER));
        assert_eq!(access_token_at(&prod, &home, NOW).await.unwrap(), None);

        write_credentials_text(&home, "{\"session\": {}}");
        let error = access_token_at(&prod, &home, NOW).await.unwrap_err();
        assert!(
            matches!(
                error,
                AuthError::Credentials(CredentialsError::Invalid { .. })
            ),
            "{error:?}"
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn device_login_stores_the_session_and_minted_key_in_its_slot() {
        let api = FakeApi::start(&device_login_routes(200, json!({ "apiKey": "rlk-new" })));
        let home = temp_home("login-device");
        save_account(&home, None, account(LATER));
        let prod_before = stored(&home, None).unwrap();

        let message = login(
            &hosted(Some("dev"), &api.base_url),
            &home,
            LoginFlow::Device,
        )
        .await
        .unwrap();
        assert_eq!(message, "Logged in as new@example.com (dev)\n");

        let requests = api.received();
        assert_eq!(
            api.paths(),
            [
                "POST /v1/auth/cli/device/start",
                "POST /v1/auth/cli/device/poll",
                "POST /v1/keys",
            ]
        );
        assert_eq!(requests[1].body, json!({ "sessionId": "device-session" }));
        assert_eq!(
            requests[2].authorization.as_deref(),
            Some("Bearer new-access")
        );
        assert_eq!(requests[2].body, json!({ "name": "rayline-cli" }));

        let dev = stored(&home, Some("dev")).unwrap();
        assert_eq!(dev.session.email, "new@example.com");
        assert_eq!(dev.session.subject, "uid_new");
        assert_eq!(dev.session.access_token, "new-access");
        assert_eq!(dev.session.access_token_expires_at, LATER);
        assert_eq!(dev.session.refresh_token, "new-refresh");
        assert_eq!(dev.session.refresh_token_expires_at, LATER + 86_400);
        assert_eq!(dev.router_key.api_key, "rlk-new");
        assert_eq!(dev.router_key.subject, "uid_new");
        // Written together: the key is saved when the session signs in.
        assert_eq!(dev.router_key.saved_at, dev.session.signed_in_at);
        assert_eq!(stored(&home, None).unwrap(), prod_before);
        let _ = fs::remove_dir_all(&home);
    }

    fn device_start_body(expires_in_seconds: Option<Value>) -> Value {
        let mut body = json!({
            "sessionId": "device-session",
            "userCode": "ABCD-EFGH",
            "verificationUri": "https://platform.example.test/device",
            "intervalSeconds": 1,
        });
        if let Some(value) = expires_in_seconds {
            body["expiresInSeconds"] = value;
        }
        body
    }

    fn login_failure(error: AuthError) -> String {
        match error {
            AuthError::LoginFailed(message) => message,
            other => panic!("expected LoginFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn device_start_requires_expires_in_seconds() {
        let api = FakeApi::start(&[("/v1/auth/cli/device/start", 200, device_start_body(None))]);
        let error = request_rayline_device_session(&hosted(None, &api.base_url))
            .await
            .unwrap_err();
        assert_eq!(
            login_failure(error),
            "Login response missing expiresInSeconds"
        );
    }

    #[tokio::test]
    async fn device_start_refuses_expires_in_seconds_that_is_not_whole_positive_seconds() {
        for value in [json!(0), json!(-5), json!("600"), json!(1.5), Value::Null] {
            let api = FakeApi::start(&[(
                "/v1/auth/cli/device/start",
                200,
                device_start_body(Some(value.clone())),
            )]);
            let error = request_rayline_device_session(&hosted(None, &api.base_url))
                .await
                .unwrap_err();
            assert_eq!(
                login_failure(error),
                format!(
                    "Login response expiresInSeconds must be a positive whole number of seconds, got {value}"
                )
            );
        }
    }

    #[tokio::test]
    async fn device_start_reads_expires_in_seconds() {
        let api = FakeApi::start(&[(
            "/v1/auth/cli/device/start",
            200,
            device_start_body(Some(json!(600))),
        )]);
        let device = request_rayline_device_session(&hosted(None, &api.base_url))
            .await
            .unwrap();
        assert_eq!(
            device,
            RaylineDeviceSession {
                session_id: "device-session".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://platform.example.test/device".to_string(),
                expires_in_seconds: 600,
            }
        );
    }

    #[tokio::test]
    async fn a_pending_poll_requires_interval_seconds() {
        for (body, expected) in [
            (
                json!({ "status": "pending" }),
                "Login response missing intervalSeconds".to_string(),
            ),
            (
                json!({ "status": "pending", "intervalSeconds": 0 }),
                "Login response intervalSeconds must be a positive whole number of seconds, got 0"
                    .to_string(),
            ),
            (
                json!({ "status": "pending", "intervalSeconds": "5" }),
                "Login response intervalSeconds must be a positive whole number of seconds, got \"5\""
                    .to_string(),
            ),
        ] {
            let api = FakeApi::start(&[("/v1/auth/cli/device/poll", 200, body)]);
            let device = RaylineDeviceSession {
                session_id: "device-session".to_string(),
                user_code: "ABCD-EFGH".to_string(),
                verification_uri: "https://platform.example.test/device".to_string(),
                expires_in_seconds: 60,
            };
            let Err(error) =
                poll_rayline_device_session(&hosted(None, &api.base_url), &device).await
            else {
                panic!("a pending poll without a valid intervalSeconds signed in");
            };
            assert_eq!(login_failure(error), expected);
            assert_eq!(api.paths(), ["POST /v1/auth/cli/device/poll"]);
        }
    }

    #[tokio::test]
    async fn login_replaces_an_invalid_credentials_file() {
        let api = FakeApi::start(&device_login_routes(200, json!({ "apiKey": "rlk-new" })));
        let home = temp_home("login-replaces");
        write_credentials_text(&home, "not json");

        let message = login(&hosted(None, &api.base_url), &home, LoginFlow::Device)
            .await
            .unwrap();
        assert!(
            message.starts_with("Logged in as new@example.com (prod)\n"),
            "{message}"
        );
        assert!(message.contains("Replaced invalid "), "{message}");
        assert_eq!(stored(&home, None).unwrap().router_key.api_key, "rlk-new");
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_failed_mint_writes_nothing() {
        let api = FakeApi::start(&device_login_routes(500, json!({ "error": "unavailable" })));
        let home = temp_home("login-mint-fails");
        let error = login(&hosted(None, &api.base_url), &home, LoginFlow::Device)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::MintFailed(_)), "{error:?}");
        assert_eq!(credentials_text(&home), None);

        save_account(&home, Some("dev"), account(LATER));
        let before = credentials_text(&home);
        let error = login(&hosted(None, &api.base_url), &home, LoginFlow::Device)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::MintFailed(_)), "{error:?}");
        assert_eq!(credentials_text(&home), before);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn logout_removes_its_slot_and_revokes_the_session() {
        let api = FakeApi::start(&[("/v1/auth/cli/revoke", 200, json!({}))]);
        let home = temp_home("logout");
        save_account(&home, None, account(LATER));
        save_account(&home, Some("dev"), account(LATER));
        let prod_before = stored(&home, None).unwrap();

        let message = logout_from_home(&hosted(Some("dev"), &api.base_url), &home)
            .await
            .unwrap();
        assert_eq!(message, "Logged out (dev)\n");
        let requests = api.received();
        assert_eq!(api.paths(), ["POST /v1/auth/cli/revoke"]);
        assert_eq!(
            requests[0].authorization,
            Some(format!("Bearer {REFRESH_TOKEN}"))
        );
        assert_eq!(requests[0].body, json!({ "reason": "cli_logout" }));
        assert_eq!(stored(&home, Some("dev")), None);
        assert_eq!(stored(&home, None).unwrap(), prod_before);

        let message = logout_from_home(&hosted(None, &api.base_url), &home)
            .await
            .unwrap();
        assert_eq!(message, "Logged out (prod)\n");
        let text = credentials_text(&home).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json!({}));
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_failed_revoke_logs_out_locally_and_is_an_error() {
        let api = FakeApi::start(&[("/v1/auth/cli/revoke", 500, json!({}))]);
        let home = temp_home("logout-revoke-fails");
        save_account(&home, None, account(LATER));
        let error = logout_from_home(&hosted(None, &api.base_url), &home)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Logged out of prod on this machine, but revoking the session failed: \
             session revoke failed: HTTP 500: {}"
        );
        assert_eq!(stored(&home, None), None);
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn logout_when_signed_out_changes_nothing() {
        let home = temp_home("logout-signed-out");
        let message = logout_from_home(&hosted(Some("dev"), UNREACHABLE), &home)
            .await
            .unwrap();
        assert_eq!(message, "Not logged in to dev\n");
        assert_eq!(credentials_text(&home), None);

        write_credentials_text(&home, "not json");
        let error = logout_from_home(&hosted(Some("dev"), UNREACHABLE), &home)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Credentials(_)), "{error:?}");
        assert_eq!(credentials_text(&home).as_deref(), Some("not json"));
        let _ = fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn a_run_uses_the_stored_router_key_and_never_signs_in_without_a_terminal() {
        let home = temp_home("router-key");
        let prod = hosted(None, UNREACHABLE);
        let error = router_key_with(&prod, &home, false).await.unwrap_err();
        assert!(
            matches!(&error, AuthError::SignedOut(env) if env == "prod"),
            "{error:?}"
        );
        assert_eq!(
            error.to_string(),
            format!("Not logged in to prod. Run: {} auth login", crate::CLI_BIN)
        );
        assert_eq!(credentials_text(&home), None);

        save_account(&home, None, account(NOW - 1));
        assert_eq!(router_key_with(&prod, &home, false).await.unwrap(), API_KEY);
        assert_eq!(
            stored_router_key(&prod, &home).unwrap().as_deref(),
            Some(API_KEY)
        );
        let _ = fs::remove_dir_all(&home);
    }

    #[test]
    fn paste_wins_over_no_browser() {
        let request = |no_browser, paste| AuthLoginRequest {
            hosted: hosted(None, UNREACHABLE),
            no_browser,
            paste,
        };
        assert_eq!(request(false, false).flow(), LoginFlow::Browser);
        assert_eq!(request(true, false).flow(), LoginFlow::Device);
        assert_eq!(request(false, true).flow(), LoginFlow::Paste);
        assert_eq!(request(true, true).flow(), LoginFlow::Paste);
    }

    #[test]
    fn a_pasted_callback_url_needs_this_logins_state_and_a_code() {
        assert_eq!(
            parse_paste_callback_url("http://127.0.0.1/?code=one-time&state=s1", "s1").unwrap(),
            "one-time"
        );
        for pasted in [
            "?code=one-time&state=s1",
            "not a url",
            "http://127.0.0.1/?code=one-time&state=other",
            "http://127.0.0.1/?state=s1",
        ] {
            assert!(
                matches!(
                    parse_paste_callback_url(pasted, "s1"),
                    Err(AuthError::InvalidPaste(_))
                ),
                "{pasted}"
            );
        }
    }

    #[test]
    fn the_callback_query_waits_for_a_code_with_this_logins_state() {
        assert_eq!(parse_callback_query("/", "s1"), Ok(None));
        assert_eq!(parse_callback_query("/favicon.ico", "s1"), Ok(None));
        assert_eq!(
            parse_callback_query("/?code=one-time&state=s1", "s1"),
            Ok(Some("one-time".to_owned()))
        );
        assert_eq!(
            parse_callback_query("/?code=one-time&state=other", "s1"),
            Err(CallbackQueryError::StateMismatch)
        );
    }
}
