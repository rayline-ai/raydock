//! The ARC runtime release local decisions load: downloaded from a pinned
//! Hugging Face repo and commit into the Hugging Face cache, and checked
//! file by file against its `manifest.json`, whose own sha256 is pinned.

use std::fs;
use std::path::{Path, PathBuf};

use crate::config::ArcRelease;

const MANIFEST_FILE: &str = "manifest.json";
/// The largest `manifest.json` read or downloaded.
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

/// The built-in release: the public ARC repo at a pinned commit.
///
/// PLACEHOLDER: the runtime release is not public on Hugging Face yet.
/// Until it is, `revision` and `manifest_sha256` are all zeros and local
/// decisions refuse to start without `routing.arc_release`. Fill in the
/// commit, the sha256 of its `manifest.json`, and the directory it is in.
pub fn built_in() -> ArcRelease {
    ArcRelease {
        repo: "rayline-ai/ARC-1.0".to_owned(),
        revision: PLACEHOLDER_REVISION.to_owned(),
        manifest_sha256: PLACEHOLDER_MANIFEST_SHA256.to_owned(),
        dir: Some("runtime".to_owned()),
    }
}
const PLACEHOLDER_REVISION: &str = "0000000000000000000000000000000000000000";
const PLACEHOLDER_MANIFEST_SHA256: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The release's directory in the cache at `hub`, with every file its
/// manifest lists present and matching. Files already cached at the pinned
/// commit are not downloaded again, so a complete cache works offline. A
/// cached file that does not match is downloaded once more; a downloaded
/// file that does not match is removed and refused. `token` authorizes a
/// private repo.
pub fn fetch(
    hub: &Path,
    hub_url: &str,
    token: Option<&str>,
    release: &ArcRelease,
) -> Result<PathBuf, String> {
    if release.revision == PLACEHOLDER_REVISION {
        return Err(format!(
            "the ARC runtime release is not published on {} yet: set `routing.arc_release`",
            release.repo
        ));
    }
    let in_release = |name: &str| match &release.dir {
        Some(dir) => format!("{dir}/{name}"),
        None => name.to_owned(),
    };
    let pinned = |path: String, max_bytes: u64, sha256: &str| Pinned {
        repo: &release.repo,
        revision: &release.revision,
        path,
        max_bytes,
        sha256: sha256.to_owned(),
    };
    let manifest = pinned(
        in_release(MANIFEST_FILE),
        MAX_MANIFEST_BYTES,
        &release.manifest_sha256,
    );
    let at = fetch_file(hub, hub_url, token, &manifest)?;
    let files = fs::read(&at)
        .map_err(|e| e.to_string())
        .and_then(|raw| rayline_arc_decide::manifest_files(&raw).map_err(|e| e.to_string()))
        .map_err(|e| format!("{} of {}: {e}", manifest.path, release.repo))?;
    for (name, want) in &files {
        fetch_file(
            hub,
            hub_url,
            token,
            &pinned(in_release(name), want.bytes, &want.sha256),
        )?;
    }
    Ok(rayline_hf::hf_cache_snapshot_path(
        hub,
        &release.repo,
        release.dir.as_deref().unwrap_or_default(),
        &release.revision,
    ))
}

/// One file pinned by repo, commit, path and sha256, at most `max_bytes`
/// long.
pub struct Pinned<'a> {
    pub repo: &'a str,
    pub revision: &'a str,
    pub path: String,
    pub max_bytes: u64,
    pub sha256: String,
}

