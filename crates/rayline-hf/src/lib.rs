//! Downloads files from the Hugging Face Hub into the Hugging Face cache.
//!
//! The on-disk layout (models--org--repo/{refs,blobs,snapshots/{commit}}) is
//! byte-identical to what huggingface_hub creates, so Rayline can reuse an
//! existing Hugging Face cache.

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::ffi::OsString;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{info, warn};

#[derive(Debug, Deserialize)]
struct HfModelInfo {
    sha: String,
}

// ---------------------------------------------------------------------------
// Retry classification (inlined from download_utils.rs)
// ---------------------------------------------------------------------------

const MAX_DOWNLOAD_ATTEMPTS: u32 = 4;

enum DownloadAttemptError {
    Fatal(String),
    Retryable(String),
}

fn is_retryable_io_error(e: &std::io::Error) -> bool {
    use std::io::ErrorKind;
    matches!(
        e.kind(),
        ErrorKind::UnexpectedEof
            | ErrorKind::ConnectionReset
            | ErrorKind::ConnectionAborted
            | ErrorKind::TimedOut
            | ErrorKind::BrokenPipe
            | ErrorKind::Interrupted
            | ErrorKind::Other
    )
}

fn is_retryable_reqwest_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_body() || e.is_request()
}

// ---------------------------------------------------------------------------
// Cache directory resolution
// ---------------------------------------------------------------------------

/// The HF cache directory: `HF_HUB_CACHE`, else `HF_HOME`/hub, else
/// `<home>/.cache/huggingface/hub`. A set but empty or non-unicode var is an
/// error naming it.
pub fn hf_cache_dir(home: &Path) -> Result<PathBuf, String> {
    hf_cache_dir_from(home, |name| std::env::var_os(name))
}

