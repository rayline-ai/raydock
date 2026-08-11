use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};

const UPDATE_BASE_URL_ENV: &str = "RAYLINE_UPDATE_BASE_URL";
const UPDATE_INSTALL_PATH_ENV: &str = "RAYLINE_UPDATE_INSTALL_PATH";
const MACOS_RLD_SIGNING_IDENTIFIER: &str = "ai.rayline.rld";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateRequest {
    pub channel: Option<String>,
    pub pinned_version: Option<String>,
    pub force: bool,
    pub check_only: bool,
    pub dry_run: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateResult {
    pub message: String,
    pub exit_code: u8,
    pub stderr: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdateError {
    Checksum(String),
    InvalidVersion(String),
    Install(String),
    Network(String),
    Signature(String),
    UnsupportedPlatform(String),
}

struct UpdateArtifacts {
    launcher: PathBuf,
    daemon: PathBuf,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Checksum(message)
            | Self::Install(message)
            | Self::Network(message)
            | Self::Signature(message)
            | Self::UnsupportedPlatform(message) => formatter.write_str(message),
            Self::InvalidVersion(version) => write!(formatter, "invalid --version '{version}'"),
        }
    }
}

impl std::error::Error for UpdateError {}

pub async fn run(request: &UpdateRequest) -> Result<UpdateResult, UpdateError> {
    let home = dirs::home_dir();
    let channel = request
        .channel
        .clone()
        .unwrap_or_else(|| resolve_channel(home.as_deref()));

    if channel == "local" {
        return Ok(UpdateResult {
            message: concat!(
                "Local dev install detected \u{2014} refresh with ",
                "`make install` or `make install-dev`.\n"
            )
            .to_owned(),
            exit_code: 2,
            stderr: true,
        });
    }

    let install_path = install_path().map_err(|error| {
        UpdateError::Install(format!(
            "failed to resolve {} install path: {error}",
            crate::CLI_BIN
        ))
    })?;
    if is_app_bundle_managed(&install_path) {
        return Ok(UpdateResult {
            message: format!(
                "{} is managed by Rayline.app. Update the app to update the CLI:\n  \
                 re-download from https://get.rayline.ai\n",
                crate::CLI_BIN
            ),
            exit_code: 0,
            stderr: false,
        });
    }
    if is_homebrew_managed(&install_path) {
        return Ok(UpdateResult {
            message: format!(
                "Installed via Homebrew. Run `brew upgrade {}` to update.\n",
                crate::CLI_BIN
            ),
            exit_code: 0,
            stderr: false,
        });
    }

    let platform_tag = detect_platform_tag()?;
    let current = Version::parse(crate::RAYLINE_VERSION)
        .map_err(|_| UpdateError::InvalidVersion(crate::RAYLINE_VERSION.to_owned()))?;
    let target = match request.pinned_version.as_deref() {
        Some(version) => {
            Version::parse(version).map_err(|_| UpdateError::InvalidVersion(version.to_owned()))?
        }
        None => fetch_latest_version(&channel).await?,
    };

    let decision = evaluate_check(
        &current,
        &target,
        request.pinned_version.is_some() || request.channel.is_some(),
        request.force,
    );
    if decision.exit_code == 0 || request.check_only {
        return Ok(decision);
    }

    let temp_dir = temp_update_dir()?;
    let artifacts = download_and_verify(&target.normalized, platform_tag, &temp_dir).await;
    let result = match artifacts {
        Ok(artifacts) => {
            if request.dry_run {
                Ok(UpdateResult {
                    message: format!(
                        "Would install {} and {}.\n",
                        artifacts.launcher.display(),
                        artifacts.daemon.display()
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            } else {
                let daemon_install_path = install_path
                    .parent()
                    .ok_or_else(|| {
                        UpdateError::Install(format!(
                            "install path has no parent directory: {}",
                            install_path.display()
                        ))
                    })?
                    // `EXE_SUFFIX` is "" on Unix and ".exe" on Windows, so the
                    // daemon installs as `rld` on Unix and `rld.exe` on Windows
                    // (matching the `.exe` lookup in `router::find_on_path`).
                    .join(format!(
                        "{}{}",
                        crate::DAEMON_BIN,
                        std::env::consts::EXE_SUFFIX
                    ));
                replace_binary(&artifacts.launcher, &install_path)?;
                replace_binary(&artifacts.daemon, &daemon_install_path)?;
                cleanup_legacy_uv_install(crate::UV_TOOL_NAME, &install_path);
                Ok(UpdateResult {
                    message: format!(
                        "Updated {} {} \u{2192} {}.\n",
                        crate::DISPLAY_NAME,
                        current.public,
                        target.public
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            }
        }
        Err(error) => Err(error),
    };
    let _ = fs::remove_dir_all(&temp_dir);
    result
}

pub fn evaluate_check(
    current: &Version,
    target: &Version,
    explicit_intent: bool,
    force: bool,
) -> UpdateResult {
    let update_available = if explicit_intent {
        target.normalized != current.normalized || force
    } else {
        target.public_cmp(current) == Ordering::Greater || force
    };

    if update_available {
        return UpdateResult {
            message: format!(
                "Update available: {} \u{2192} {}. Run: {} update\n",
                current.public,
                target.public,
                crate::CLI_BIN
            ),
            exit_code: 1,
            stderr: false,
        };
    }

    UpdateResult {
        message: format!("Already on latest ({}).\n", current.public),
        exit_code: 0,
        stderr: false,
    }
}

pub fn resolve_channel(home: Option<&Path>) -> String {
    let env_channel = std::env::var("RAYLINE_UPDATE_CHANNEL").ok();
    resolve_channel_from_sources(home, env_channel.as_deref(), crate::RAYLINE_CHANNEL)
}

fn resolve_channel_from_sources(
    home: Option<&Path>,
    env_channel: Option<&str>,
    embedded_channel: &str,
) -> String {
    if let Some(channel) = env_channel {
        if is_valid_channel(channel) {
            return channel.to_owned();
        }
    }

    if let Some(home) = home {
        if let Some(channel) = config_channel(home) {
            return channel;
        }
    }

    if is_valid_channel(embedded_channel) {
        return embedded_channel.to_owned();
    }

    "local".to_owned()
}

fn config_channel(home: &Path) -> Option<String> {
    let path = home
        .join(".config")
        .join(crate::CONFIG_DIR)
        .join("cli.toml");
    let contents = std::fs::read_to_string(path).ok()?;
    let mut in_update_section = false;

    for raw_line in contents.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_update_section = line == "[update]";
            continue;
        }
        if !in_update_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "channel" {
            continue;
        }
        let channel = value.trim().trim_matches('"').trim_matches('\'');
        if is_valid_channel(channel) {
            return Some(channel.to_owned());
        }
    }

    None
}

fn is_valid_channel(channel: &str) -> bool {
    // Release ladder for both brands: main (rolling, rawest) -> dev (release
    // candidate) -> prod (public X.Y.Z). `local` is the dev-build sentinel.
    matches!(channel, "prod" | "dev" | "main" | "local")
}

async fn fetch_latest_version(channel: &str) -> Result<Version, UpdateError> {
    let url = latest_url_for(channel).ok_or_else(|| {
        UpdateError::Network(format!("channel '{channel}' has no latest pointer"))
    })?;
    let pointer = fetch_pointer_bytes(&url).await?;
    // Fail closed: the version pointer must carry a verifiable signature before
    // we trust the version it names. This prevents an attacker from FORGING a
    // pointer to an arbitrary or never-released version. It does NOT, on its
    // own, defeat replay/freeze: an attacker who can serve content may replay an
    // older, validly-signed pointer. The `target > current` check in
    // `evaluate_check` blocks rolling BELOW the installed version, but full
    // anti-rollback/freshness (a signed timestamp+expiry or highest-seen-version
    // floor) is tracked as a follow-up — see docs/release.md.
    let signature = fetch_pointer_bytes(&format!("{url}.minisig")).await?;
    parse_verified_latest(&pointer, &signature, crate::MINISIGN_PUBLIC_KEYS)
}

async fn fetch_pointer_bytes(url: &str) -> Result<Vec<u8>, UpdateError> {
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .map_err(|error| UpdateError::Network(format!("failed to fetch {url}: {error}")))?
        .get(url)
        .send()
        .await
        .map_err(|error| UpdateError::Network(format!("failed to fetch {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "{url} returned HTTP {}",
            response.status().as_u16()
        )));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| UpdateError::Network(format!("failed to fetch {url}: {error}")))
}

/// Verify the latest-pointer bytes against a pinned key, then parse the version.
///
/// Fails closed: an absent/invalid signature, non-UTF-8 bytes, or an
/// unparseable version is an error. The signature is over the pointer's raw
/// bytes, so a tampered version string no longer matches.
fn parse_verified_latest(
    pointer: &[u8],
    signature: &[u8],
    pubkeys: &[&str],
) -> Result<Version, UpdateError> {
    verify_signature(pointer, signature, pubkeys)?;
    let text = std::str::from_utf8(pointer)
        .map_err(|_| UpdateError::Network("latest pointer is not valid UTF-8".to_owned()))?
        .trim();
    Version::parse(text).map_err(|_| {
        UpdateError::Network(format!("could not parse version from pointer: {text:?}"))
    })
}

fn latest_url_for(channel: &str) -> Option<String> {
    let latest_key = match channel {
        "prod" => "cli/latest.txt",
        "dev" => "cli/latest-dev.txt",
        "main" => "cli/latest-main.txt",
        _ => return None,
    };
    Some(format!("{}/{latest_key}", base_url()))
}

fn artifact_url_for(version: &str, platform_tag: &str) -> String {
    format!(
        "{}/cli/v{version}/{}-{platform_tag}",
        base_url(),
        crate::CLI_BIN
    )
}

fn daemon_artifact_url_for(version: &str, platform_tag: &str) -> String {
    format!(
        "{}/cli/v{version}/{}-{platform_tag}",
        base_url(),
        crate::DAEMON_BIN
    )
}

fn checksums_url_for(version: &str) -> String {
    format!("{}/cli/v{version}/SHA256SUMS", base_url())
}

fn checksums_sig_url_for(version: &str) -> String {
    format!("{}/cli/v{version}/SHA256SUMS.minisig", base_url())
}

/// Verify that `sums` is signed by at least one of the pinned `pubkeys`.
///
/// Fails closed: returns `Err` if `sig_bytes` is empty, malformed, or no
/// pinned key validates the signature. The caller must call this BEFORE
/// trusting any checksum from `sums`.
pub fn verify_signature(
    sums: &[u8],
    sig_bytes: &[u8],
    pubkeys: &[&str],
) -> Result<(), UpdateError> {
    if sig_bytes.is_empty() {
        return Err(UpdateError::Signature(
            "SHA256SUMS.minisig is empty — refusing to install unsigned update".to_owned(),
        ));
    }

    let sig =
        Signature::decode(std::str::from_utf8(sig_bytes).map_err(|_| {
            UpdateError::Signature("SHA256SUMS.minisig is not valid UTF-8".to_owned())
        })?)
        .map_err(|error| {
            UpdateError::Signature(format!("malformed SHA256SUMS.minisig: {error}"))
        })?;

    for pubkey_str in pubkeys {
        let pk = PublicKey::from_base64(pubkey_str).map_err(|error| {
            UpdateError::Signature(format!("invalid pinned public key: {error}"))
        })?;
        if pk.verify(sums, &sig, true).is_ok() {
            return Ok(());
        }
    }

    Err(UpdateError::Signature(
        "SHA256SUMS signature could not be verified against any pinned public key — \
         refusing to install"
            .to_owned(),
    ))
}

pub(crate) fn base_url() -> String {
    std::env::var(UPDATE_BASE_URL_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| crate::UPDATE_BASE_URL.to_owned())
        .trim_end_matches('/')
        .to_owned()
}

pub(crate) fn detect_platform_tag() -> Result<&'static str, UpdateError> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Ok("macosx_11_0_arm64"),
        ("macos", "x86_64") => Ok("macosx_10_12_x86_64"),
        ("linux", "x86_64") => Ok("linux_x86_64"),
        ("linux", "aarch64") => Ok("linux_aarch64"),
        ("windows", "x86_64") => Ok("windows_x86_64"),
        (os, arch) => Err(UpdateError::UnsupportedPlatform(format!(
            "unsupported platform: {os}-{arch}. Supported: linux_aarch64, linux_x86_64, macosx_10_12_x86_64, macosx_11_0_arm64, windows_x86_64",
        ))),
    }
}

async fn download_and_verify(
    version: &str,
    platform_tag: &str,
    dest_dir: &Path,
) -> Result<UpdateArtifacts, UpdateError> {
    fs::create_dir_all(dest_dir).map_err(UpdateError::from)?;
    let launcher_name = format!("{}-{platform_tag}", crate::CLI_BIN);
    let daemon_name = format!("{}-{platform_tag}", crate::DAEMON_BIN);
    let launcher_path = dest_dir.join(&launcher_name);
    let daemon_path = dest_dir.join(&daemon_name);
    let sums_path = dest_dir.join("SHA256SUMS");
    let sig_path = dest_dir.join("SHA256SUMS.minisig");

    // Download checksums and its signature first so we can verify before
    // trusting any artifact bytes.
    download_to(&checksums_url_for(version), &sums_path).await?;
    download_to(&checksums_sig_url_for(version), &sig_path).await?;

    // Verify signature BEFORE reading or trusting the checksums file.
    // Fail closed: missing or invalid signature aborts the update.
    let sums_bytes = fs::read(&sums_path).map_err(UpdateError::from)?;
    let sig_bytes = fs::read(&sig_path).map_err(UpdateError::from)?;
    verify_signature(&sums_bytes, &sig_bytes, crate::MINISIGN_PUBLIC_KEYS)?;

    let sums = String::from_utf8(sums_bytes)
        .map_err(|_| UpdateError::Checksum("SHA256SUMS contains invalid UTF-8".to_owned()))?;

    download_to(&artifact_url_for(version, platform_tag), &launcher_path).await?;
    download_to(
        &daemon_artifact_url_for(version, platform_tag),
        &daemon_path,
    )
    .await?;

    verify_checksum(&sums, &launcher_name, &launcher_path)?;
    verify_checksum(&sums, &daemon_name, &daemon_path)?;
    verify_macos_rld_signature(&daemon_path)?;

    Ok(UpdateArtifacts {
        launcher: launcher_path,
        daemon: daemon_path,
    })
}

pub(crate) async fn download_to(url: &str, path: &Path) -> Result<(), UpdateError> {
    let part = path.with_extension("part");
    if let Some(source) = url.strip_prefix("file://") {
        fs::copy(source, &part).map_err(UpdateError::from)?;
        fs::rename(&part, path).map_err(UpdateError::from)?;
        return Ok(());
    }

    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?
        .get(url)
        .send()
        .await
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?;
    if !response.status().is_success() {
        return Err(UpdateError::Network(format!(
            "GET {url} returned HTTP {}",
            response.status().as_u16()
        )));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|error| UpdateError::Network(format!("download failed for {url}: {error}")))?;
    fs::write(&part, bytes).map_err(UpdateError::from)?;
    fs::rename(&part, path).map_err(UpdateError::from)?;
    Ok(())
}

fn expected_sha256(sums: &str, artifact_name: &str) -> Result<String, UpdateError> {
    for line in sums.lines() {
        let stripped = line.trim();
        if stripped.is_empty() || stripped.starts_with('#') {
            continue;
        }
        let mut parts = stripped.split_whitespace();
        let Some(digest) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        if name.trim().trim_start_matches('*') == artifact_name {
            return Ok(digest.to_owned());
        }
    }
    Err(UpdateError::Checksum(format!(
        "no sha256 entry for {artifact_name} in SHA256SUMS"
    )))
}

pub(crate) fn verify_checksum(
    sums: &str,
    artifact_name: &str,
    path: &Path,
) -> Result<(), UpdateError> {
    let expected = expected_sha256(sums, artifact_name)?;
    let actual = sha256_file(path)?;
    if expected != actual {
        let _ = fs::remove_file(path);
        return Err(UpdateError::Checksum(format!(
            "sha256 mismatch for {artifact_name}: expected {expected}, got {actual}"
        )));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, UpdateError> {
    let bytes = fs::read(path).map_err(UpdateError::from)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(target_os = "macos")]
fn verify_macos_rld_signature(path: &Path) -> Result<(), UpdateError> {
    let verification = Command::new("/usr/bin/codesign")
        .args(["--verify", "--strict", "--verbose=2"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            UpdateError::Signature(format!(
                "failed to verify the macOS rld code signature: {error}"
            ))
        })?;
    if !verification.status.success() {
        return Err(UpdateError::Signature(
            "rld has an invalid macOS code signature — refusing to install".to_owned(),
        ));
    }

    let details = codesign_metadata(path, &["-d", "--verbose=4"])?;
    let requirement = codesign_metadata(path, &["-d", "-r-"])?;
    validate_macos_rld_signature_metadata(&details, &requirement)
}

#[cfg(not(target_os = "macos"))]
fn verify_macos_rld_signature(_path: &Path) -> Result<(), UpdateError> {
    Ok(())
}

#[cfg(target_os = "macos")]
fn codesign_metadata(path: &Path, args: &[&str]) -> Result<String, UpdateError> {
    let output = Command::new("/usr/bin/codesign")
        .args(args)
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            UpdateError::Signature(format!("failed to inspect the rld code signature: {error}"))
        })?;
    if !output.status.success() {
        return Err(UpdateError::Signature(
            "could not inspect the macOS rld code signature".to_owned(),
        ));
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    Ok(text)
}

fn validate_macos_rld_signature_metadata(
    details: &str,
    requirement: &str,
) -> Result<(), UpdateError> {
    let identifier = signature_field(details, "Identifier").unwrap_or_default();
    if identifier != MACOS_RLD_SIGNING_IDENTIFIER {
        return Err(UpdateError::Signature(format!(
            "rld has macOS signing identifier '{identifier}', expected '{MACOS_RLD_SIGNING_IDENTIFIER}'"
        )));
    }

    let team = signature_field(details, "TeamIdentifier").unwrap_or_default();
    if team.is_empty() || team == "not set" {
        return Err(UpdateError::Signature(
            "rld is ad-hoc signed, so Keychain approval would not survive an update".to_owned(),
        ));
    }
    if requirement.contains("# designated => cdhash ") {
        return Err(UpdateError::Signature(
            "rld's designated requirement is pinned to one build hash".to_owned(),
        ));
    }
    if !details
        .lines()
        .any(|line| line.starts_with("Authority=Developer ID Application:"))
    {
        return Err(UpdateError::Signature(
            "rld is not signed with a Developer ID Application certificate".to_owned(),
        ));
    }
    Ok(())
}

fn signature_field<'a>(details: &'a str, field: &str) -> Option<&'a str> {
    details
        .lines()
        .find_map(|line| line.strip_prefix(field)?.strip_prefix('='))
}

