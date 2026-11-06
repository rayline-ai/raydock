//! Process-lifetime ownership of a subscription pool.
//!
//! A pool runtime refreshes rotating OAuth grants. Two processes refreshing the
//! same grants invalidate each other's refresh tokens, so every process that
//! runs a refreshing [`crate::SubscriptionPoolRuntime`] must hold the pool's
//! [`PoolLock`] for as long as the runtime lives. Read-only status polls do not
//! refresh and need no lock.
//!
//! The lock is an advisory `flock` on a named file: anything that deletes the
//! lock file or its directory while an owner runs lets a second owner in, so
//! cleanup must check [`recorded_holder_pid`] and the lock before removing it.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};

use fs2::FileExt as _;
use thiserror::Error;

use crate::config::{ConfigError, validate_identifier};

/// State directory, relative to the home directory, shared by every binary
/// that can own a pool. Owners must agree on it for the lock to exclude them.
pub const DEFAULT_POOL_STATE_DIR: &str = ".rayline/subscriptions";

/// An exclusive advisory lock on one pool, released when dropped or when the
/// process exits.
#[derive(Debug)]
pub struct PoolLock {
    _file: File,
    path: PathBuf,
}

impl PoolLock {
    /// Default state directory under `home`.
    pub fn default_state_dir(home: &Path) -> PathBuf {
        home.join(DEFAULT_POOL_STATE_DIR)
    }

    /// Lock file path for `pool_id` under `state_dir`.
    pub fn path_for(state_dir: &Path, pool_id: &str) -> Result<PathBuf, PoolLockError> {
        validate_identifier("pool", pool_id).map_err(PoolLockError::InvalidPoolId)?;
        Ok(state_dir.join(format!("pool-{pool_id}.lock")))
    }

    /// Takes the pool lock without waiting. Fails with [`PoolLockError::Held`]
    /// when another process owns the pool.
    pub fn try_acquire(state_dir: &Path, pool_id: &str) -> Result<Self, PoolLockError> {
        let path = Self::path_for(state_dir, pool_id)?;
        // A relative directory would depend on the working directory, whose
        // own ancestors are never checked.
        if !state_dir.is_absolute() {
            return Err(PoolLockError::UnsafeStateDir(state_dir.to_owned()));
        }
        create_private_dir(state_dir)?;
        let mut file = open_lock_file(&path)?;
        match file.try_lock_exclusive() {
            Ok(()) => {}
            Err(source) if source.kind() == fs2::lock_contended_error().kind() => {
                return Err(PoolLockError::Held {
                    holder_pid: read_holder_pid(&mut file),
                    path,
                });
            }
            Err(source) => return Err(PoolLockError::Lock { path, source }),
        }
        // A file unlinked or replaced between open and lock is not the pool's
        // lock any more; owning it would not exclude the next owner.
        if !still_names_open_file(&file, &path) {
            return Err(PoolLockError::Replaced(path));
        }
        let pid = std::process::id();
        file.set_len(0)
            .and_then(|()| file.seek(SeekFrom::Start(0)))
            .and_then(|_| writeln!(file, "{pid}"))
            .and_then(|()| file.flush())
            .map_err(|source| PoolLockError::Lock {
                path: path.clone(),
                source,
            })?;
        Ok(Self { _file: file, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The pid recorded by the current or last owner, if it is readable. A stale
/// value is possible after a crash, so callers must only use it for messages.
pub fn recorded_holder_pid(state_dir: &Path, pool_id: &str) -> Option<u32> {
    let path = PoolLock::path_for(state_dir, pool_id).ok()?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let mut file = options.open(&path).ok()?;
    read_holder_pid(&mut file)
}

#[derive(Debug, Error)]
pub enum PoolLockError {
    #[error(transparent)]
    InvalidPoolId(ConfigError),
    #[error("create subscription pool state directory {path:?}: {source}")]
    StateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error(
        "subscription pool state directory {0:?} is not a private directory owned by this user"
    )]
    UnsafeStateDir(PathBuf),
    #[error("open subscription pool lock {path:?}: {source}")]
    Open {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("subscription pool lock {0:?} is not a regular file owned by this user")]
    UnsafeLockFile(PathBuf),
    #[error("subscription pool lock {0:?} was replaced while it was being taken")]
    Replaced(PathBuf),
    #[error("{}", held_message(path, *holder_pid))]
    Held {
        path: PathBuf,
        holder_pid: Option<u32>,
    },
    #[error("lock subscription pool {path:?}: {source}")]
    Lock {
        path: PathBuf,
        source: std::io::Error,
    },
}

fn held_message(path: &Path, holder_pid: Option<u32>) -> String {
    match holder_pid {
        Some(pid) => format!("subscription pool is already owned by pid {pid} (lock {path:?})"),
        None => format!("subscription pool is already owned by another process (lock {path:?})"),
    }
}

fn create_private_dir(dir: &Path) -> Result<(), PoolLockError> {
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        validate_trusted_ancestors(parent, Missing::Allowed)?;
    }
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|source| PoolLockError::StateDir {
            path: dir.to_owned(),
            source,
        })?;
    // Components missing at the first check were unverified: another user
    // could have created one (in a sticky shared directory) before we did.
    // Every component must now exist and be trusted; one missing here was
    // moved away, which only its foreign owner could do.
    if let Some(parent) = dir.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        validate_trusted_ancestors(parent, Missing::Refused)?;
    }
    let metadata = fs::symlink_metadata(dir).map_err(|source| PoolLockError::StateDir {
        path: dir.to_owned(),
        source,
    })?;
    if !metadata.is_dir() {
        return Err(PoolLockError::UnsafeStateDir(dir.to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        // SAFETY: geteuid has no arguments or memory safety preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(PoolLockError::UnsafeStateDir(dir.to_owned()));
        }
        if metadata.mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700)).map_err(|source| {
                PoolLockError::StateDir {
                    path: dir.to_owned(),
                    source,
                }
            })?;
        }
    }
    Ok(())
}