fn hf_cache_dir_from(
    home: &Path,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<PathBuf, String> {
    let read = |name: &str| -> Result<Option<PathBuf>, String> {
        let Some(value) = var(name) else {
            return Ok(None);
        };
        match value.into_string() {
            Ok(value) if value.is_empty() => Err(format!("{name} is set but empty")),
            Ok(value) => Ok(Some(PathBuf::from(value))),
            Err(_) => Err(format!("{name} is not valid unicode")),
        }
    };
    if let Some(dir) = read("HF_HUB_CACHE")? {
        return Ok(dir);
    }
    if let Some(hf_home) = read("HF_HOME")? {
        return Ok(hf_home.join("hub"));
    }
    Ok(home.join(".cache").join("huggingface").join("hub"))
}

/// "org/repo" → "models--org--repo".
pub fn repo_to_folder_name(repo: &str) -> String {
    format!("models--{}", repo.replace('/', "--"))
}

// ---------------------------------------------------------------------------
// HF API
// ---------------------------------------------------------------------------

/// Latest commit hash for a repo via the HF API.
pub fn hf_api_get_commit(repo: &str) -> Result<String, String> {
    let url = format!("{}/api/models/{repo}", hf_endpoint()?);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    let response = client
        .get(&url)
        .header("User-Agent", "rayline")
        .send()
        .map_err(|e| format!("HF API request failed: {e}"))?;

    if !response.status().is_success() {
        return Err(format!("HF API returned status {}", response.status()));
    }

    let body = response
        .text()
        .map_err(|e| format!("Failed to read HF API response: {e}"))?;
    let info: HfModelInfo =
        serde_json::from_str(&body).map_err(|e| format!("Failed to parse HF API response: {e}"))?;

    Ok(info.sha)
}

// ---------------------------------------------------------------------------
// SHA256
// ---------------------------------------------------------------------------

fn compute_sha256(path: &Path) -> Result<String, String> {
    let mut file =
        fs::File::open(path).map_err(|e| format!("Failed to open file for hashing: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("Read error during hashing: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn sha256_blob_name(digest: &str) -> String {
    format!("sha256-{digest}")
}

pub fn hf_cache_snapshot_path(hub: &Path, repo: &str, filename: &str, commit: &str) -> PathBuf {
    hub.join(repo_to_folder_name(repo))
        .join("snapshots")
        .join(commit)
        .join(filename)
}

/// The pinned snapshot file, when it is in the cache.
pub fn cached_hf_file(hub: &Path, repo: &str, filename: &str, commit: &str) -> Option<PathBuf> {
    let snapshot = hf_cache_snapshot_path(hub, repo, filename, commit);
    snapshot.is_file().then_some(snapshot)
}

// ---------------------------------------------------------------------------
// Download to HF cache
// ---------------------------------------------------------------------------

/// Download a file into the HF cache at `hub`. Returns the snapshot symlink
/// path. Steps mirror `huggingface_hub`: write to `blobs/.download-*`,
/// SHA256-rename to `blobs/sha256-{hash}`, write `refs/main`, symlink
/// `snapshots/{commit}/{filename}` → blob.
pub fn download_to_hf_cache(
    hub: &Path,
    repo: &str,
    filename: &str,
    commit: &str,
) -> Result<PathBuf, String> {
    download_to_hf_cache_at(
        hub,
        &hf_endpoint()?,
        Download::default(),
        repo,
        filename,
        commit,
    )
}

/// How [`download_to_hf_cache_at`] downloads a file.
#[derive(Debug, Clone, Copy, Default)]
pub struct Download<'a> {
    /// Sent as a bearer token to the Hub only: over https, or over http to
    /// a loopback address.
    pub token: Option<&'a str>,
    /// A download that grows past this many bytes is stopped and refused.
    pub max_bytes: Option<u64>,
    /// The commit is a pin, not the repo's `main`: `refs/main` is left as
    /// it is.
    pub pinned: bool,
}

/// Whether a bearer token may go to `base_url`: https, or http to a
/// loopback address.
fn may_carry_token(base_url: &str) -> bool {
    match reqwest::Url::parse(base_url) {
        Ok(url) if url.scheme() == "https" => true,
        Ok(url) if url.scheme() == "http" => url.host_str().is_some_and(|host| {
            host == "localhost"
                || host
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        }),
        _ => false,
    }
}

/// The Hub token for private repos: `HF_TOKEN`, else the token file
/// `huggingface-cli login` writes (`$HF_HOME/token`, else
/// `<home>/.cache/huggingface/token`), else none. A set but empty or
/// non-unicode `HF_TOKEN` is an error naming it. The token itself never
/// appears in a log or an error.
pub fn hf_token(home: &Path) -> Result<Option<String>, String> {
    hf_token_from(home, |name| std::env::var_os(name))
}

fn hf_token_from(
    home: &Path,
    var: impl Fn(&str) -> Option<OsString>,
) -> Result<Option<String>, String> {
    if let Some(value) = var("HF_TOKEN") {
        return match value.into_string() {
            Ok(value) if value.trim().is_empty() => Err("HF_TOKEN is set but empty".to_owned()),
            Ok(value) => Ok(Some(value.trim().to_owned())),
            Err(_) => Err("HF_TOKEN is not valid unicode".to_owned()),
        };
    }
    let hf_home = match var("HF_HOME").map(OsString::into_string) {
        Some(Ok(dir)) if !dir.is_empty() => PathBuf::from(dir),
        Some(_) => return Err("HF_HOME is set but empty or not valid unicode".to_owned()),
        None => home.join(".cache").join("huggingface"),
    };
    let file = hf_home.join("token");
    match fs::read_to_string(&file) {
        Ok(token) if token.trim().is_empty() => Ok(None),
        Ok(token) => Ok(Some(token.trim().to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "cannot read the Hugging Face token file {}: {e}",
            file.display()
        )),
    }
}

/// The Hub's address: `HF_ENDPOINT`, else `https://huggingface.co`. A set
/// value must be an absolute `http` or `https` URL; anything else is an error
/// naming the var.
pub fn hf_endpoint() -> Result<String, String> {
    hf_endpoint_from(std::env::var_os("HF_ENDPOINT"))
}

fn hf_endpoint_from(value: Option<OsString>) -> Result<String, String> {
    let Some(value) = value else {
        return Ok("https://huggingface.co".to_owned());
    };
    let value = value
        .into_string()
        .map_err(|_| "HF_ENDPOINT is not valid unicode".to_owned())?;
    let url = reqwest::Url::parse(&value)
        .ok()
        // The parser reads `https:///x` as host `x`; a host must follow `//`.
        .filter(|url| {
            matches!(url.scheme(), "http" | "https")
                && url.has_host()
                && !value[url.scheme().len()..].starts_with(":///")
        })
        .ok_or_else(|| "HF_ENDPOINT is not an absolute http or https URL".to_owned())?;
    // A user name or password in the URL would reach logs and cache files.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(
            "HF_ENDPOINT carries a user name or password; give the Hub's address only".to_owned(),
        );
    }
    // Repo paths are appended to it.
    if url.query().is_some() || url.fragment().is_some() {
        return Err("HF_ENDPOINT has a query or fragment; give the Hub's address only".to_owned());
    }
    Ok(value.trim_end_matches('/').to_owned())
}

/// Download a file from the Hub at `hf_base_url` into the HF cache at
/// `cache`, as [`download_to_hf_cache`] does.
///
/// With a token, every request to `hf_base_url` carries it as a bearer
/// token, and a plain-http `hf_base_url` that isn't loopback is refused, as
/// is any redirect to such a URL. A redirect to another host drops it (the
/// HTTP client removes `Authorization` on a cross-host redirect).
pub fn download_to_hf_cache_at(
    cache: &Path,
    hf_base_url: &str,
    download: Download<'_>,
    repo: &str,
    filename: &str,
    commit: &str,
) -> Result<PathBuf, String> {
    let token = download.token;
    if token.is_some() && !may_carry_token(hf_base_url) {
        return Err(format!(
            "HF_ENDPOINT {hf_base_url} is plain http: the Hugging Face token is sent only over https (or to a loopback address)"
        ));
    }
    let repo_dir = cache.join(repo_to_folder_name(repo));
    let refs_dir = repo_dir.join("refs");
    let blobs_dir = repo_dir.join("blobs");
    let snapshot_dir = repo_dir.join("snapshots").join(commit);

    fs::create_dir_all(&refs_dir).map_err(|e| format!("Failed to create refs dir: {e}"))?;
    fs::create_dir_all(&blobs_dir).map_err(|e| format!("Failed to create blobs dir: {e}"))?;
    fs::create_dir_all(&snapshot_dir).map_err(|e| format!("Failed to create snapshot dir: {e}"))?;

    let snapshot_file = snapshot_dir.join(filename);
    if let Some(parent) = snapshot_file.parent() {
        if parent != snapshot_dir {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create snapshot subdir: {e}"))?;
        }
    }

    let safe_name = filename.replace('/', "_");
    let tmp_path = blobs_dir.join(format!(".download-{safe_name}.tmp"));
    // Sidecar that records which URL produced the current `.tmp`. Lets us
    // resume across process restarts: if the URL matches we keep the
    // partial file and ask the server for a Range; if it differs (or is
    // missing) we wipe the stale bytes before re-downloading.
    let tmp_url_path = blobs_dir.join(format!(".download-{safe_name}.tmp.url"));
    let download_url = format!(
        "{}/{repo}/resolve/{commit}/{filename}",
        hf_base_url.trim_end_matches('/')
    );

    let prior_url = fs::read_to_string(&tmp_url_path).ok();
    let resumable = prior_url.as_deref().map(str::trim) == Some(download_url.as_str());
    if !resumable {
        if tmp_path.exists() {
            info!(
                "[HfCache] Discarding stale partial download at {:?} (URL changed)",
                tmp_path
            );
            let _ = fs::remove_file(&tmp_path);
        }
        if let Err(e) = fs::write(&tmp_url_path, &download_url) {
            warn!(
                "[HfCache] Failed to write tmp URL sidecar {:?}: {}",
                tmp_url_path, e
            );
        }
    } else if let Ok(meta) = fs::metadata(&tmp_path) {
        info!(
            "[HfCache] Resuming previous download at {:?} ({} bytes already on disk)",
            tmp_path,
            meta.len()
        );
    }

    info!(
        "[HfCache] Downloading {} -> temp {:?}",
        download_url, tmp_path
    );

    download_file_to_path(&download_url, token, download.max_bytes, &tmp_path)?;

    info!("[HfCache] Computing SHA256 for {:?}", tmp_path);
    let sha256 = compute_sha256(&tmp_path)?;
    let blob_name = sha256_blob_name(&sha256);
    let blob_path = blobs_dir.join(&blob_name);

    // An existing blob is kept only when its bytes still hash to its name;
    // a corrupt one is replaced by the fresh download.
    if blob_path.exists() && compute_sha256(&blob_path).is_ok_and(|sha| sha == sha256) {
        let _ = fs::remove_file(&tmp_path);
        info!("[HfCache] Blob already exists: {:?}", blob_path);
    } else {
        // Windows doesn't rename over an existing file.
        let _ = fs::remove_file(&blob_path);
        fs::rename(&tmp_path, &blob_path).map_err(|e| format!("Failed to rename blob: {e}"))?;
        info!("[HfCache] Blob created: {:?}", blob_path);
    }

    let _ = fs::remove_file(&tmp_url_path);

    if !download.pinned {
        let refs_main = refs_dir.join("main");
        let mut f =
            fs::File::create(&refs_main).map_err(|e| format!("Failed to create refs/main: {e}"))?;
        f.write_all(commit.as_bytes())
            .map_err(|e| format!("Failed to write refs/main: {e}"))?;
    }

    let symlink_target = snapshot_symlink_target(filename, &blob_name);

    let _ = fs::remove_file(&snapshot_file);
    create_symlink_or_copy(&symlink_target, &snapshot_file, &blob_path)?;

    info!(
        "[HfCache] Cache entry created: {:?} -> {:?}",
        snapshot_file, symlink_target
    );

    Ok(snapshot_file)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// The snapshot file's link to its blob, relative to the snapshot file's
/// directory (`snapshots/<commit>/` plus one level per `/` in `filename`).
/// It is built from path components, so it uses the platform's separator.
fn snapshot_symlink_target(filename: &str, blob_name: &str) -> PathBuf {
    let mut target = PathBuf::from("..");
    target.push("..");
    for _ in 0..filename.matches('/').count() {
        target.push("..");
    }
    target.push("blobs");
    target.push(blob_name);
    target
}

fn create_symlink_or_copy(
    symlink_target: &Path,
    symlink_path: &Path,
    _blob_path: &Path,
) -> Result<(), String> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(symlink_target, symlink_path)
            .map_err(|e| format!("Failed to create symlink: {e}"))?;
    }

    #[cfg(windows)]
    {
        match std::os::windows::fs::symlink_file(symlink_target, symlink_path) {
            Ok(()) => {}
            Err(e) => {
                warn!("[HfCache] Symlink creation failed ({e}), falling back to copy");
                fs::copy(_blob_path, symlink_path)
                    .map_err(|e| format!("Failed to copy blob to snapshot: {e}"))?;
            }
        }
    }

    #[cfg(not(any(unix, windows)))]
    {
        fs::copy(_blob_path, symlink_path)
            .map_err(|e| format!("Failed to copy blob to snapshot: {e}"))?;
    }

    Ok(())
}