fn install_path() -> io::Result<PathBuf> {
    std::env::var_os(UPDATE_INSTALL_PATH_ENV)
        .map(PathBuf::from)
        .map_or_else(std::env::current_exe, Ok)
        .map(resolve_install_path)
}

/// `current_exe()` may return the invoking symlink rather than its target on
/// some platforms (macOS). A migrated install runs the binary through the
/// legacy `rl -> rayline` alias the installer leaves behind, and self-update
/// must replace the real binary: renaming the download over the alias would
/// orphan `rayline` at the old version, reintroducing exactly the divergence
/// the installer migration removes. Nonexistent paths (e.g. an env override
/// pointing at a not-yet-created file) pass through unchanged.
fn resolve_install_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

fn is_homebrew_managed(install_path: &Path) -> bool {
    let Some(prefix) = homebrew_prefix() else {
        return false;
    };
    path_is_under_prefix(install_path, &prefix)
}

/// True when the running `rayline` lives inside a `.app` bundle (directly, or via a
/// `/usr/local/bin` symlink created by the in-app "Install command line tools"
/// action). The app owns updates in that case; self-replacing would break the
/// notarization seal. Canonicalize first so symlink paths resolve to the real
/// bundle path before matching.
fn is_app_bundle_managed(install_path: &Path) -> bool {
    let resolved =
        std::fs::canonicalize(install_path).unwrap_or_else(|_| install_path.to_path_buf());
    resolved.to_string_lossy().contains(".app/Contents/")
}