/// The pinned file's path in the cache at `hub`: the cached copy when it
/// matches (hashed on every call: a blob's name doesn't prove its bytes),
/// else downloaded. A copy that doesn't match is removed; a download that
/// doesn't match is refused.
pub fn fetch_file(
    hub: &Path,
    hub_url: &str,
    token: Option<&str>,
    file: &Pinned<'_>,
) -> Result<PathBuf, String> {
    let (repo, revision, path) = (file.repo, file.revision, file.path.as_str());
    let matches = |at: &Path| {
        fs::metadata(at).is_ok_and(|meta| meta.len() <= file.max_bytes)
            && file_sha256(at).is_some_and(|sha| sha == file.sha256)
    };
    if let Some(cached) = rayline_hf::cached_hf_file(hub, repo, path, revision) {
        if matches(&cached) {
            return Ok(cached);
        }
        discard(&cached);
    }
    let downloaded = rayline_hf::download_to_hf_cache_at(
        hub,
        hub_url,
        rayline_hf::Download {
            token,
            max_bytes: Some(file.max_bytes),
            pinned: true,
        },
        repo,
        path,
        revision,
    )
    .map_err(|e| format!("cannot download {path} from {repo} at {revision}: {e}"))?;
    if matches(&downloaded) {
        return Ok(downloaded);
    }
    discard(&downloaded);
    Err(format!(
        "{path} from {repo} at {revision} is not the pinned file; it was removed from the cache"
    ))
}

/// The SHA-256 of the file at `path`, read in chunks.
fn file_sha256(path: &Path) -> Option<String> {
    use sha2::Digest as _;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut fs::File::open(path).ok()?, &mut hasher).ok()?;
    Some(format!("{:x}", hasher.finalize()))
}