fn download_file_to_path(
    url: &str,
    token: Option<&str>,
    max_bytes: Option<u64>,
    dest_path: &Path,
) -> Result<u64, String> {
    info!("[HfCache] Downloading {} -> {:?}", url, dest_path);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    // With a token, no redirect may lead to plain http: the client keeps
    // `Authorization` when a redirect stays on the same host and port.
    let carries_token = token.is_some();
    let redirects = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= 10 {
            attempt.error("too many redirects")
        } else if carries_token && !may_carry_token(attempt.url().as_str()) {
            attempt.error("a redirect to plain http, where the Hugging Face token isn't sent")
        } else {
            attempt.follow()
        }
    });
    let client = reqwest::blocking::Client::builder()
        .redirect(redirects)
        .timeout(Duration::from_secs(3600))
        .connect_timeout(Duration::from_secs(30))
        .tcp_keepalive(Duration::from_secs(30))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {e}"))?;

    // No unconditional pre-delete here: the caller (download_to_hf_cache)
    // already validates that any pre-existing tmp file belongs to this same
    // URL via the .tmp.url sidecar. Keeping the partial bytes around lets
    // attempt_download resume mid-file across process restarts.

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match attempt_download(url, token, max_bytes, dest_path, &client) {
            Ok(bytes) => return Ok(bytes),
            Err(DownloadAttemptError::Fatal(msg)) => {
                let _ = fs::remove_file(dest_path);
                return Err(msg);
            }
            Err(DownloadAttemptError::Retryable(msg)) => {
                if attempt >= MAX_DOWNLOAD_ATTEMPTS {
                    let _ = fs::remove_file(dest_path);
                    return Err(format!("{msg} (failed after {attempt} attempts)"));
                }
                let backoff = Duration::from_secs(1u64 << (attempt - 1).min(4));
                warn!(
                    "[HfCache] Transient error on attempt {}/{}: {}. Retrying in {:?}.",
                    attempt, MAX_DOWNLOAD_ATTEMPTS, msg, backoff
                );
                std::thread::sleep(backoff);
            }
        }
    }
}

