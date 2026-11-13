use std::cmp::Ordering;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::UpdateChannel;

const CORE_LICENSE_NAME: &str = "rayline-core-LICENSE";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateRequest {
    pub pinned_version: Option<Version>,
    pub force: bool,
    pub check_only: bool,
    pub dry_run: bool,
    /// Hidden `--base-url`: where releases are read from.
    pub base_url: Option<Url>,
    /// Hidden `--install-path`: which binary is replaced.
    pub install_path: Option<PathBuf>,
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
    core: PathBuf,
    license: PathBuf,
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

/// Maps an io error to an install error that names the failed operation and
/// the path it was on.
pub(crate) fn io_failed<'a>(
    operation: &'a str,
    path: &'a Path,
) -> impl FnOnce(io::Error) -> UpdateError + 'a {
    move |error| UpdateError::Install(format!("failed to {operation} {}: {error}", path.display()))
}

/// Maps an io error to an install error that names the failed operation and
/// both of its paths.
fn io_failed_between<'a>(
    operation: &'a str,
    from: &'a Path,
    to: &'a Path,
) -> impl FnOnce(io::Error) -> UpdateError + 'a {
    move |error| {
        UpdateError::Install(format!(
            "failed to {operation} {} to {}: {error}",
            from.display(),
            to.display()
        ))
    }
}

/// `rayline update` on `channel`, the loaded `update.channel`.
pub async fn run(
    request: &UpdateRequest,
    channel: UpdateChannel,
) -> Result<UpdateResult, UpdateError> {
    run_on(request, channel, crate::MINISIGN_PUBLIC_KEYS).await
}

/// The pointer file naming the latest release on `channel`.
fn latest_pointer(channel: UpdateChannel) -> &'static str {
    match channel {
        UpdateChannel::Prod => "cli/latest.txt",
        UpdateChannel::Dev => "cli/latest-dev.txt",
        UpdateChannel::Main => "cli/latest-main.txt",
    }
}