fn path_is_under_prefix(install_path: &Path, prefix: &Path) -> bool {
    let install_path =
        fs::canonicalize(install_path).unwrap_or_else(|_| install_path.to_path_buf());
    let prefix = fs::canonicalize(prefix).unwrap_or_else(|_| prefix.to_path_buf());
    install_path.starts_with(prefix)
}

fn homebrew_prefix() -> Option<PathBuf> {
    let brew = command_on_path("brew")?;
    let output = Command::new(brew)
        .arg("--prefix")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let prefix = String::from_utf8(output.stdout).ok()?;
    let prefix = prefix.trim();
    if prefix.is_empty() {
        None
    } else {
        Some(PathBuf::from(prefix))
    }
}

pub(crate) fn replace_binary(downloaded: &Path, install_path: &Path) -> Result<(), UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent directory: {}",
            install_path.display()
        ))
    })?;
    fs::create_dir_all(parent).map_err(|error| {
        UpdateError::Install(format!(
            "failed to create install directory {}: {error}",
            parent.display()
        ))
    })?;

    let staged = staged_install_path(install_path)?;
    fs::copy(downloaded, &staged).map_err(|error| {
        UpdateError::Install(format!(
            "failed to stage update at {}: {error}",
            staged.display()
        ))
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&staged, permissions).map_err(|error| {
            UpdateError::Install(format!(
                "failed to mark update executable at {}: {error}",
                staged.display()
            ))
        })?;
    }

    let result = move_into_place(&staged, install_path);
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result
}

