//! Private, guarded replacement of omp state during the hosted ARC cutover.

use std::ffi::OsString;
#[cfg(any(not(unix), test))]
use std::fs;
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

/// Only the hosted launcher calls this; hybrid owns a different provider contract.
pub(super) fn migrate(config_dir: &Path, args: &[OsString]) -> io::Result<()> {
    let paths = session_paths(args, &std::env::current_dir()?);
    super::omp_roles::migrate(config_dir)?;
    super::omp_sessions::migrate(config_dir, &paths)
}

fn session_paths(args: &[OsString], launch_cwd: &Path) -> Vec<PathBuf> {
    let mut cwd = None;
    let mut session_dir = None;
    let mut resume = None;
    let mut index = 0;
    while index < args.len() {
        let Some(text) = args[index].to_str() else {
            index += 1;
            continue;
        };
        if text == "--" {
            break;
        }
        let (flag, inline) = text.split_once('=').map_or((text, None), |(flag, value)| {
            (flag, Some(OsString::from(value)))
        });
        if matches!(
            flag,
            "--cwd" | "--session-dir" | "--resume" | "--restore" | "-r"
        ) {
            let value = inline.or_else(|| {
                args.get(index + 1)
                    .filter(|value| !value.as_encoded_bytes().starts_with(b"-"))
                    .map(|value| {
                        index += 1;
                        value.clone()
                    })
            });
            if let Some(value) = value {
                match flag {
                    "--cwd" => cwd = Some(PathBuf::from(value)),
                    "--session-dir" => session_dir = Some(PathBuf::from(value)),
                    _ => resume = Some(PathBuf::from(value)),
                }
            }
        }
        index += 1;
    }
    let cwd = cwd.map_or_else(|| launch_cwd.to_owned(), |path| launch_cwd.join(path));
    let mut paths = Vec::new();
    if let Some(path) = session_dir {
        paths.push(cwd.join(path));
    }
    if let Some(path) = resume {
        let candidate = cwd.join(&path);
        // A session ID/prefix is resolved inside the selected session directory
        // by omp, not a filename in the launch directory.
        if path.is_absolute()
            || path.components().count() > 1
            || path
                .extension()
                .is_some_and(|extension| extension == "jsonl")
            || candidate.is_file()
        {
            paths.push(candidate);
        }
    }
    paths
}

pub(super) struct Snapshot {
    pub file: File,
    metadata: Metadata,
}

impl Snapshot {
    pub(super) fn preserve_modified_time(&self, replacement: &File) -> io::Result<()> {
        replacement.set_modified(self.metadata.modified()?)
    }
}

#[cfg(not(unix))]
pub(super) fn open(path: &Path) -> io::Result<Option<Snapshot>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
        // Open the reparse point itself, never its symlink/junction target.
        options.custom_flags(0x0020_0000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x0400 != 0 {
            return Err(io::Error::other("omp migration refuses a reparse point"));
        }
    }
    if !metadata.is_file() || !same_snapshot(&metadata, &fs::symlink_metadata(path)?) {
        return Err(io::Error::other(
            "omp migration requires an unchanged regular file",
        ));
    }
    Ok(Some(Snapshot { file, metadata }))
}

#[cfg(not(unix))]
pub(super) fn replace(
    path: &Path,
    snapshot: &Snapshot,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("omp state has no parent directory"))?;
    let tmp_path = parent.join(format!(
        ".omp-migrate-{}-{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp_path)?;
    let mut cleanup = TemporaryFile(Some(tmp_path));
    write(&mut file)?;
    file.sync_all()?;
    drop(file);
    if !same_snapshot(&snapshot.metadata, &snapshot.file.metadata()?)
        || !same_snapshot(&snapshot.metadata, &fs::symlink_metadata(path)?)
    {
        return Err(io::Error::other(
            "omp state changed during migration; close other omp sessions and retry",
        ));
    }
    let tmp = cleanup
        .0
        .as_ref()
        .ok_or_else(|| io::Error::other("missing migration temporary file"))?;
    fs::rename(tmp, path)?;
    cleanup.0 = None;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

fn same_snapshot(before: &Metadata, after: &Metadata) -> bool {
    if !after.is_file()
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        before.dev() == after.dev()
            && before.ino() == after.ino()
            && before.ctime() == after.ctime()
            && before.ctime_nsec() == after.ctime_nsec()
    }
    #[cfg(not(unix))]
    {
        before.created().ok() == after.created().ok()
    }
}

#[cfg(not(unix))]
struct TemporaryFile(Option<PathBuf>);

#[cfg(not(unix))]
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(unix)]
fn leaf_name(name: &std::ffi::OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt as _;
    if Path::new(name).file_name() != Some(name) {
        return Err(io::Error::other("omp migration requires a leaf filename"));
    }
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| io::Error::other("omp migration filename contains a NUL byte"))
}

#[cfg(unix)]
pub(super) fn open_at(dir: &File, name: &std::ffi::OsStr) -> io::Result<Option<Snapshot>> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let name = leaf_name(name)?;
    // SAFETY: dir owns a live directory descriptor and name is NUL-terminated.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: openat returned a new owned descriptor, closed exactly once by File.
    let file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(io::Error::other("omp migration requires a regular file"));
    }
    Ok(Some(Snapshot { file, metadata }))
}