/// Another user able to rename an entry anywhere above the state directory
/// could swap it under a running owner. The path is therefore resolved one
/// component at a time, the way the kernel does, following every symlink
/// (including links inside link targets): each directory passed through must
/// be ours or root's and not writable by others unless it is sticky, and each
/// symlink must be owned by us or root (its holder is a directory already
/// checked). Missing components below are created by us, privately; `..`
/// among them is refused because it would step back into unchecked ground.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Missing {
    /// Before creation: the rest of the path will be created by us.
    Allowed,
    /// After creation: a missing component means the path was tampered with.
    Refused,
}

fn validate_trusted_ancestors(parent: &Path, missing: Missing) -> Result<(), PoolLockError> {
    #[cfg(unix)]
    {
        use std::collections::VecDeque;
        use std::ffi::OsString;
        use std::os::unix::fs::MetadataExt as _;
        use std::path::{Component, PathBuf};

        const MAX_SYMLINK_HOPS: usize = 40;
        // SAFETY: geteuid has no arguments or memory safety preconditions.
        let euid = unsafe { libc::geteuid() };
        let state_error = |path: &Path, source| PoolLockError::StateDir {
            path: path.to_owned(),
            source,
        };
        let owner_trusted = |metadata: &fs::Metadata| metadata.uid() == euid || metadata.uid() == 0;
        let trusted_dir = |metadata: &fs::Metadata| {
            let shared_writable = metadata.mode() & 0o022 != 0 && metadata.mode() & 0o1000 == 0;
            owner_trusted(metadata) && !shared_writable
        };
        // Pending components; `None` marks `..`.
        let split = |path: &Path| -> VecDeque<Option<OsString>> {
            path.components()
                .filter_map(|component| match component {
                    Component::Normal(name) => Some(Some(name.to_owned())),
                    Component::ParentDir => Some(None),
                    Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
                })
                .collect()
        };

        let root = Path::new("/");
        let metadata = fs::metadata(root).map_err(|source| state_error(root, source))?;
        if !trusted_dir(&metadata) {
            return Err(PoolLockError::UnsafeStateDir(root.to_owned()));
        }
        let mut resolved = PathBuf::from(root);
        let mut pending = split(parent);
        let mut hops = 0;
        while let Some(component) = pending.pop_front() {
            let Some(name) = component else {
                // `..` of a checked directory is its checked parent.
                resolved.pop();
                continue;
            };
            let next = resolved.join(&name);
            let metadata = match fs::symlink_metadata(&next) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if missing == Missing::Refused || pending.iter().any(Option::is_none) {
                        return Err(PoolLockError::UnsafeStateDir(parent.to_owned()));
                    }
                    break;
                }
                Err(source) => return Err(state_error(&next, source)),
            };
            if metadata.file_type().is_symlink() {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS || !owner_trusted(&metadata) {
                    return Err(PoolLockError::UnsafeStateDir(next));
                }
                let target = fs::read_link(&next).map_err(|source| state_error(&next, source))?;
                if target.is_absolute() {
                    resolved = PathBuf::from(root);
                }
                for component in split(&target).into_iter().rev() {
                    pending.push_front(component);
                }
                continue;
            }
            if !trusted_dir(&metadata) {
                return Err(PoolLockError::UnsafeStateDir(next));
            }
            resolved = next;
        }
    }
    #[cfg(not(unix))]
    let _ = (parent, missing);
    Ok(())
}