fn attempt_download(
    url: &str,
    token: Option<&str>,
    max_bytes: Option<u64>,
    dest_path: &Path,
    client: &reqwest::blocking::Client,
) -> Result<u64, DownloadAttemptError> {
    // Honor any partial file on disk — either retained from a previous
    // process invocation (via the .tmp.url sidecar guarding the call site)
    // or left by a previous retry within this same invocation.
    let existing_bytes = fs::metadata(dest_path).map(|m| m.len()).unwrap_or(0);

    let mut request = client.get(url).header("User-Agent", "rayline");
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if existing_bytes > 0 {
        request = request.header("Range", format!("bytes={existing_bytes}-"));
    }

    let response = match request.send() {
        Ok(r) => r,
        Err(e) => {
            let msg = match std::error::Error::source(&e) {
                Some(reason) if e.is_redirect() => {
                    format!("Download request failed: {e}: {reason}")
                }
                _ => format!("Download request failed: {e}"),
            };
            return if is_retryable_reqwest_error(&e) {
                Err(DownloadAttemptError::Retryable(msg))
            } else {
                Err(DownloadAttemptError::Fatal(msg))
            };
        }
    };

    let status = response.status();
    if !status.is_success() {
        // 416 Range Not Satisfiable means the partial file on disk is already
        // at (or past) EOF — common when a previous run got killed between
        // the download finishing and the blob being promoted. Wipe the
        // local partial and retry from scratch instead of failing fatally.
        if status.as_u16() == 416 && existing_bytes > 0 {
            let _ = fs::remove_file(dest_path);
            return Err(DownloadAttemptError::Retryable(format!(
                "Server reported partial file out of range ({existing_bytes} bytes); restarting from zero"
            )));
        }
        if matches!(status.as_u16(), 401 | 403) {
            let auth = if token.is_some() {
                "the Hugging Face token (HF_TOKEN, or the token file) has no access to it"
            } else {
                "the repo is private or gated: set HF_TOKEN to a token with access"
            };
            return Err(DownloadAttemptError::Fatal(format!(
                "Download failed with status {status}: {auth}"
            )));
        }
        let msg = format!("Download failed with status {status}");
        return if status.is_server_error() {
            Err(DownloadAttemptError::Retryable(msg))
        } else {
            Err(DownloadAttemptError::Fatal(msg))
        };
    }

    if existing_bytes > 0 && status.as_u16() != 206 {
        let _ = fs::remove_file(dest_path);
        return Err(DownloadAttemptError::Retryable(format!(
            "Server ignored resume request for partial file ({existing_bytes} bytes, status {status}); restarting from zero"
        )));
    }

    let resumed = existing_bytes > 0 && status.as_u16() == 206;
    let (mut file, mut bytes_downloaded) = if resumed {
        let file = fs::OpenOptions::new()
            .append(true)
            .open(dest_path)
            .map_err(|e| DownloadAttemptError::Fatal(format!("Failed to open tmp file: {e}")))?;
        let content_len = response.content_length().unwrap_or(0);
        let total = if content_len > 0 {
            existing_bytes + content_len
        } else {
            0
        };
        info!(
            "[HfCache] Resuming from {} bytes (total {:?})",
            existing_bytes, total
        );
        (file, existing_bytes)
    } else {
        let file = fs::File::create(dest_path)
            .map_err(|e| DownloadAttemptError::Fatal(format!("Failed to create file: {e}")))?;
        (file, 0u64)
    };

    let mut reader = response;
    let mut buf = [0u8; 65536];

    loop {
        let n = match reader.read(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                drop(file);
                let msg = format!("Read error during download: {e}");
                return if is_retryable_io_error(&e) {
                    Err(DownloadAttemptError::Retryable(msg))
                } else {
                    Err(DownloadAttemptError::Fatal(msg))
                };
            }
        };
        if n == 0 {
            break;
        }
        if let Err(e) = file.write_all(&buf[..n]) {
            drop(file);
            return Err(DownloadAttemptError::Fatal(format!(
                "Write error during download: {e}"
            )));
        }
        bytes_downloaded += n as u64;
        if max_bytes.is_some_and(|max| bytes_downloaded > max) {
            drop(file);
            return Err(DownloadAttemptError::Fatal(format!(
                "the download is larger than the {} bytes expected",
                max_bytes.unwrap_or_default()
            )));
        }
    }

    drop(file);

    info!(
        "[HfCache] Download complete: {} bytes -> {:?}",
        bytes_downloaded, dest_path
    );
    Ok(bytes_downloaded)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn test_repo_to_folder_name() {
        assert_eq!(
            repo_to_folder_name("unsloth/Qwen3.5-2B-GGUF"),
            "models--unsloth--Qwen3.5-2B-GGUF"
        );
    }

    #[test]
    fn the_cache_dir_comes_from_hf_vars_else_home() {
        let home = Path::new("home");
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };
        assert_eq!(
            hf_cache_dir_from(home, vars(&[])).unwrap(),
            home.join(".cache").join("huggingface").join("hub")
        );
        assert_eq!(
            hf_cache_dir_from(home, vars(&[("HF_HOME", "hf")])).unwrap(),
            Path::new("hf").join("hub")
        );
        assert_eq!(
            hf_cache_dir_from(home, vars(&[("HF_HUB_CACHE", "hub"), ("HF_HOME", "hf")])).unwrap(),
            Path::new("hub")
        );
        assert_eq!(
            hf_cache_dir_from(home, vars(&[("HF_HUB_CACHE", "")])).unwrap_err(),
            "HF_HUB_CACHE is set but empty"
        );
        assert_eq!(
            hf_cache_dir_from(home, vars(&[("HF_HOME", "")])).unwrap_err(),
            "HF_HOME is set but empty"
        );
    }

    #[test]
    fn the_token_is_hf_token_else_the_token_file() {
        let home = unique_test_dir("token");
        let none = |_: &str| None;
        assert_eq!(hf_token_from(&home, none).unwrap(), None);
        let file = home.join(".cache").join("huggingface");
        fs::create_dir_all(&file).unwrap();
        fs::write(file.join("token"), "hf_file\n").unwrap();
        assert_eq!(
            hf_token_from(&home, none).unwrap().as_deref(),
            Some("hf_file")
        );
        let env = |name: &str| (name == "HF_TOKEN").then(|| OsString::from("hf_env"));
        assert_eq!(
            hf_token_from(&home, env).unwrap().as_deref(),
            Some("hf_env")
        );
        let empty = |name: &str| (name == "HF_TOKEN").then(|| OsString::from(" "));
        let error = hf_token_from(&home, empty).unwrap_err();
        assert!(error.contains("HF_TOKEN"), "{error}");
        let _ = fs::remove_dir_all(home);
    }

    #[test]
    fn the_hub_address_is_hf_endpoint_else_huggingface() {
        assert_eq!(hf_endpoint_from(None).unwrap(), "https://huggingface.co");
        assert_eq!(
            hf_endpoint_from(Some("http://127.0.0.1:9/".into())).unwrap(),
            "http://127.0.0.1:9"
        );
        for bad in ["", "huggingface.co", "ftp://x", "https://", "https:///x"] {
            let error = hf_endpoint_from(Some(bad.into())).unwrap_err();
            assert!(error.contains("HF_ENDPOINT"), "{bad}: {error}");
        }
    }

    #[test]
    fn a_non_unicode_hf_var_is_an_error_naming_it() {
        #[cfg(unix)]
        let bad = {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![0xff])
        };
        #[cfg(windows)]
        let bad = {
            use std::os::windows::ffi::OsStringExt;
            OsString::from_wide(&[0xd800])
        };
        let error = hf_cache_dir_from(Path::new("home"), |name| {
            (name == "HF_HOME").then(|| bad.clone())
        })
        .unwrap_err();
        assert_eq!(error, "HF_HOME is not valid unicode");
    }

    #[test]
    fn test_resumed_download_restarts_from_zero_when_range_is_ignored() {
        let test_dir = unique_test_dir("range-ignored");
        fs::create_dir_all(&test_dir).unwrap();
        let dest_path = test_dir.join("model.gguf.tmp");
        fs::write(&dest_path, b"hello").unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/model.gguf", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);

        let server = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            let mut handled = 0;
            while handled < 2 && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream.set_nonblocking(false).unwrap();
                        let request = read_http_request(&mut stream);
                        server_requests.lock().unwrap().push(request);
                        let body: &[u8] = if handled == 0 {
                            b"world"
                        } else {
                            b"hello world"
                        };
                        write_http_response(&mut stream, body);
                        handled += 1;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("test server accept failed: {e}"),
                }
            }
        });

        let downloaded = download_file_to_path(&url, None, None, &dest_path).unwrap();

        server.join().unwrap();
        let recorded = requests.lock().unwrap();
        assert_eq!(recorded.len(), 2);
        assert!(recorded[0].to_ascii_lowercase().contains("range: bytes=5-"));
        assert!(!recorded[1].to_ascii_lowercase().contains("range:"));
        assert_eq!(downloaded, 11);
        assert_eq!(fs::read(&dest_path).unwrap(), b"hello world");

        let _ = fs::remove_dir_all(test_dir);
    }

    #[test]
    fn snapshot_symlink_target_uses_native_separators() {
        let native = |parts: &[&str]| parts.join(std::path::MAIN_SEPARATOR_STR);
        assert_eq!(
            snapshot_symlink_target("tokenizer.json", "sha256-ab")
                .to_str()
                .unwrap(),
            native(&["..", "..", "blobs", "sha256-ab"])
        );
        assert_eq!(
            snapshot_symlink_target("onnx/model.onnx", "sha256-ab")
                .to_str()
                .unwrap(),
            native(&["..", "..", "..", "blobs", "sha256-ab"])
        );
    }

    #[test]
    fn test_hf_cache_download_writes_the_blob_ref_and_snapshot() {
        let test_dir = unique_test_dir("cache-download");
        let cache_dir = test_dir.join("hub");
        let body = b"tokenizer bytes";
        let expected = sha256_hex(body);

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            write_http_response(&mut stream, body);
        });

        let path = download_to_hf_cache_at(
            &cache_dir,
            &base_url,
            Download::default(),
            "org/repo",
            "tokenizer.json",
            "abc123",
        )
        .unwrap();

        server.join().unwrap();
        assert_eq!(
            path,
            cache_dir
                .join(repo_to_folder_name("org/repo"))
                .join("snapshots")
                .join("abc123")
                .join("tokenizer.json")
        );
        assert_eq!(fs::read(&path).unwrap(), body);
        assert_eq!(
            fs::read_to_string(
                cache_dir
                    .join(repo_to_folder_name("org/repo"))
                    .join("refs")
                    .join("main")
            )
            .unwrap(),
            "abc123"
        );
        assert!(
            cache_dir
                .join(repo_to_folder_name("org/repo"))
                .join("blobs")
                .join(format!("sha256-{expected}"))
                .is_file()
        );

        let _ = fs::remove_dir_all(test_dir);
    }

    fn unique_test_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "rayline-hf-{}-{}-{}",
            name,
            std::process::id(),
            nanos
        ))
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn read_http_request(stream: &mut std::net::TcpStream) -> String {
        let mut request = String::new();
        let mut reader = std::io::BufReader::new(stream);
        loop {
            let mut line = String::new();
            let bytes = reader.read_line(&mut line).unwrap();
            if bytes == 0 || line == "\r\n" {
                break;
            }
            request.push_str(&line);
        }
        request
    }

    fn write_http_response(stream: &mut std::net::TcpStream, body: &[u8]) {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
    }

    #[test]
    fn a_token_goes_only_over_https_or_to_loopback() {
        assert!(may_carry_token("https://huggingface.co"));
        assert!(may_carry_token("http://127.0.0.1:8080"));
        assert!(may_carry_token("http://localhost:8080"));
        assert!(may_carry_token("http://[::1]:8080"));
        assert!(!may_carry_token("http://hub.example.com"));
        assert!(!may_carry_token("http://10.0.0.1"));
        let cache_dir = std::env::temp_dir().join(format!("hf-plain-{}", std::process::id()));
        let error = download_to_hf_cache_at(
            &cache_dir,
            "http://hub.example.com",
            Download {
                token: Some("hf_test"),
                ..Download::default()
            },
            "org/repo",
            "tokenizer.json",
            "abc123",
        )
        .unwrap_err();
        assert!(error.contains("plain http"), "{error}");
        assert!(!error.contains("hf_test"), "{error}");
        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn hf_endpoint_refuses_credentials_without_echoing_them() {
        for value in [
            "https://user:secret@hub.example",
            "http://secret@127.0.0.1:8080/x",
        ] {
            let error = hf_endpoint_from(Some(value.into())).unwrap_err();
            assert!(error.contains("user name or password"), "{error}");
            assert!(!error.contains("secret"), "{error}");
        }
        for value in ["https://hub.example/path?a=b@c", "https://hub.example/#x"] {
            let error = hf_endpoint_from(Some(value.into())).unwrap_err();
            assert!(error.contains("query or fragment"), "{error}");
        }
        assert_eq!(
            hf_endpoint_from(Some("https://hub.example/mirror/".into())).unwrap(),
            "https://hub.example/mirror"
        );
        let error = hf_endpoint_from(Some("ftp://secret".into())).unwrap_err();
        assert!(!error.contains("secret"), "{error}");
    }

    #[test]
    fn a_token_never_follows_a_redirect_to_plain_http() {
        let cache_dir = std::env::temp_dir().join(format!("hf-redirect-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache_dir);
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let _ = read_http_request(&mut stream);
            let _ = stream.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://hub.example.com/f.bin\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        });
        let error = download_to_hf_cache_at(
            &cache_dir,
            &base_url,
            Download {
                token: Some("hf_test"),
                ..Download::default()
            },
            "org/repo",
            "f.bin",
            "abc123",
        )
        .unwrap_err();
        server.join().unwrap();
        assert!(error.contains("plain http"), "{error}");
        assert!(!error.contains("hf_test"), "{error}");
        let _ = fs::remove_dir_all(&cache_dir);
    }

    #[test]
    fn a_pinned_download_past_its_size_is_refused_and_leaves_refs_main() {
        let cache_dir = std::env::temp_dir().join(format!("hf-max-{}", std::process::id()));
        let _ = fs::remove_dir_all(&cache_dir);
        let serve = |body: &'static [u8]| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let _ = read_http_request(&mut stream);
                write_http_response(&mut stream, body);
            });
            (base_url, server)
        };
        let pinned = |max_bytes| Download {
            max_bytes: Some(max_bytes),
            pinned: true,
            ..Download::default()
        };
        let (base_url, server) = serve(b"0123456789");
        let error = download_to_hf_cache_at(
            &cache_dir,
            &base_url,
            pinned(4),
            "org/repo",
            "f.bin",
            "abc123",
        )
        .unwrap_err();
        server.join().unwrap();
        assert!(
            error.contains("larger than the 4 bytes expected"),
            "{error}"
        );

        let (base_url, server) = serve(b"0123456789");
        download_to_hf_cache_at(
            &cache_dir,
            &base_url,
            pinned(10),
            "org/repo",
            "f.bin",
            "abc123",
        )
        .unwrap();
        server.join().unwrap();
        assert!(!cache_dir.join("models--org--repo/refs/main").exists());
        let _ = fs::remove_dir_all(&cache_dir);
    }
}