#[cfg(unix)]
pub(super) fn open(path: &Path) -> io::Result<Option<Snapshot>> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("omp state has no parent directory"))?;
    let dir = match File::open(parent) {
        Ok(dir) => dir,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    open_at(
        &dir,
        path.file_name()
            .ok_or_else(|| io::Error::other("missing omp state filename"))?,
    )
}

#[cfg(unix)]
pub(super) fn replace(
    path: &Path,
    snapshot: &Snapshot,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("omp state has no parent directory"))?;
    let dir = File::open(parent)?;
    replace_at(
        &dir,
        path.file_name()
            .ok_or_else(|| io::Error::other("missing omp state filename"))?,
        snapshot,
        write,
    )
}

#[cfg(unix)]
pub(super) fn replace_at(
    dir: &File,
    name: &std::ffi::OsStr,
    snapshot: &Snapshot,
    write: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    let destination = leaf_name(name)?;
    let temporary = leaf_name(std::ffi::OsStr::new(&format!(
        ".omp-migrate-{}-{:016x}.tmp",
        std::process::id(),
        rand::random::<u64>()
    )))?;
    // SAFETY: dir owns a directory descriptor; temporary is NUL-terminated.
    // O_EXCL prevents replacing any existing file, including a symlink.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temporary.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    let mut output = unsafe { File::from_raw_fd(fd) };
    let mut cleanup = TemporaryAt {
        dir,
        name: temporary,
        armed: true,
    };
    write(&mut output)?;
    output.sync_all()?;
    drop(output);
    let current = open_at(dir, name)?;
    if !same_snapshot(&snapshot.metadata, &snapshot.file.metadata()?)
        || !current
            .as_ref()
            .is_some_and(|current| same_snapshot(&snapshot.metadata, &current.metadata))
    {
        return Err(io::Error::other(
            "omp state changed during migration; close other omp sessions and retry",
        ));
    }
    // SAFETY: both filenames are NUL-terminated leaf names relative to the same
    // still-open directory, so directory renames cannot redirect this operation.
    if unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            cleanup.name.as_ptr(),
            dir.as_raw_fd(),
            destination.as_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    cleanup.armed = false;
    dir.sync_all()
}

#[cfg(unix)]
struct TemporaryAt<'a> {
    dir: &'a File,
    name: std::ffi::CString,
    armed: bool,
}

#[cfg(unix)]
impl Drop for TemporaryAt<'_> {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd as _;
        if self.armed {
            // SAFETY: dir and the NUL-terminated temporary leaf name outlive
            // this cleanup; unlinkat cannot follow a replaced symlink.
            unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn forwarded_session_locations_respect_cwd_and_end_of_options() {
        let args = [
            "--session-dir",
            "old",
            "--cwd=/work",
            "--session-dir=history",
            "--resume",
            "saved.jsonl",
            "--",
            "--session-dir",
            "prompt-text",
        ]
        .map(OsString::from);
        assert_eq!(
            session_paths(&args, Path::new("/launch")),
            [
                PathBuf::from("/work/history"),
                PathBuf::from("/work/saved.jsonl")
            ]
        );
        assert!(
            session_paths(&[OsString::from("--resume=abc123")], Path::new("/launch")).is_empty()
        );
    }

    #[test]
    fn a_concurrent_edit_is_not_overwritten() {
        let dir = super::super::tests::temp_dir("omp-migration-race");
        let path = dir.join("config.yml");
        fs::write(&path, "old").unwrap();
        let snapshot = open(&path).unwrap().unwrap();
        let result = replace(&path, &snapshot, |output| {
            output.write_all(b"new")?;
            fs::write(&path, "user edit")
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "user edit");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn directory_swaps_cannot_redirect_replacement() {
        use std::ffi::OsStr;
        use std::os::unix::fs::symlink;

        let root = super::super::tests::temp_dir("omp-migration-directory");
        let original = root.join("original");
        let moved = root.join("moved");
        let other = root.join("other");
        fs::create_dir(&original).unwrap();
        fs::create_dir(&other).unwrap();
        fs::write(original.join("state"), "legacy").unwrap();
        fs::write(other.join("state"), "unrelated").unwrap();
        let dir = File::open(&original).unwrap();
        let snapshot = open_at(&dir, OsStr::new("state")).unwrap().unwrap();
        fs::rename(&original, &moved).unwrap();
        symlink(&other, &original).unwrap();
        replace_at(&dir, OsStr::new("state"), &snapshot, |out| {
            out.write_all(b"arc")
        })
        .unwrap();
        assert_eq!(fs::read_to_string(moved.join("state")).unwrap(), "arc");
        assert_eq!(
            fs::read_to_string(other.join("state")).unwrap(),
            "unrelated"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_cannot_redirect_migration() {
        use std::os::unix::fs::symlink;

        let dir = super::super::tests::temp_dir("omp-migration-symlink");
        let target = dir.join("target");
        let path = dir.join("config.yml");
        fs::write(&target, "untouched").unwrap();
        symlink(&target, &path).unwrap();
        assert!(open(&path).is_err());
        assert_eq!(fs::read_to_string(&target).unwrap(), "untouched");
    }
}