/// `run` with the keys release signatures are checked against.
async fn run_on(
    request: &UpdateRequest,
    channel: UpdateChannel,
    pubkeys: &[&str],
) -> Result<UpdateResult, UpdateError> {
    let base_url = request
        .base_url
        .as_ref()
        .map_or(crate::UPDATE_BASE_URL, Url::as_str)
        .trim_end_matches('/');
    let install_path = install_path(request.install_path.as_deref()).map_err(|error| {
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
    if is_homebrew_managed(&install_path)? {
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
    let target = match &request.pinned_version {
        Some(version) => version.clone(),
        None => fetch_latest_version(base_url, channel, pubkeys).await?,
    };

    let decision = evaluate_check(
        &current,
        &target,
        request.pinned_version.is_some(),
        request.force,
    );
    if decision.exit_code == 0 || request.check_only {
        return Ok(decision);
    }

    let temp_dir = temp_update_dir()?;
    let artifacts = download_and_verify(base_url, &target, platform_tag, &temp_dir, pubkeys).await;
    let result = match artifacts {
        Ok(artifacts) => {
            if request.dry_run {
                Ok(UpdateResult {
                    message: format!(
                        "Would install {}, {} and {}.\n",
                        artifacts.launcher.display(),
                        artifacts.core.display(),
                        artifacts.license.display()
                    ),
                    exit_code: 0,
                    stderr: false,
                })
            } else {
                let core_install_path = install_path
                    .with_file_name(format!("rayline-core{}", std::env::consts::EXE_SUFFIX));
                let license_install_path = core_install_path.with_file_name(CORE_LICENSE_NAME);
                #[cfg(windows)]
                if let Err(error) = cleanup_retired_backups(&install_path) {
                    eprintln!(
                        "Could not finish old update backup cleanup: {error}. Continuing installation; a later update will retry."
                    );
                }
                replace_file(&artifacts.license, &license_install_path, false)?;
                replace_binary(&artifacts.core, &core_install_path)?;
                replace_binary(&artifacts.launcher, &install_path)?;
                crate::command_alias::install_alias(&install_path)?;
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

/// Releases are published under their full version, build label included
/// (`cli/v0.2.12+ee1afb00b6cd/`), and there is no index to resolve a bare
/// `X.Y.Z` from. A missing release named without a build label says so.
fn bare_version_hint(target: &Version, error: UpdateError) -> UpdateError {
    match error {
        UpdateError::Network(message)
            if !target.normalized.contains('+') && message.ends_with("HTTP 404") =>
        {
            UpdateError::Network(format!(
                "release {} was not found. Releases are published under their full version including the build label; run `raydock update --version {}+<build>` with the full version from the release notes (`raydock --version` prints it for an installed release).",
                target.public, target.public
            ))
        }
        error => error,
    }
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

async fn fetch_latest_version(
    base_url: &str,
    channel: UpdateChannel,
    pubkeys: &[&str],
) -> Result<Version, UpdateError> {
    let url = format!("{base_url}/{}", latest_pointer(channel));
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
    parse_verified_latest(&pointer, &signature, pubkeys)
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

fn artifact_url_for(base_url: &str, version: &str, platform_tag: &str, name: &str) -> String {
    format!("{base_url}/cli/v{version}/{name}-{platform_tag}")
}

fn checksums_url_for(base_url: &str, version: &str) -> String {
    format!("{base_url}/cli/v{version}/SHA256SUMS")
}

fn checksums_sig_url_for(base_url: &str, version: &str) -> String {
    format!("{base_url}/cli/v{version}/SHA256SUMS.minisig")
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
    base_url: &str,
    target: &Version,
    platform_tag: &str,
    dest_dir: &Path,
    pubkeys: &[&str],
) -> Result<UpdateArtifacts, UpdateError> {
    let version = target.normalized.as_str();
    fs::create_dir_all(dest_dir).map_err(io_failed("create", dest_dir))?;
    let launcher_name = format!("{}-{platform_tag}", crate::CLI_ASSET);
    let launcher_path = dest_dir.join(&launcher_name);
    let core_name = format!("rayline-core-{platform_tag}");
    let core_path = dest_dir.join(&core_name);
    let license_path = dest_dir.join(CORE_LICENSE_NAME);
    let sums_path = dest_dir.join("SHA256SUMS");
    let sig_path = dest_dir.join("SHA256SUMS.minisig");

    // Download checksums and its signature first so we can verify before
    // trusting any artifact bytes.
    download_to(&checksums_url_for(base_url, version), &sums_path)
        .await
        .map_err(|error| bare_version_hint(target, error))?;
    download_to(&checksums_sig_url_for(base_url, version), &sig_path).await?;

    // Verify signature BEFORE reading or trusting the checksums file.
    // Fail closed: missing or invalid signature aborts the update.
    let sums_bytes = fs::read(&sums_path).map_err(io_failed("read", &sums_path))?;
    let sig_bytes = fs::read(&sig_path).map_err(io_failed("read", &sig_path))?;
    verify_signature(&sums_bytes, &sig_bytes, pubkeys)?;

    let sums = String::from_utf8(sums_bytes)
        .map_err(|_| UpdateError::Checksum("SHA256SUMS contains invalid UTF-8".to_owned()))?;

    download_to(
        &artifact_url_for(base_url, version, platform_tag, crate::CLI_ASSET),
        &launcher_path,
    )
    .await?;
    download_to(
        &artifact_url_for(base_url, version, platform_tag, "rayline-core"),
        &core_path,
    )
    .await?;
    download_to(
        &format!("{base_url}/cli/v{version}/{CORE_LICENSE_NAME}"),
        &license_path,
    )
    .await?;

    verify_checksum(&sums, &launcher_name, &launcher_path)?;
    verify_checksum(&sums, &core_name, &core_path)?;
    verify_checksum(&sums, CORE_LICENSE_NAME, &license_path)?;

    Ok(UpdateArtifacts {
        launcher: launcher_path,
        core: core_path,
        license: license_path,
    })
}

pub(crate) async fn download_to(url: &str, path: &Path) -> Result<(), UpdateError> {
    let part = path.with_extension("part");
    if let Some(source) = url.strip_prefix("file://") {
        let source = Path::new(source);
        fs::copy(source, &part).map_err(io_failed_between("copy", source, &part))?;
        fs::rename(&part, path).map_err(io_failed_between("move", &part, path))?;
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
    fs::write(&part, bytes).map_err(io_failed("write", &part))?;
    fs::rename(&part, path).map_err(io_failed_between("move", &part, path))?;
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
    let bytes = fs::read(path).map_err(io_failed("read", path))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn install_path(option: Option<&Path>) -> io::Result<PathBuf> {
    let path = match option {
        Some(path) => path.to_path_buf(),
        None => std::env::current_exe()?,
    };
    Ok(crate::command_alias::primary_path(&resolve_install_path(
        path,
    )))
}

/// `current_exe()` may return the invoking symlink rather than its target on
/// some platforms (macOS), for example the `rayline` alias. Self-update must
/// replace the real binary, not the alias. A path that does not exist yet
/// (an `--install-path` naming a file to create) passes through unchanged.
fn resolve_install_path(path: PathBuf) -> PathBuf {
    fs::canonicalize(&path).unwrap_or(path)
}

/// True when `install_path` is under Homebrew's prefix. Without `brew` on
/// `PATH` nothing is Homebrew-managed; a `brew --prefix` that fails is an
/// error.
pub(crate) fn is_homebrew_managed(install_path: &Path) -> Result<bool, UpdateError> {
    Ok(match command_on_path("brew") {
        Some(brew) => path_is_under_prefix(install_path, &brew_prefix(&brew)?),
        None => false,
    })
}

/// True when the running `rayline` lives inside a `.app` bundle (directly, or via a
/// `/usr/local/bin` symlink created by the in-app "Install command line tools"
/// action). The app owns updates in that case; self-replacing would break the
/// notarization seal. Canonicalize first so symlink paths resolve to the real
/// bundle path before matching.
pub(crate) fn is_app_bundle_managed(install_path: &Path) -> bool {
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

/// The prefix `brew --prefix` prints, run with the `brew` at `brew`.
fn brew_prefix(brew: &Path) -> Result<PathBuf, UpdateError> {
    let failed = |reason: String| {
        UpdateError::Install(format!(
            "`brew --prefix` ({}) failed: {reason}",
            brew.display()
        ))
    };
    let output = Command::new(brew)
        .arg("--prefix")
        .stdin(Stdio::null())
        .output()
        .map_err(|error| failed(error.to_string()))?;
    if !output.status.success() {
        return Err(failed(output.status.to_string()));
    }
    let prefix = String::from_utf8(output.stdout)
        .map_err(|_| failed("it printed a prefix that is not UTF-8".to_owned()))?;
    match prefix.trim() {
        "" => Err(failed("it printed no prefix".to_owned())),
        prefix => Ok(PathBuf::from(prefix)),
    }
}

pub(crate) fn replace_binary(downloaded: &Path, install_path: &Path) -> Result<(), UpdateError> {
    replace_file(downloaded, install_path, true)
}

fn replace_file(
    downloaded: &Path,
    install_path: &Path,
    executable: bool,
) -> Result<(), UpdateError> {
    #[cfg(not(unix))]
    let _ = executable;
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
        let permissions = fs::Permissions::from_mode(if executable { 0o755 } else { 0o644 });
        fs::set_permissions(&staged, permissions).map_err(|error| {
            UpdateError::Install(format!(
                "failed to set update permissions at {}: {error}",
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
pub(crate) fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
    fs::rename(staged, install_path).map_err(|error| {
        UpdateError::Install(format!(
            "failed to replace {}: {error}",
            install_path.display()
        ))
    })
}

#[cfg(windows)]
pub(crate) fn move_into_place(staged: &Path, install_path: &Path) -> Result<(), UpdateError> {
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
            retire_replaced_backup(&backup, install_path);
            Ok(())
        }
        Err(error) => {
            // Restore the original so a failed update never leaves a hole.
            fs::rename(&backup, install_path).map_err(|restore_error| {
                UpdateError::Install(format!(
                    "failed to replace {}: {error}; restore also failed: {restore_error}. Original preserved at {}",
                    install_path.display(),
                    backup.display()
                ))
            })?;
            Err(UpdateError::Install(format!(
                "failed to replace {}: {error}",
                install_path.display()
            )))
        }
    }
}

/// Mark a committed replacement's old file for a later update to delete. The
/// running image is normally still locked now; retirement is only housekeeping
/// and must not stop installation of the remaining companion binaries.
#[cfg(any(windows, test))]
fn retire_replaced_backup(backup: &Path, install_path: &Path) {
    let result = sidecar_install_path(install_path, "old").and_then(|retired| {
        fs::rename(backup, &retired).map_err(io_failed_between("move", backup, &retired))
    });
    if let Err(error) = result {
        eprintln!(
            "Updated {}; retained previous file at {} because it could not be marked for cleanup: {error}. Remove it manually after older sessions and installers exit.",
            install_path.display(),
            backup.display()
        );
    }
}

pub(crate) fn staged_install_path(install_path: &Path) -> Result<PathBuf, UpdateError> {
    sidecar_install_path(install_path, "tmp")
}

fn sidecar_install_path(install_path: &Path, kind: &str) -> Result<PathBuf, UpdateError> {
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
    staged_name.push(format!(".{kind}-{}-{unique}", std::process::id()));
    Ok(parent.join(staged_name))
}

/// Before an actual update, delete backups from previous successful Windows swaps.
/// Historical .tmp files also name active staging and rollback files, so those
/// require manual cleanup after all installers and Raydock sessions have exited.
#[cfg(any(windows, test))]
fn cleanup_retired_backups(install_path: &Path) -> Result<(), UpdateError> {
    let parent = install_path.parent().ok_or_else(|| {
        UpdateError::Install(format!(
            "install path has no parent: {}",
            install_path.display()
        ))
    })?;
    for entry in fs::read_dir(parent).map_err(|error| {
        UpdateError::Install(format!(
            "cannot inspect update backups in {}: {error}",
            parent.display()
        ))
    })? {
        let entry = entry.map_err(io_failed("list update backups in", parent))?;
        if entry.file_name().to_str().is_some_and(is_retired_backup) {
            match entry.file_type() {
                Ok(kind) if kind.is_file() => remove_retired_backup(&entry.path())?,
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(io_failed("inspect", &entry.path())(error)),
            }
        }
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn is_retired_backup(name: &str) -> bool {
    [
        "raydock.exe",
        "rayline.exe",
        "rayline-core.exe",
        CORE_LICENSE_NAME,
    ]
    .iter()
    .any(|binary| {
        name.strip_prefix(&format!(".{binary}.old-"))
            .and_then(|suffix| suffix.split_once('-'))
            .is_some_and(|(pid, nonce)| {
                !pid.is_empty()
                    && pid.bytes().all(|b| b.is_ascii_digit())
                    && !nonce.is_empty()
                    && nonce.bytes().all(|b| b.is_ascii_hexdigit())
            })
    })
}

#[cfg(any(windows, test))]
fn remove_retired_backup(path: &Path) -> Result<(), UpdateError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        // Another cleanup may have removed the same committed backup.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error)
            if error.kind() == io::ErrorKind::PermissionDenied
                || (cfg!(windows) && matches!(error.raw_os_error(), Some(32 | 33))) =>
        {
            eprintln!(
                "Retained old executable {}: {error}. Close older Raydock sessions; a later update or installer will retry cleanup.",
                path.display()
            );
            Ok(())
        }
        Err(error) => Err(UpdateError::Install(format!(
            "cannot remove retired update backup {}: {error}",
            path.display()
        ))),
    }
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
    fs::create_dir_all(&path).map_err(io_failed("create", &path))?;
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

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

    #[test]
    fn a_failing_brew_prefix_is_an_error_naming_it() {
        let root = std::env::temp_dir().join(format!("raydock-brew-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let missing = root.join("no-such-brew");
        let error = brew_prefix(&missing).unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("`brew --prefix` ({}) failed: ", missing.display())),
            "{error}"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = |name: &str, body: &str| {
                let path = root.join(name);
                fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
                path
            };
            let exits = script("exits", "exit 3");
            assert_eq!(
                brew_prefix(&exits).unwrap_err().to_string(),
                format!(
                    "`brew --prefix` ({}) failed: exit status: 3",
                    exits.display()
                )
            );
            let silent = script("silent", "echo");
            assert_eq!(
                brew_prefix(&silent).unwrap_err().to_string(),
                format!(
                    "`brew --prefix` ({}) failed: it printed no prefix",
                    silent.display()
                )
            );
            let prints = script("prints", "echo /opt/homebrew");
            assert_eq!(
                brew_prefix(&prints).unwrap(),
                PathBuf::from("/opt/homebrew")
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn an_io_failure_names_the_operation_and_path() {
        let path = std::env::temp_dir().join("raydock-update-no-such-file");
        let error = sha256_file(&path).unwrap_err().to_string();
        assert!(
            error.starts_with(&format!("failed to read {}: ", path.display())),
            "{error}"
        );
    }

    #[test]
    fn cleanup_removes_only_committed_windows_backups() -> TestResult {
        let root = temp_update_dir()?;
        let retired = [
            ".raydock.exe.old-123-456",
            ".rayline.exe.old-123-abcdef",
            ".rayline-core.exe.old-123-456",
            ".rayline-core-LICENSE.old-123-456",
        ];
        let retained = [
            "raydock.exe",
            ".raydock.exe.tmp-123-456",
            ".rayline.exe.tmp-123-456",
            ".raydock.exe.old-manual-backup",
            ".unrelated.exe.old-123-456",
            "rayline.exe.old-0123456789abcdef0123456789abcdef",
            "rayline.exe.old-incomplete",
        ];
        for name in retired.iter().chain(retained.iter()) {
            fs::write(root.join(name), b"payload")?;
        }
        let installed = root.join("raydock.exe");
        let backup = staged_install_path(&installed)?;
        fs::write(&backup, b"previous executable")?;
        retire_replaced_backup(&backup, &installed);
        assert!(!backup.exists());
        let entries = fs::read_dir(&root)?.collect::<io::Result<Vec<_>>>()?;
        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.file_name().to_str().is_some_and(is_retired_backup))
                .count(),
            retired.len() + 1,
            "retirement must keep the old file until a later sweep"
        );
        let directory = root.join(".rayline-core.exe.old-123-789");
        fs::create_dir(&directory)?;
        cleanup_retired_backups(&root.join("raydock.exe"))?;
        for name in retired {
            assert!(!root.join(name).exists(), "retired file {name}");
        }
        for name in retained {
            assert_eq!(fs::read(root.join(name))?, b"payload", "retained {name}");
        }
        assert_eq!(fs::read_dir(&root)?.count(), retained.len() + 1);
        assert!(directory.is_dir());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_cleanup_retries_locked_retired_backup() -> TestResult {
        use std::os::windows::fs::OpenOptionsExt;

        let root = temp_update_dir()?;
        let backup = root.join(".raydock.exe.old-123-456");
        fs::write(&backup, b"old executable")?;
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)?;
        cleanup_retired_backups(&root.join("raydock.exe"))?;
        assert!(backup.exists());
        drop(locked);
        cleanup_retired_backups(&root.join("raydock.exe"))?;
        assert!(!backup.exists());
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_swap_defers_cleanup_and_preserves_failed_swap() -> TestResult {
        let root = temp_update_dir()?;
        let installed = root.join("raydock.exe");
        let staged = root.join("staged.exe");
        fs::write(&installed, b"original")?;
        fs::write(&staged, b"new")?;
        move_into_place(&staged, &installed)?;
        assert_eq!(fs::read_dir(&root)?.count(), 2);
        assert_eq!(fs::read(&installed)?, b"new");
        assert!(move_into_place(&root.join("missing-staged.exe"), &installed).is_err());
        cleanup_retired_backups(&installed)?;
        assert_eq!(fs::read(&installed)?, b"new");
        assert_eq!(fs::read_dir(&root)?.count(), 1);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    fn windows_retirement_failure_preserves_committed_install_and_backup() -> TestResult {
        use std::os::windows::fs::OpenOptionsExt;

        let root = temp_update_dir()?;
        let installed = root.join("raydock.exe");
        let backup = staged_install_path(&installed)?;
        fs::write(&installed, b"committed")?;
        fs::write(&backup, b"previous")?;
        let locked = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&backup)?;
        retire_replaced_backup(&backup, &installed);
        assert!(backup.exists());
        assert_eq!(fs::read(&installed)?, b"committed");
        drop(locked);
        assert_eq!(fs::read(&backup)?, b"previous");
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn core_license_installs_as_verified_nonexecutable_data() -> TestResult {
        let root = temp_update_dir()?;
        let source = root.join("rayline-core-LICENSE");
        let destination = root.join("installed/rayline-core-LICENSE");
        let content = b"Pinned license fixture\n";
        fs::write(&source, content)?;
        let sums = format!("{:x}  rayline-core-LICENSE\n", Sha256::digest(content));
        verify_checksum(&sums, "rayline-core-LICENSE", &source)?;
        replace_file(&source, &destination, false)?;
        assert_eq!(fs::read(&destination)?, content);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&destination)?.permissions().mode() & 0o777,
                0o644
            );
        }
        fs::write(&source, b"tampered license")?;
        assert!(verify_checksum(&sums, "rayline-core-LICENSE", &source).is_err());
        assert_eq!(fs::read(&destination)?, content);
        fs::remove_dir_all(root)?;
        Ok(())
    }

    fn version(raw: &str) -> Version {
        Version::parse(raw).unwrap_or_else(|_| panic!("test version {raw} parses"))
    }

    #[test]
    fn a_missing_bare_version_names_the_full_form() {
        let not_found = || UpdateError::Network("GET x returned HTTP 404".to_owned());
        let UpdateError::Network(message) = bare_version_hint(&version("0.2.12"), not_found())
        else {
            panic!("network error expected");
        };
        assert!(message.contains("--version 0.2.12+<build>"), "{message}");
        // A full version, or another failure, keeps its own error.
        assert_eq!(
            bare_version_hint(&version("0.2.12+ee1afb00b6cd"), not_found()),
            not_found()
        );
        let other = UpdateError::Network("GET x returned HTTP 500".to_owned());
        assert_eq!(bare_version_hint(&version("0.2.12"), other.clone()), other);
    }

    // ── Signature verification tests ─────────────────────────────────────────

    // Test public key (matches the .minisig in tests/fixtures/SHA256SUMS.minisig)
    // Generated for testing only — NOT the production key.
    const TEST_PUBKEY: &str = "RWRrxu13/uJmEyk8KO8zSLW71qzyxIAYFeYo1nde3MpRDaEwS7yVv2U1";

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
            b"abc123  rayline-macosx_11_0_arm64\ndef456  rayline-core-macosx_11_0_arm64\n",
        )
        .expect("write SHA256SUMS");

        // Point download_and_verify at the local file tree.
        let base_url = format!("file://{}", test_root.display());
        let result = download_and_verify(
            &base_url,
            &version("1.0.0"),
            "macosx_11_0_arm64",
            &dest_dir,
            &[TEST_PUBKEY],
        )
        .await;

        // Cleanup best-effort (test trees are small)
        let _ = fs::remove_dir_all(&test_root);
        let _ = fs::remove_dir_all(&dest_dir);

        assert!(
            result.is_err(),
            "download_and_verify must return Err when SHA256SUMS.minisig is absent"
        );
    }

    // ── update against a fake release server ─────────────────────────────────

    // Signs tests/fixtures/release-*SHA256SUMS. Testing only — NOT the
    // production key.
    const RELEASE_TEST_PUBKEY: &str = "RWQ7upEBuCoV7z5WLNio5DhgB/JWqJVgNLY9gCH96m8L8UV+8rhE6fUV";
    const TEST_RELEASE: &str = "99.0.0";
    const LAUNCHER: &[u8] = b"fixture launcher\n";
    const CORE: &[u8] = b"fixture core\n";
    const LICENSE: &[u8] = b"fixture license\n";

    /// A fixture as it was signed: a checkout may have turned its LF into CRLF.
    fn signed_fixture(name: &str) -> Vec<u8> {
        let mut bytes = fixture(name);
        bytes.retain(|byte| *byte != b'\r');
        bytes
    }

    type Files = std::collections::HashMap<String, Vec<u8>>;

    /// An HTTP release server on 127.0.0.1 serving `files` by path, recording
    /// every path it is asked for.
    struct FakeReleaseServer {
        base_url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl FakeReleaseServer {
        fn start(files: Files) -> Self {
            use std::io::{BufRead, Write};

            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind fake server");
            let base_url = format!("http://{}", listener.local_addr().expect("server address"));
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let log = std::sync::Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let Ok(reader) = stream.try_clone() else {
                        continue;
                    };
                    let mut reader = io::BufReader::new(reader);
                    let mut request_line = String::new();
                    if reader.read_line(&mut request_line).is_err() {
                        continue;
                    }
                    let mut header = String::new();
                    while reader.read_line(&mut header).is_ok_and(|read| read > 2) {
                        header.clear();
                    }
                    let path = request_line.split_whitespace().nth(1).unwrap_or("");
                    let (status, body) = match files.get(path) {
                        Some(body) => ("200 OK", body.as_slice()),
                        None => ("404 Not Found", &[][..]),
                    };
                    log.lock().expect("request log").push(path.to_owned());
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                }
            });
            Self { base_url, requests }
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().expect("request log").clone()
        }
    }

    /// Release files for TEST_RELEASE on this platform, with `sums` naming the
    /// SHA256SUMS fixture (signed with RELEASE_TEST_PUBKEY).
    fn release_files(sums: &str) -> Files {
        let release = format!("/cli/v{TEST_RELEASE}");
        let mut files = Files::from([
            (format!("{release}/SHA256SUMS"), signed_fixture(sums)),
            (
                format!("{release}/SHA256SUMS.minisig"),
                signed_fixture(&format!("{sums}.minisig")),
            ),
            (format!("{release}/{CORE_LICENSE_NAME}"), LICENSE.to_vec()),
        ]);
        let platform = detect_platform_tag().expect("supported test platform");
        for (name, bytes) in [(crate::CLI_ASSET, LAUNCHER), ("rayline-core", CORE)] {
            files.insert(format!("{release}/{name}-{platform}"), bytes.to_vec());
        }
        files
    }

    fn exe(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
    }

    /// An install directory holding only `raydock`, and a request that
    /// replaces it from `server`.
    fn install_from(server: &FakeReleaseServer) -> TestResult<(PathBuf, UpdateRequest)> {
        let install = temp_update_dir()?;
        let installed = exe(&install, crate::CLI_BIN);
        fs::write(&installed, b"installed")?;
        let request = UpdateRequest {
            pinned_version: Some(version(TEST_RELEASE)),
            force: false,
            check_only: false,
            dry_run: false,
            base_url: Some(Url::parse(&format!("{}/", server.base_url)).unwrap()),
            install_path: Some(installed),
        };
        Ok((install, request))
    }

    #[tokio::test]
    async fn check_reads_the_channel_pointer_from_the_base_url() -> TestResult {
        for (channel, pointer) in [
            (UpdateChannel::Prod, "/cli/latest.txt"),
            (UpdateChannel::Dev, "/cli/latest-dev.txt"),
            (UpdateChannel::Main, "/cli/latest-main.txt"),
        ] {
            let server = FakeReleaseServer::start(Files::from([
                (pointer.to_owned(), signed_fixture("latest.txt")),
                (
                    format!("{pointer}.minisig"),
                    signed_fixture("latest.txt.minisig"),
                ),
            ]));
            let (install, request) = install_from(&server)?;
            fs::write(exe(&install, "rayline-core"), b"installed core")?;
            let request = UpdateRequest {
                pinned_version: None,
                check_only: true,
                ..request
            };
            let result = run_on(&request, channel, &[LATEST_TEST_PUBKEY]).await?;
            assert_eq!(result.exit_code, 1, "{}", result.message);
            assert!(
                result.message.contains(&format!("\u{2192} {TEST_RELEASE}")),
                "{}",
                result.message
            );
            assert_eq!(
                server.requests(),
                [pointer.to_owned(), format!("{pointer}.minisig")]
            );
            assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
            fs::remove_dir_all(install)?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn update_installs_the_release_at_the_install_path() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let installed = exe(&install, crate::CLI_BIN);

        let dry_run = UpdateRequest {
            dry_run: true,
            ..request.clone()
        };
        let result = run_on(&dry_run, UpdateChannel::Prod, &[RELEASE_TEST_PUBKEY]).await?;
        assert!(
            result.message.starts_with("Would install"),
            "{}",
            result.message
        );
        assert_eq!(
            fs::read_dir(&install)?.count(),
            1,
            "a dry run installs nothing"
        );
        assert_eq!(fs::read(&installed)?, b"installed");

        let result = run_on(&request, UpdateChannel::Prod, &[RELEASE_TEST_PUBKEY]).await?;
        assert_eq!(result.exit_code, 0, "{}", result.message);
        assert!(result.message.contains(TEST_RELEASE), "{}", result.message);
        assert_eq!(fs::read(&installed)?, LAUNCHER);
        assert_eq!(fs::read(exe(&install, "rayline-core"))?, CORE);
        assert_eq!(fs::read(install.join(CORE_LICENSE_NAME))?, LICENSE);
        assert!(fs::symlink_metadata(exe(&install, "rayline")).is_ok());
        // Windows keeps the replaced launcher as a retired backup.
        let mut installed_names = fs::read_dir(&install)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<Vec<_>>>()?;
        installed_names.retain(|name| !is_retired_backup(name));
        installed_names.sort();
        let mut expected_names = [crate::CLI_BIN, "rayline", "rayline-core"]
            .map(|name| format!("{name}{}", std::env::consts::EXE_SUFFIX))
            .to_vec();
        expected_names.push(CORE_LICENSE_NAME.to_owned());
        expected_names.sort();
        assert_eq!(installed_names, expected_names);
        // A pinned version reads no channel pointer.
        let release = format!("/cli/v{TEST_RELEASE}/");
        assert!(
            server
                .requests()
                .iter()
                .all(|path| path.starts_with(&release)),
            "{:?}",
            server.requests()
        );
        fs::remove_dir_all(install)?;
        Ok(())
    }

    #[tokio::test]
    async fn update_refuses_a_release_without_the_core() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-no-core-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let result = run_on(&request, UpdateChannel::Prod, &[RELEASE_TEST_PUBKEY]).await;
        let Err(UpdateError::Checksum(message)) = result else {
            panic!("a release without the core must fail closed: {result:?}");
        };
        assert!(message.contains("rayline-core-"), "{message}");
        assert_eq!(fs::read_dir(&install)?.count(), 1);
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        fs::remove_dir_all(install)?;
        Ok(())
    }

    #[tokio::test]
    async fn update_refuses_a_release_signed_by_another_key() -> TestResult {
        let server = FakeReleaseServer::start(release_files("release-SHA256SUMS"));
        let (install, request) = install_from(&server)?;
        let result = run_on(&request, UpdateChannel::Prod, crate::MINISIGN_PUBLIC_KEYS).await;
        assert!(
            matches!(result, Err(UpdateError::Signature(_))),
            "{result:?}"
        );
        assert_eq!(fs::read_dir(&install)?.count(), 1);
        assert_eq!(fs::read(exe(&install, crate::CLI_BIN))?, b"installed");
        fs::remove_dir_all(install)?;
        Ok(())
    }
}
