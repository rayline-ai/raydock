//! Private restart snapshots of authoritative restrictions, never usage or credentials.
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::limits::ClaimScope;
use crate::routing_state::RoutingBlock;

const SCHEMA: u32 = 1;
pub const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const MAX_ACCOUNTS: usize = 512;
const MAX_BLOCKS_PER_ACCOUNT: usize = 64;
const SNAPSHOT: &str = "routing-blocks.json";
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug)]
pub struct StoreAccount {
    pub account_id: String,
    pub source_identity: String,
}

#[derive(Clone, Debug)]
pub struct AccountBlocks {
    pub account_id: String,
    pub source_identity: String,
    pub blocks: Vec<RoutingBlock>,
}

#[derive(Clone, Debug)]
pub struct RestoredBlocks {
    pub account_id: String,
    pub blocks: Vec<RoutingBlock>,
}

/// Errors deliberately omit paths, source identities, and JSON parser excerpts.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("routing snapshot filesystem operation failed")]
    Io,
    #[error("routing snapshot directory or file is not private and safe")]
    UnsafeFilesystem,
    #[error("routing snapshot exceeds its size or record limit")]
    TooLarge,
    #[error("routing snapshot is corrupt")]
    Corrupt,
    #[error("routing snapshot schema or configuration is incompatible")]
    Incompatible,
    #[cfg(not(unix))]
    #[error("routing snapshot storage requires a Unix filesystem")]
    Unsupported,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    schema: u32,
    pool: String,
    accounts: Vec<StoredAccount>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredAccount {
    account: String,
    source: String,
    blocks: Vec<RoutingBlock>,
}

#[derive(Serialize)]
struct SavedSnapshot<'a> {
    schema: u32,
    pool: &'a str,
    accounts: Vec<SavedAccount<'a>>,
}

#[derive(Serialize)]
struct SavedAccount<'a> {
    account: String,
    source: String,
    blocks: &'a [RoutingBlock],
}

/// Open once and retain the directory descriptor: later path replacement cannot
/// redirect reads, temporary files, renames, or fsync to another directory.
#[derive(Debug)]
pub struct BlockStore {
    directory: File,
    pool: String,
}