/// Atomically move the staged binary onto `install_path`.
///
/// On Unix `rename(2)` replaces the target even while it is running. Windows
/// locks a running `.exe` and rejects renaming over it, but it *does* allow
/// renaming the running file out of the way — so we move the current binary to
/// a sidecar path first, then move the update into place, restoring the
/// original if the second move fails.
#[cfg(not(windows))]
fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    fs::rename(staged, install_path).map_err(|error| {
        UpdateError::Install(format!(
            "failed to replace {}: {error}",
            install_path.display()
        ))
    })
}

#[cfg(windows)]
fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    if !install_path.exists() {
        return fs::rename(staged, install_path).map_err(|error| {
            UpdateError::Install(format!(
                "failed to install {}: {error}",
                install_path.display()
            ))
        });
    }

    let backup = staged_install_path(install_path)?;
    fs::rename(install_path, &backup).map_err(|error| {
        UpdateError::Install(format!(
            "failed to move existing binary {} aside: {error}",
            install_path.display()
        ))
    })?;

    match fs::rename(staged, install_path) {
        Ok(()) => {
            // Best-effort: the old binary stays locked while it is the running
            // process, so removal may fail here and succeed on a later update.
            let _ = fs::remove_file(&backup);
            Ok(())
        }
        Err(error) => {
            // Restore the original so a failed update never leaves a hole.
            let _ = fs::rename(&backup, install_path);
            Err(UpdateError::Install(format!(
                "failed to replace {}: {error}",
                install_path.display()
            )))
        }
    }
}