fn still_names_open_file(file: &File, path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        let (Ok(open), Ok(named)) = (file.metadata(), fs::symlink_metadata(path)) else {
            return false;
        };
        open.dev() == named.dev() && open.ino() == named.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        true
    }
}

fn open_lock_file(path: &Path) -> Result<File, PoolLockError> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path).map_err(|source| PoolLockError::Open {
        path: path.to_owned(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| PoolLockError::Open {
        path: path.to_owned(),
        source,
    })?;
    if !metadata.is_file() {
        return Err(PoolLockError::UnsafeLockFile(path.to_owned()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        // SAFETY: geteuid has no arguments or memory safety preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.nlink() != 1 {
            return Err(PoolLockError::UnsafeLockFile(path.to_owned()));
        }
    }
    Ok(file)
}

fn read_holder_pid(file: &mut File) -> Option<u32> {
    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).ok()?;
    file.take(32).read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_owner_is_refused_with_the_holder_pid() {
        let temp = tempfile::tempdir().expect("tempdir");
        let state = temp.path().join("state");
        let first = PoolLock::try_acquire(&state, "default").expect("first owner");
        assert_eq!(first.path(), state.join("pool-default.lock"));

        let second = PoolLock::try_acquire(&state, "default");
        assert!(matches!(
            second,
            Err(PoolLockError::Held { holder_pid: Some(pid), .. }) if pid == std::process::id()
        ));
        assert_eq!(
            recorded_holder_pid(&state, "default"),
            Some(std::process::id())
        );
    }

    #[test]
    fn pools_lock_independently_and_release_on_drop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let default = PoolLock::try_acquire(temp.path(), "default").expect("default");
        let _other = PoolLock::try_acquire(temp.path(), "other").expect("other pool");
        drop(default);
        PoolLock::try_acquire(temp.path(), "default").expect("reacquire after drop");
    }

    #[test]
    fn rejects_pool_ids_that_could_escape_the_state_dir() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            PoolLock::try_acquire(temp.path(), "../default"),
            Err(PoolLockError::InvalidPoolId(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlinked_lock_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("elsewhere");
        fs::write(&target, b"").expect("target");
        std::os::unix::fs::symlink(&target, temp.path().join("pool-default.lock"))
            .expect("symlink");
        assert!(matches!(
            PoolLock::try_acquire(temp.path(), "default"),
            Err(PoolLockError::Open { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_symlink_held_by_a_shared_writable_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().expect("tempdir");
        let trusted = temp.path().join("trusted");
        fs::create_dir(&trusted).expect("trusted dir");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).expect("shared dir");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        // The link resolves somewhere trusted, but anyone could re-point it.
        std::os::unix::fs::symlink(&trusted, shared.join("link")).expect("symlink");
        assert!(matches!(
            PoolLock::try_acquire(&shared.join("link/state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
        // A link held by a private directory is fine.
        std::os::unix::fs::symlink(&trusted, temp.path().join("private-link")).expect("symlink");
        PoolLock::try_acquire(&temp.path().join("private-link/state"), "default")
            .expect("link in a private directory");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_nested_symlink_held_by_a_shared_writable_directory() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().expect("tempdir");
        let trusted = temp.path().join("trusted");
        fs::create_dir(&trusted).expect("trusted dir");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).expect("shared dir");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        // alias (private) -> shared/hop (re-pointable by anyone) -> trusted
        std::os::unix::fs::symlink(&trusted, shared.join("hop")).expect("hop");
        std::os::unix::fs::symlink(shared.join("hop"), temp.path().join("alias")).expect("alias");
        assert!(matches!(
            PoolLock::try_acquire(&temp.path().join("alias/state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn the_post_create_check_refuses_a_missing_ancestor() {
        let temp = tempfile::tempdir().expect("tempdir");
        let temp = temp.path().canonicalize().expect("physical temp");
        // Moved away after creation: only its (foreign) owner could do that.
        assert!(matches!(
            validate_trusted_ancestors(&temp.join("moved"), Missing::Refused),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
        validate_trusted_ancestors(&temp.join("moved"), Missing::Allowed).expect("not yet created");
        validate_trusted_ancestors(&temp, Missing::Refused).expect("present");
    }

    #[test]
    fn refuses_parent_steps_below_a_missing_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(matches!(
            PoolLock::try_acquire(&temp.path().join("missing/../state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
        // `..` through directories that exist resolves like the kernel does.
        fs::create_dir(temp.path().join("present")).expect("present");
        PoolLock::try_acquire(&temp.path().join("present/../state"), "default")
            .expect("existing parent step");
    }

    #[cfg(unix)]
    #[test]
    fn refuses_a_state_dir_under_a_shared_writable_parent() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().expect("tempdir");
        let shared = temp.path().join("shared");
        fs::create_dir(&shared).expect("shared dir");
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o777)).expect("chmod");
        assert!(matches!(
            PoolLock::try_acquire(&shared.join("state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
        // Also when the unsafe directory is a grandparent and the parent is
        // still missing, or is private but movable through the grandparent.
        assert!(matches!(
            PoolLock::try_acquire(&shared.join("missing/state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
        let private = shared.join("private");
        fs::create_dir(&private).expect("private dir");
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).expect("chmod");
        assert!(matches!(
            PoolLock::try_acquire(&private.join("state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o1777)).expect("sticky");
        PoolLock::try_acquire(&shared.join("state"), "default").expect("sticky parent is safe");
    }

    #[test]
    fn refuses_a_relative_state_dir() {
        assert!(matches!(
            PoolLock::try_acquire(Path::new("relative/state"), "default"),
            Err(PoolLockError::UnsafeStateDir(_))
        ));
    }

    #[test]
    fn reading_the_holder_pid_does_not_create_the_lock() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert_eq!(recorded_holder_pid(temp.path(), "default"), None);
        assert!(!temp.path().join("pool-default.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn tightens_a_group_readable_state_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        let temp = tempfile::tempdir().expect("tempdir");
        let state = temp.path().join("state");
        fs::create_dir(&state).expect("state dir");
        fs::set_permissions(&state, fs::Permissions::from_mode(0o755)).expect("chmod");
        let _lock = PoolLock::try_acquire(&state, "default").expect("lock");
        let mode = fs::metadata(&state).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