impl BlockStore {
    pub fn new(state_dir: PathBuf, pool_identity: &str) -> Result<Self, StoreError> {
        #[cfg(unix)]
        {
            let directory = open_directory(&state_dir)?;
            validate_metadata(&directory, true)?;
            Ok(Self {
                directory,
                pool: fingerprint("pool", pool_identity),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (state_dir, pool_identity);
            Err(StoreError::Unsupported)
        }
    }

    /// Absence is an empty snapshot. Any other failure is explicit; callers must
    /// retain unknown capacity and reconcile instead of assuming availability.
    pub fn load(
        &self,
        accounts: &[StoreAccount],
        _now_ms: u64,
    ) -> Result<Vec<RestoredBlocks>, StoreError> {
        if accounts.len() > MAX_ACCOUNTS {
            return Err(StoreError::TooLarge);
        }
        #[cfg(unix)]
        {
            validate_metadata(&self.directory, true)?;
            let mut file = match open_at(
                &self.directory,
                SNAPSHOT,
                libc::O_RDONLY | libc::O_NONBLOCK,
                0,
            ) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(_) => return Err(StoreError::UnsafeFilesystem),
            };
            validate_metadata(&file, false)?;
            if file.metadata().map_err(|_| StoreError::Io)?.len() > MAX_SNAPSHOT_BYTES as u64 {
                return Err(StoreError::TooLarge);
            }
            let mut bytes = Vec::new();
            Read::by_ref(&mut file)
                .take(MAX_SNAPSHOT_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| StoreError::Io)?;
            if bytes.len() > MAX_SNAPSHOT_BYTES {
                return Err(StoreError::TooLarge);
            }
            let mut snapshot: Snapshot =
                serde_json::from_slice(&bytes).map_err(|_| StoreError::Corrupt)?;
            if snapshot.schema != SCHEMA || snapshot.pool != self.pool {
                return Err(StoreError::Incompatible);
            }
            validate_snapshot(&snapshot)?;
            let mut restored = Vec::new();
            for current in accounts {
                let id = fingerprint("account", &current.account_id);
                let source = fingerprint("source", &current.source_identity);
                if let Some(index) = snapshot
                    .accounts
                    .iter()
                    .position(|stored| stored.account == id && stored.source == source)
                {
                    // Deadlines transition restrictions into a controlled probe,
                    // not permission to serve. State owns expiry/generation rules.
                    restored.push(RestoredBlocks {
                        account_id: current.account_id.clone(),
                        blocks: snapshot.accounts.swap_remove(index).blocks,
                    });
                }
            }
            Ok(restored)
        }
        #[cfg(not(unix))]
        {
            Err(StoreError::Unsupported)
        }
    }

    pub fn save(&self, accounts: &[AccountBlocks]) -> Result<(), StoreError> {
        if accounts.len() > MAX_ACCOUNTS {
            return Err(StoreError::TooLarge);
        }
        if accounts
            .iter()
            .any(|account| account.blocks.len() > MAX_BLOCKS_PER_ACCOUNT)
        {
            return Err(StoreError::TooLarge);
        }
        let mut seen = HashSet::new();
        for account in accounts {
            if !seen.insert(&account.account_id) {
                return Err(StoreError::Corrupt);
            }
            validate_blocks(&account.blocks)?;
        }
        let snapshot = SavedSnapshot {
            schema: SCHEMA,
            pool: &self.pool,
            accounts: accounts
                .iter()
                .map(|account| SavedAccount {
                    account: fingerprint("account", &account.account_id),
                    source: fingerprint("source", &account.source_identity),
                    blocks: &account.blocks,
                })
                .collect(),
        };
        let mut buffer = BoundedBuffer(Vec::new());
        serde_json::to_writer(&mut buffer, &snapshot).map_err(|_| StoreError::TooLarge)?;
        #[cfg(unix)]
        {
            validate_metadata(&self.directory, true)?;
            match open_at(
                &self.directory,
                SNAPSHOT,
                libc::O_RDONLY | libc::O_NONBLOCK,
                0,
            ) {
                Ok(file) => validate_metadata(&file, false)?,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(StoreError::UnsafeFilesystem),
            }
            let temporary = format!(
                ".routing-blocks-{}-{}.tmp",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
            );
            let mut file = open_at(
                &self.directory,
                &temporary,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
            .map_err(|_| StoreError::Io)?;
            let result = (|| {
                file.write_all(&buffer.0).map_err(|_| StoreError::Io)?;
                file.sync_all().map_err(|_| StoreError::Io)?;
                rename_at(&self.directory, &temporary, SNAPSHOT).map_err(|_| StoreError::Io)?;
                self.directory.sync_all().map_err(|_| StoreError::Io)
            })();
            if result.is_err() {
                unlink_at(&self.directory, &temporary);
            }
            result
        }
        #[cfg(not(unix))]
        {
            Err(StoreError::Unsupported)
        }
    }
}

fn fingerprint(domain: &str, value: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"rayline-routing-blocks-v1\0");
    hash.update(domain.as_bytes());
    hash.update(b"\0");
    hash.update(value.as_bytes());
    format!("{:x}", hash.finalize())
}

fn validate_snapshot(snapshot: &Snapshot) -> Result<(), StoreError> {
    if snapshot.accounts.len() > MAX_ACCOUNTS {
        return Err(StoreError::TooLarge);
    }
    let valid_hash = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    let mut seen = HashSet::new();
    for account in &snapshot.accounts {
        if !valid_hash(&account.account)
            || !valid_hash(&account.source)
            || !seen.insert(&account.account)
        {
            return Err(StoreError::Corrupt);
        }
        validate_blocks(&account.blocks)?;
    }
    Ok(())
}

fn validate_blocks(blocks: &[RoutingBlock]) -> Result<(), StoreError> {
    if blocks.len() > MAX_BLOCKS_PER_ACCOUNT {
        return Err(StoreError::TooLarge);
    }
    for block in blocks {
        // Never persist provider-controlled free text.
        if let ClaimScope::Model(model) = &block.scope {
            let hashed = model
                .as_str()
                .strip_prefix("sha256_")
                .is_some_and(|digest| {
                    digest.len() == 64
                        && digest
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                });
            if !hashed && !matches!(model.as_str(), "fable" | "opus" | "sonnet" | "haiku") {
                return Err(StoreError::Corrupt);
            }
        }
        if matches!(block.scope, ClaimScope::Surface(_))
            || block.claim_key.as_deref().is_some_and(|key| {
                !matches!(
                    key,
                    "five_hour"
                        | "seven_day"
                        | "seven_day_overage_included"
                        | "seven_day_opus"
                        | "seven_day_sonnet"
                )
            })
        {
            return Err(StoreError::Corrupt);
        }
    }
    Ok(())
}

struct BoundedBuffer(Vec<u8>);
impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_SNAPSHOT_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("snapshot size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
fn validate_metadata(file: &File, directory: bool) -> Result<(), StoreError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|_| StoreError::Io)?;
    // SAFETY: geteuid has no arguments and no memory-safety preconditions.
    let uid = unsafe { libc::geteuid() };
    if metadata.uid() != uid
        || metadata.mode() & 0o7777 != if directory { 0o700 } else { 0o600 }
        || if directory {
            !metadata.is_dir()
        } else {
            !metadata.is_file() || metadata.nlink() != 1
        }
    {
        return Err(StoreError::UnsafeFilesystem);
    }
    Ok(())
}