fn staged_install_path(install_path: &Path) -> Result<PathBuf, UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent directory: {}",
            install_path.display()
        ))
    })?;
    let file_name = install_path.file_name().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no file name: {}",
            install_path.display()
        ))
    })?;
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let mut staged_name = OsString::from(".");
    staged_name.push(file_name);
    staged_name.push(format!(".tmp-{}-{unique}", std::process::id()));
    Ok(parent.join(staged_name))
}

fn cleanup_legacy_uv_install(tool_name: &str, install_path: &Path) {
    if std::env::var_os(UPDATE_INSTALL_PATH_ENV).is_some() {
        return;
    }
    let Some(uv) = command_on_path("uv") else {
        return;
    };
    if is_uv_tool_managed_install(&uv, tool_name, install_path) {
        return;
    }
    let _ = run_legacy_uv_uninstall(&uv, tool_name);
}

fn is_uv_tool_managed_install(uv: &Path, tool_name: &str, install_path: &Path) -> bool {
    let output = Command::new(uv)
        .args(["tool", "dir"])
        .stdin(Stdio::null())
        .output()
        .ok();
    let Some(output) = output.filter(|output| output.status.success()) else {
        return false;
    };
    let Some(tools_dir) = String::from_utf8(output.stdout)
        .ok()
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty())
    else {
        return false;
    };
    is_uv_tool_managed_path(tool_name, install_path, Path::new(&tools_dir))
}

fn is_uv_tool_managed_path(tool_name: &str, install_path: &Path, tools_dir: &Path) -> bool {
    path_is_under_prefix(install_path, &tools_dir.join(tool_name))
}

fn run_legacy_uv_uninstall(uv: &Path, tool_name: &str) -> io::Result<bool> {
    let status = Command::new(uv)
        .args(["tool", "uninstall", tool_name])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?;
    Ok(status.success())
}