/// Removes a snapshot file. The blob it links to is removed too when it is
/// corrupt (it doesn't hash to its name) and in the same repo's `blobs`
/// directory: a sound blob may be another revision's, and a link anywhere
/// else is only unlinked.
fn discard(snapshot: &Path) {
    let blobs = snapshot
        .ancestors()
        .find(|dir| dir.file_name().is_some_and(|name| name == "snapshots"))
        .and_then(Path::parent)
        .and_then(|repo| fs::canonicalize(repo.join("blobs")).ok());
    if let (Ok(blob), Some(blobs)) = (fs::canonicalize(snapshot), blobs) {
        let named = blob
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("sha256-"));
        let corrupt = blob.parent() == Some(blobs.as_path())
            && named.is_some_and(|sha| file_sha256(&blob).is_none_or(|actual| actual != sha));
        if corrupt {
            let _ = fs::remove_file(blob);
        }
    }
    let _ = fs::remove_file(snapshot);
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;
    use std::collections::BTreeMap;

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", sha2::Sha256::digest(bytes))
    }
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    const REVISION: &str = "1111111111111111111111111111111111111111";
    const REPO: &str = "test-org/arc";
    const RELEASE_DIR: &str = "runtime";
    const TOKEN: &str = "hf_test_token";

    /// A fake Hub serving `files` (path under the repo -> bytes) at
    /// `REVISION`, and 404 for anything else. Records each path it serves.
    /// A private one answers 401 to a request without `TOKEN`.
    struct FakeHub {
        url: String,
        served: Arc<Mutex<Vec<String>>>,
    }

    fn fake_hub(files: BTreeMap<String, Vec<u8>>) -> FakeHub {
        serve(files, false)
    }

    fn serve(files: BTreeMap<String, Vec<u8>>, private: bool) -> FakeHub {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let served = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&served);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().expect("clone"));
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut authorized = !private;
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap_or_default();
                    if name.eq_ignore_ascii_case("authorization")
                        && value.trim() == format!("Bearer {TOKEN}")
                    {
                        authorized = true;
                    }
                }
                let target = line.split_whitespace().nth(1).unwrap_or_default();
                let prefix = format!("/{REPO}/resolve/{REVISION}/");
                let body = target
                    .strip_prefix(&prefix)
                    .and_then(|path| files.get(path).map(|body| (path, body)));
                let response = match body {
                    _ if !authorized => {
                        b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                    Some((path, body)) => {
                        log.lock().expect("lock").push(path.to_owned());
                        let mut head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        )
                        .into_bytes();
                        head.extend_from_slice(body);
                        head
                    }
                    None => {
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                            .to_vec()
                    }
                };
                let _ = stream.write_all(&response);
            }
        });
        FakeHub { url, served }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rayline-arc-release-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    /// A release's files under `runtime/`, its manifest, and the pin.
    fn release(files: &[(&str, &[u8])]) -> (BTreeMap<String, Vec<u8>>, ArcRelease) {
        release_in(Some(RELEASE_DIR), files)
    }

    fn release_in(
        dir: Option<&str>,
        files: &[(&str, &[u8])],
    ) -> (BTreeMap<String, Vec<u8>>, ArcRelease) {
        let at = |name: &str| match dir {
            Some(dir) => format!("{dir}/{name}"),
            None => name.to_owned(),
        };
        let listed: BTreeMap<&str, serde_json::Value> = files
            .iter()
            .map(|(name, body)| {
                (
                    *name,
                    serde_json::json!({"sha256": sha256_hex(body), "bytes": body.len()}),
                )
            })
            .collect();
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schema": "rayline.arc-runtime-manifest.v1", "files": listed,
        }))
        .expect("manifest");
        let pin = ArcRelease {
            repo: REPO.to_owned(),
            revision: REVISION.to_owned(),
            manifest_sha256: sha256_hex(&manifest),
            dir: dir.map(str::to_owned),
        };
        let mut served: BTreeMap<String, Vec<u8>> = files
            .iter()
            .map(|(name, body)| (at(name), body.to_vec()))
            .collect();
        served.insert(at(MANIFEST_FILE), manifest);
        (served, pin)
    }

    const FILES: [(&str, &[u8]); 3] = [
        ("arc-runtime.json", b"{\"runtime\": 1}"),
        ("heads.safetensors", b"heads"),
        ("prices.json", b"{\"models\": {}}"),
    ];

    #[test]
    fn a_pinned_release_downloads_once_and_then_loads_offline() {
        let hub = scratch("good");
        let (served, pin) = release(&FILES);
        let fake = fake_hub(served);
        let dir = fetch(&hub, &fake.url, None, &pin).expect("fetches");
        for (name, body) in FILES {
            assert_eq!(fs::read(dir.join(name)).expect("cached"), body);
        }
        assert_eq!(fake.served.lock().expect("lock").len(), FILES.len() + 1);

        // Offline: nothing listens at this address, and the cache suffices.
        let offline = TcpListener::bind("127.0.0.1:0").expect("bind");
        let offline_url = format!("http://{}", offline.local_addr().expect("addr"));
        drop(offline);
        assert_eq!(fetch(&hub, &offline_url, None, &pin).expect("cached"), dir);
        let _ = fs::remove_dir_all(hub);
    }

    #[cfg(unix)]
    #[test]
    fn discarding_a_snapshot_removes_only_a_corrupt_blob_of_its_own_repo() {
        let root = scratch("discard");
        let repo = root.join("models--test-org--arc");
        let snapshot_dir = repo.join("snapshots").join(REVISION).join(RELEASE_DIR);
        fs::create_dir_all(&snapshot_dir).unwrap();
        fs::create_dir_all(repo.join("blobs")).unwrap();
        let outside = root.join("unrelated.txt");
        fs::write(&outside, b"keep me").unwrap();
        let link = snapshot_dir.join("manifest.json");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        discard(&link);
        assert!(!link.exists() && fs::symlink_metadata(&link).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep me");

        // A sound blob (it hashes to its name) may be another revision's.
        let sound = repo
            .join("blobs")
            .join(format!("sha256-{}", sha256_hex(b"good")));
        fs::write(&sound, b"good").unwrap();
        std::os::unix::fs::symlink(&sound, &link).unwrap();
        discard(&link);
        assert!(sound.exists() && fs::symlink_metadata(&link).is_err());

        // A corrupt one is removed.
        let corrupt = repo
            .join("blobs")
            .join(format!("sha256-{}", sha256_hex(b"good")));
        fs::write(&corrupt, b"bad").unwrap();
        std::os::unix::fs::symlink(&corrupt, &link).unwrap();
        discard(&link);
        assert!(!corrupt.exists());
    }

    #[test]
    fn a_tampered_file_or_manifest_is_refused_and_not_kept() {
        let hub = scratch("tampered");
        let (mut served, pin) = release(&FILES);
        served.insert(
            format!("{RELEASE_DIR}/heads.safetensors"),
            b"other".to_vec(),
        );
        let fake = fake_hub(served.clone());
        let error = fetch(&hub, &fake.url, None, &pin).expect_err("refused");
        assert!(error.contains("runtime/heads.safetensors"), "{error}");
        assert!(error.contains("not the pinned file"), "{error}");
        let path = format!("{RELEASE_DIR}/heads.safetensors");
        assert!(rayline_hf::cached_hf_file(&hub, REPO, &path, REVISION).is_none());

        // A manifest that is not the pinned one is refused before any file.
        let mut other = pin.clone();
        other.manifest_sha256 = "f".repeat(64);
        let error = fetch(&hub, &fake.url, None, &other).expect_err("refused");
        assert!(error.contains("runtime/manifest.json"), "{error}");
        let _ = fs::remove_dir_all(hub);
    }

    #[test]
    fn a_cached_file_that_changed_is_downloaded_again() {
        let hub = scratch("repair");
        let (served, pin) = release(&FILES);
        let fake = fake_hub(served);
        let dir = fetch(&hub, &fake.url, None, &pin).expect("fetches");
        let heads = fs::canonicalize(dir.join("heads.safetensors")).expect("blob");
        fs::write(&heads, b"changed").expect("tamper");
        fetch(&hub, &fake.url, None, &pin).expect("repaired");
        assert_eq!(
            fs::read(dir.join("heads.safetensors")).expect("read"),
            b"heads"
        );
        let _ = fs::remove_dir_all(hub);
    }

    #[test]
    fn a_missing_file_names_it() {
        let hub = scratch("missing");
        let (mut served, pin) = release(&FILES);
        served.remove(&format!("{RELEASE_DIR}/prices.json"));
        let fake = fake_hub(served);
        let error = fetch(&hub, &fake.url, None, &pin).expect_err("missing");
        assert!(
            error.contains("cannot download runtime/prices.json"),
            "{error}"
        );
        assert!(error.contains("404"), "{error}");
        let _ = fs::remove_dir_all(hub);
    }

    #[test]
    fn a_manifest_path_outside_the_release_is_refused() {
        let hub = scratch("outside");
        let (served, pin) = release(&[("../escape", b"x")]);
        let fake = fake_hub(served);
        let error = fetch(&hub, &fake.url, None, &pin).expect_err("refused");
        assert!(error.contains("outside the release"), "{error}");
        let _ = fs::remove_dir_all(hub);
    }

    #[test]
    fn a_private_repo_needs_the_token_and_names_hf_token_without_it() {
        let hub = scratch("private");
        let (served, pin) = release_in(None, &FILES);
        let fake = serve(served, true);
        let error = fetch(&hub, &fake.url, None, &pin).expect_err("401");
        assert!(error.contains("401"), "{error}");
        assert!(error.contains("set HF_TOKEN"), "{error}");
        assert!(fake.served.lock().expect("lock").is_empty());

        let dir = fetch(&hub, &fake.url, Some(TOKEN), &pin).expect("with the token");
        // A release at the repo root is the snapshot itself.
        assert!(dir.ends_with(REVISION));
        for (name, body) in FILES {
            assert_eq!(fs::read(dir.join(name)).expect("cached"), body);
        }

        let error =
            fetch(&scratch("private-wrong"), &fake.url, Some("hf_other"), &pin).expect_err("401");
        assert!(error.contains("has no access"), "{error}");
        assert!(!error.contains("hf_other"), "{error}");
        let _ = fs::remove_dir_all(hub);
    }

    #[test]
    fn the_placeholder_pin_is_refused_by_name() {
        let error = fetch(
            Path::new("/nonexistent"),
            "http://127.0.0.1:9",
            None,
            &built_in(),
        )
        .expect_err("placeholder");
        assert!(error.contains("routing.arc_release"), "{error}");
    }
}