#[cfg(unix)]
fn open_at(directory: &File, name: &str, flags: i32, mode: libc::c_uint) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name =
        std::ffi::CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: valid directory fd and NUL-terminated name; successful fd is owned exactly once.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn rename_at(directory: &File, old: &str, new: &str) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let old =
        std::ffi::CString::new(old).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    let new =
        std::ffi::CString::new(new).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: both names are NUL terminated and directory remains open.
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            old.as_ptr(),
            directory.as_raw_fd(),
            new.as_ptr(),
        )
    } < 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn unlink_at(directory: &File, name: &str) {
    use std::os::fd::AsRawFd;
    if let Ok(name) = std::ffi::CString::new(name) {
        // SAFETY: NUL-terminated name and live directory descriptor.
        unsafe {
            libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0);
        }
    }
}

#[cfg(unix)]
fn open_directory(path: &std::path::Path) -> Result<File, StoreError> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Component;
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map_err(|_| StoreError::Io)?
            .join(path)
    };
    let mut directory = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")
        .map_err(|_| StoreError::Io)?;
    for component in path.components() {
        let component = match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(component) => component,
            _ => return Err(StoreError::UnsafeFilesystem),
        };
        let name = std::ffi::CString::new(component.as_bytes())
            .map_err(|_| StoreError::UnsafeFilesystem)?;
        // SAFETY: valid fd and NUL-terminated name; no-follow applies to every component.
        let mut fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0,
            )
        };
        if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            // SAFETY: valid fd and name; newly created directories are private.
            if unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) } < 0
                && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
            {
                return Err(StoreError::Io);
            }
            directory.sync_all().map_err(|_| StoreError::Io)?;
            // SAFETY: as above; race winners still must be real directories, not links.
            fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    0,
                )
            };
        }
        if fd < 0 {
            return Err(StoreError::UnsafeFilesystem);
        }
        // SAFETY: successful openat returns a fresh owned descriptor.
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(all(test, unix))]
#[path = "persistence_tests.rs"]
mod tests;

#[cfg(all(test, unix))]
#[path = "persistence_security_tests.rs"]
mod security_tests;