fn command_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn temp_update_dir() -> Result<PathBuf, UpdateError> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| UpdateError::Install(format!("system clock error: {error}")))?
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "{}-update-{}-{unique}",
        crate::CLI_BIN,
        std::process::id()
    ));
    fs::create_dir_all(&path).map_err(UpdateError::from)?;
    Ok(path)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Version {
    normalized: String,
    public: String,
    base_parts: Vec<u64>,
    suffix: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VersionParseError;

#[derive(Debug, Eq, PartialEq)]
enum VersionToken {
    Number(u64),
    Text(String),
}

impl Version {
    pub fn parse(raw: &str) -> Result<Self, VersionParseError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(VersionParseError);
        }
        let (public, local) = trimmed.split_once('+').unwrap_or((trimmed, ""));
        if public.is_empty() || trimmed.matches('+').count() > 1 {
            return Err(VersionParseError);
        }
        if trimmed.contains('+') && local.is_empty() {
            return Err(VersionParseError);
        }
        if !local.is_empty() {
            validate_version_label(local)?;
        }

        let (base_parts, suffix) = parse_public_version(public)?;

        Ok(Self {
            normalized: trimmed.to_ascii_lowercase(),
            public: public.to_owned(),
            base_parts,
            suffix: suffix.map(str::to_ascii_lowercase),
        })
    }

    fn base_cmp(&self, other: &Self) -> Ordering {
        let length = self.base_parts.len().max(other.base_parts.len());
        for index in 0..length {
            let left = self.base_parts.get(index).copied().unwrap_or(0);
            let right = other.base_parts.get(index).copied().unwrap_or(0);
            match left.cmp(&right) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    }

    fn public_cmp(&self, other: &Self) -> Ordering {
        match self.base_cmp(other) {
            Ordering::Equal => compare_suffixes(self.suffix.as_deref(), other.suffix.as_deref()),
            ordering => ordering,
        }
    }
}

fn parse_public_version(public: &str) -> Result<(Vec<u64>, Option<&str>), VersionParseError> {
    let mut base_parts = Vec::new();
    let mut index = 0;
    let bytes = public.as_bytes();

    loop {
        let start = index;
        while index < bytes.len() && bytes[index].is_ascii_digit() {
            index += 1;
        }
        if start == index {
            return Err(VersionParseError);
        }
        base_parts.push(
            public[start..index]
                .parse::<u64>()
                .map_err(|_| VersionParseError)?,
        );

        if index == bytes.len() {
            return Ok((base_parts, None));
        }
        if bytes[index] == b'.' && index + 1 < bytes.len() && bytes[index + 1].is_ascii_digit() {
            index += 1;
            continue;
        }

        let suffix = &public[index..];
        validate_version_label(suffix)?;
        return Ok((base_parts, Some(suffix)));
    }
}

fn validate_version_label(label: &str) -> Result<(), VersionParseError> {
    if label.is_empty()
        || !label.chars().any(|ch| ch.is_ascii_alphanumeric())
        || !label
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
    {
        return Err(VersionParseError);
    }
    Ok(())
}

fn compare_suffixes(left: Option<&str>, right: Option<&str>) -> Ordering {
    match (left, right) {
        (None, None) => Ordering::Equal,
        (None, Some(right)) if suffix_is_post_release(right) => Ordering::Less,
        (Some(left), None) if suffix_is_post_release(left) => Ordering::Greater,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(left), Some(right)) => compare_suffix_tokens(left, right),
    }
}

fn suffix_is_post_release(suffix: &str) -> bool {
    suffix_tokens(suffix).first().is_some_and(
        |token| matches!(token, VersionToken::Text(text) if text_rank(text) == POST_RELEASE_RANK),
    )
}

fn compare_suffix_tokens(left: &str, right: &str) -> Ordering {
    let left_tokens = suffix_tokens(left);
    let right_tokens = suffix_tokens(right);
    let length = left_tokens.len().max(right_tokens.len());
    for index in 0..length {
        match (left_tokens.get(index), right_tokens.get(index)) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => match compare_version_tokens(left, right) {
                Ordering::Equal => {}
                ordering => return ordering,
            },
        }
    }
    Ordering::Equal
}

fn compare_version_tokens(left: &VersionToken, right: &VersionToken) -> Ordering {
    match (left, right) {
        (VersionToken::Number(left), VersionToken::Number(right)) => left.cmp(right),
        (VersionToken::Text(left), VersionToken::Text(right)) => {
            match text_rank(left).cmp(&text_rank(right)) {
                Ordering::Equal => left.cmp(right),
                ordering => ordering,
            }
        }
        (VersionToken::Number(_), VersionToken::Text(_)) => Ordering::Less,
        (VersionToken::Text(_), VersionToken::Number(_)) => Ordering::Greater,
    }
}

fn suffix_tokens(suffix: &str) -> Vec<VersionToken> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut current_is_digit = None;
    for ch in suffix
        .trim_start_matches(['.', '-', '_'])
        .chars()
        .flat_map(char::to_lowercase)
    {
        if matches!(ch, '.' | '-' | '_') {
            push_suffix_token(&mut tokens, &mut current, &mut current_is_digit);
            continue;
        }
        let is_digit = ch.is_ascii_digit();
        if current_is_digit.is_some_and(|current| current != is_digit) {
            push_suffix_token(&mut tokens, &mut current, &mut current_is_digit);
        }
        current_is_digit = Some(is_digit);
        current.push(ch);
    }
    push_suffix_token(&mut tokens, &mut current, &mut current_is_digit);
    tokens
}

