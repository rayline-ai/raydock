//! Downloads files from the Hugging Face Hub into the Hugging Face cache.
//!
//! The on-disk layout (models--org--repo/{refs,blobs,snapshots/{commit}}) is
//! byte-identical to what huggingface_hub creates, so Rayline can reuse an
//! existing Hugging Face cache.

use serde::Deserialize;
use sha2::{Digest, Sha256};
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

/// Resolve the HF cache directory. Priority: `HF_HUB_CACHE`, `HF_HOME`/hub,
/// then `~/.cache/huggingface/hub` (or `%LOCALAPPDATA%\huggingface\hub` on
/// Windows).
pub fn hf_cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("HF_HUB_CACHE") {
        return PathBuf::from(dir);
    }
    if let Ok(home) = std::env::var("HF_HOME") {
        return PathBuf::from(home).join("hub");
    }
    let cache =
        dirs::cache_dir().unwrap_or_else(|| dirs::home_dir().expect("no home dir").join(".cache"));
    cache.join("huggingface").join("hub")
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
    let url = format!("https://huggingface.co/api/models/{repo}");
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

pub fn hf_cache_snapshot_path(repo: &str, filename: &str, commit: &str) -> PathBuf {
    hf_cache_dir()
        .join(repo_to_folder_name(repo))
        .join("snapshots")
        .join(commit)
        .join(filename)
}

/// The pinned snapshot file, when it is in the cache.
pub fn cached_hf_file(repo: &str, filename: &str, commit: &str) -> Option<PathBuf> {
    let snapshot = hf_cache_snapshot_path(repo, filename, commit);
    snapshot.is_file().then_some(snapshot)
}

// ---------------------------------------------------------------------------
// Download to HF cache
// ---------------------------------------------------------------------------

/// Download a file into the HF cache structure. Returns the snapshot symlink
/// path. Steps mirror `huggingface_hub`: write to `blobs/.download-*`,
/// SHA256-rename to `blobs/sha256-{hash}`, write `refs/main`, symlink
/// `snapshots/{commit}/{filename}` → blob.
pub fn download_to_hf_cache(repo: &str, filename: &str, commit: &str) -> Result<PathBuf, String> {
    download_to_hf_cache_at(
        &hf_cache_dir(),
        "https://huggingface.co",
        repo,
        filename,
        commit,
    )
}

fn download_to_hf_cache_at(
    cache: &Path,
    hf_base_url: &str,
    repo: &str,
    filename: &str,
    commit: &str,
) -> Result<PathBuf, String> {
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

    download_file_to_path(&download_url, &tmp_path)?;

    info!("[HfCache] Computing SHA256 for {:?}", tmp_path);
    let sha256 = compute_sha256(&tmp_path)?;
    let blob_name = sha256_blob_name(&sha256);
    let blob_path = blobs_dir.join(&blob_name);

    if blob_path.exists() {
        let _ = fs::remove_file(&tmp_path);
        info!("[HfCache] Blob already exists: {:?}", blob_path);
    } else {
        fs::rename(&tmp_path, &blob_path).map_err(|e| format!("Failed to rename blob: {e}"))?;
        info!("[HfCache] Blob created: {:?}", blob_path);
    }

    let _ = fs::remove_file(&tmp_url_path);

    let refs_main = refs_dir.join("main");
    let mut f =
        fs::File::create(&refs_main).map_err(|e| format!("Failed to create refs/main: {e}"))?;
    f.write_all(commit.as_bytes())
        .map_err(|e| format!("Failed to write refs/main: {e}"))?;

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

fn download_file_to_path(url: &str, dest_path: &Path) -> Result<u64, String> {
    info!("[HfCache] Downloading {} -> {:?}", url, dest_path);

    if let Some(parent) = dest_path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }

    let client = reqwest::blocking::Client::builder()
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
        match attempt_download(url, dest_path, &client) {
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
    dest_path: &Path,
    client: &reqwest::blocking::Client,
) -> Result<u64, DownloadAttemptError> {
    // Honor any partial file on disk — either retained from a previous
    // process invocation (via the .tmp.url sidecar guarding the call site)
    // or left by a previous retry within this same invocation.
    let existing_bytes = fs::metadata(dest_path).map(|m| m.len()).unwrap_or(0);

    let mut request = client.get(url).header("User-Agent", "rayline");
    if existing_bytes > 0 {
        request = request.header("Range", format!("bytes={existing_bytes}-"));
    }

    let response = match request.send() {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Download request failed: {e}");
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

        let downloaded = download_file_to_path(&url, &dest_path).unwrap();

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
}