fn push_suffix_token(
    tokens: &mut Vec<VersionToken>,
    current: &mut String,
    current_is_digit: &mut Option<bool>,
) {
    if current.is_empty() {
        *current_is_digit = None;
        return;
    }
    if current_is_digit.unwrap_or(false) {
        let number = current.parse::<u64>().unwrap_or(u64::MAX);
        tokens.push(VersionToken::Number(number));
    } else {
        tokens.push(VersionToken::Text(std::mem::take(current)));
    }
    current.clear();
    *current_is_digit = None;
}

/// Rank of a post-release label (`1.0.0-post1`), the only suffix family that
/// sorts *above* a bare release. Kept as a named constant so `text_rank` and
/// `suffix_is_post_release` cannot drift apart.
const POST_RELEASE_RANK: u8 = 6;

fn text_rank(text: &str) -> u8 {
    // Ordered low -> high. `main` sits below `dev` because the release ladder is
    // main (rawest, rolling) -> dev (release candidate) -> prod: a main build
    // must never be reported as newer than the dev build it is promoted into.
    match text {
        "main" => 0,
        "dev" => 1,
        "a" | "alpha" => 2,
        "b" | "beta" => 3,
        "c" | "pre" | "preview" | "rc" => 4,
        "post" | "r" | "rev" => POST_RELEASE_RANK,
        _ => 5,
    }
}

impl From<io::Error> for UpdateError {
    fn from(error: io::Error) -> Self {
        Self::Network(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uv_tool_managed_path_matches_tool_environment() {
        let tools_dir = Path::new("/Users/test/.local/share/uv/tools");
        let install_path = tools_dir.join("rayline-cli").join("bin").join("rayline");

        assert!(is_uv_tool_managed_path(
            "rayline-cli",
            &install_path,
            tools_dir
        ));
    }

    #[test]
    fn uv_tool_managed_path_rejects_other_installs() {
        let tools_dir = Path::new("/Users/test/.local/share/uv/tools");
        let install_path = Path::new("/usr/local/bin/rayline");

        assert!(!is_uv_tool_managed_path(
            "rayline-cli",
            install_path,
            tools_dir
        ));
    }

    // ── Signature verification tests ─────────────────────────────────────────

    // Test public key (matches the .minisig in tests/fixtures/SHA256SUMS.minisig)
    // Generated for testing only — NOT the production key.
    const TEST_PUBKEY: &str = "RWRqzAWsbJCJh9W2BSnYcbRiBwshTgouNtwYqkmFX1Qs6kXdxY70sRCP";

    // Untrusted public key (different keypair, not in the pinned set)
    const UNTRUSTED_PUBKEY: &str = "RWQR3fP7y6Yexmbshr2vDmkWJeKelP8y80oyaUgE9AjMymImLFrUu0Dy";

    // Test public key for the latest.txt pointer fixtures (matches
    // tests/fixtures/latest.txt.minisig). Testing only — NOT the production key.
    const LATEST_TEST_PUBKEY: &str = "RWTORxdK/+7+ayF6kfI5TjZjEMLnbo81yDtE94iGpdP7yJHQ0F+HGm0l";

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {name} unreadable: {e}"))
    }

    /// (a) Valid sig + matching pinned key → Ok
    #[test]
    fn verify_signature_valid_key_accepts() {
        let sums = fixture("SHA256SUMS");
        let sig = fixture("SHA256SUMS.minisig");
        assert!(
            verify_signature(&sums, &sig, &[TEST_PUBKEY]).is_ok(),
            "valid signature must verify against pinned key"
        );
    }

    /// (b) Tampered SHA256SUMS → Err (signature won't match different content)
    #[test]
    fn verify_signature_tampered_content_rejects() {
        let sums = fixture("SHA256SUMS.tampered");
        let sig = fixture("SHA256SUMS.minisig");
        let result = verify_signature(&sums, &sig, &[TEST_PUBKEY]);
        assert!(
            result.is_err(),
            "tampered content must not verify against original signature"
        );
        assert!(matches!(result, Err(UpdateError::Signature(_))));
    }

    /// (c) Signature from an unpinned key → Err
    #[test]
    fn verify_signature_unpinned_key_rejects() {
        let sums = fixture("SHA256SUMS");
        let sig = fixture("SHA256SUMS.minisig");
        // Only the UNTRUSTED key is in the pinned set — it didn't sign this file
        let result = verify_signature(&sums, &sig, &[UNTRUSTED_PUBKEY]);
        assert!(
            result.is_err(),
            "signature under unpinned key must be rejected"
        );
        assert!(matches!(result, Err(UpdateError::Signature(_))));
    }

    /// (d) Empty sig_bytes → Err (fail closed — no .minisig means reject)
    #[test]
    fn verify_signature_empty_sig_fails_closed() {
        let sums = fixture("SHA256SUMS");
        let result = verify_signature(&sums, &[], &[TEST_PUBKEY]);
        assert!(result.is_err(), "absent/empty signature must fail closed");
        let Err(UpdateError::Signature(msg)) = result else {
            panic!("expected Signature error");
        };
        assert!(
            msg.contains("empty"),
            "error message should mention empty: {msg}"
        );
    }

    /// (e) Multiple pinned keys — succeeds when at least one matches
    #[test]
    fn verify_signature_accepts_when_one_of_multiple_keys_matches() {
        let sums = fixture("SHA256SUMS");
        let sig = fixture("SHA256SUMS.minisig");
        // List has the untrusted key first, then the real test key
        assert!(
            verify_signature(&sums, &sig, &[UNTRUSTED_PUBKEY, TEST_PUBKEY]).is_ok(),
            "must succeed when any pinned key matches"
        );
    }

    #[test]
    fn stable_developer_id_rld_signature_is_accepted() {
        let details = concat!(
            "Identifier=ai.rayline.rld\n",
            "Authority=Developer ID Application: Rayline Inc (TEAM123456)\n",
            "TeamIdentifier=TEAM123456\n",
        );
        let requirement = concat!(
            "# designated => identifier \"ai.rayline.rld\" and anchor apple generic ",
            "and certificate leaf[subject.OU] = TEAM123456\n",
        );

        assert!(validate_macos_rld_signature_metadata(details, requirement).is_ok());
    }

    #[test]
    fn ad_hoc_rld_signature_is_rejected() {
        let details = concat!(
            "Identifier=rld-08156cb81e3c6ef8\n",
            "Signature=adhoc\n",
            "TeamIdentifier=not set\n",
        );
        let requirement = "# designated => cdhash H\"0123456789abcdef\"\n";

        let error = validate_macos_rld_signature_metadata(details, requirement)
            .expect_err("ad-hoc signature must be rejected");
        assert!(matches!(error, UpdateError::Signature(_)));
    }

    #[test]
    fn wrong_rld_signing_identifier_is_rejected() {
        let details = concat!(
            "Identifier=example.unrelated.rld\n",
            "Authority=Developer ID Application: Rayline Inc (TEAM123456)\n",
            "TeamIdentifier=TEAM123456\n",
        );
        let requirement = "# designated => identifier \"example.unrelated.rld\"\n";

        let error = validate_macos_rld_signature_metadata(details, requirement)
            .expect_err("wrong signing identifier must be rejected");
        assert!(matches!(error, UpdateError::Signature(_)));
    }

    // ── latest.txt version-pointer verification ──────────────────────────────

    /// Signed pointer + matching pinned key → verifies and parses the version.
    #[test]
    fn latest_pointer_valid_signature_parses() {
        let pointer = fixture("latest.txt");
        let sig = fixture("latest.txt.minisig");
        let version = parse_verified_latest(&pointer, &sig, &[LATEST_TEST_PUBKEY])
            .expect("valid signed pointer should verify and parse");
        assert_eq!(version.public, "99.0.0");
    }

    // Tamper / missing-signature / unpinned-key rejection is covered by the
    // verify_signature tests above; parse_verified_latest delegates to that same
    // function, so those paths are not re-tested against the pointer payload.

    // ── Integration tests for download_and_verify ─────────────────────────────

    /// download_and_verify returns Err when SHA256SUMS.minisig is absent from
    /// the release directory. The pipeline must fail closed: no signature means
    /// no installation, even if SHA256SUMS itself is present.
    ///
    /// Setup: a `file://` release server with SHA256SUMS but no .minisig.
    /// The `file://` short-circuit in `download_to` makes this a pure I/O test
    /// with no network dependency.
    #[tokio::test]
    async fn self_update_fails_closed_without_minisig() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let test_root = std::env::temp_dir().join(format!("rayline-test-no-minisig-{unique}"));
        let dest_dir = std::env::temp_dir().join(format!("rayline-test-dest-{unique}"));

        // Create the release directory structure that download_and_verify expects.
        // checksums_url_for("1.0.0") → "<base>/cli/v1.0.0/SHA256SUMS"
        let release_dir = test_root.join("cli").join("v1.0.0");
        fs::create_dir_all(&release_dir).expect("create release dir");
        fs::create_dir_all(&dest_dir).expect("create dest dir");

        // SHA256SUMS is present — intentionally NO SHA256SUMS.minisig.
        fs::write(
            release_dir.join("SHA256SUMS"),
            b"abc123  rayline-macosx_11_0_arm64\ndef456  rld-macosx_11_0_arm64\n",
        )
        .expect("write SHA256SUMS");

        // Point download_and_verify at the local file tree.
        let base_url = format!("file://{}", test_root.display());
        // SAFETY: test binary is single-threaded (tokio::test with default
        // worker count 1); no other test reads RAYLINE_UPDATE_BASE_URL.
        unsafe { std::env::set_var(UPDATE_BASE_URL_ENV, &base_url) };
        let result = download_and_verify("1.0.0", "macosx_11_0_arm64", &dest_dir).await;
        unsafe { std::env::remove_var(UPDATE_BASE_URL_ENV) };

        // Cleanup best-effort (test trees are small)
        let _ = fs::remove_dir_all(&test_root);
        let _ = fs::remove_dir_all(&dest_dir);

        assert!(
            result.is_err(),
            "download_and_verify must return Err when SHA256SUMS.minisig is absent"
        );
    }
}
