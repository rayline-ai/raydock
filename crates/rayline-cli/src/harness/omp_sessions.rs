//! Hosted-only migration of persisted selections, never historical served models.
use super::{LaunchModels, omp_migration};
use serde_json::{Value, json};
#[cfg(any(windows, test))]
use std::fs::OpenOptions;
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, Seek, Write},
    path::{Path, PathBuf},
};

const LEGACY: &str = "rayline/rayline-router";

fn invalid(message: &str) -> io::Error {
    io::Error::other(format!(
        "Cannot migrate omp session: {message}; close any running omp session and repair or restore the journal before retrying"
    ))
}

/// `extra_paths` are explicit CLI session files or session directories. A
/// Legacy selection moves to the key's listed ARC Sage (`smol`: ARC Swift).
pub(super) fn migrate(
    config_dir: &Path,
    extra_paths: &[PathBuf],
    models: &LaunchModels,
) -> io::Result<()> {
    #[cfg(unix)]
    {
        if let Some(root) = managed_root(config_dir)? {
            visit(&directory(&root)?, &root, models)?;
        }
        for path in extra_paths {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.is_dir() {
                let root = fs::canonicalize(path)?;
                visit(&directory(&root)?, &root, models)?;
                continue;
            }
            let parent = path
                .parent()
                .ok_or_else(|| invalid("session path has no parent"))?;
            let parent = fs::canonicalize(parent)?;
            let dir = directory(&parent)?;
            let name = path
                .file_name()
                .ok_or_else(|| invalid("session path has no filename"))?;
            migrate_file(&parent.join(name), &parent.join(name), &dir, models)?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        if let Some(root) = managed_root(config_dir)? {
            visit_windows(&WindowsDirectory::open(&root)?, models)?;
        }
        for path in extra_paths {
            let metadata = match fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                visit_windows(&WindowsDirectory::open(&fs::canonicalize(path)?)?, models)?;
            } else {
                let parent = fs::canonicalize(
                    path.parent()
                        .ok_or_else(|| invalid("session path has no parent"))?,
                )?;
                let guard = WindowsDirectory::open(&parent)?;
                let leaf = guard.path.join(
                    path.file_name()
                        .ok_or_else(|| invalid("session path has no filename"))?,
                );
                migrate_file(&leaf, &leaf, models)?;
            }
        }
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = (config_dir, extra_paths);
        Err(invalid(
            "this platform does not provide supported session ownership locks",
        ))
    }
}

fn managed_root(config_dir: &Path) -> io::Result<Option<PathBuf>> {
    let path = config_dir.join("sessions");
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("managed sessions root is not a plain directory"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid("managed sessions root is a reparse point"));
        }
    }
    fs::canonicalize(config_dir).map(|root| Some(root.join("sessions")))
}

// Most journals need no migration. Scan bytes before allocating a tree or
// interpreting an unrelated (possibly interrupted) journal. JSON string tokens
// are decoded only when escaped, so Unicode-escaped legacy ids are not missed.
fn candidate(file: &File) -> io::Result<bool> {
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        if raw.last() == Some(&b'\n') {
            raw.pop();
            if raw.last() == Some(&b'\r') {
                raw.pop();
            }
        }
        if raw
            .windows(b"rayline-router".len())
            .any(|s| s == b"rayline-router")
        {
            return Ok(true);
        }
        if !raw.contains(&b'\\') {
            continue;
        }
        let mut start = None;
        let mut escaped = false;
        let mut has_escape = false;
        for (index, byte) in raw.iter().copied().enumerate() {
            if start.is_none() {
                if byte == b'"' {
                    start = Some(index);
                    has_escape = false;
                }
            } else if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
                has_escape = true;
            } else if byte == b'"' {
                let begin = start.take().expect("string token was started");
                if has_escape {
                    match serde_json::from_slice::<String>(&raw[begin..=index]) {
                        Ok(text) if text.contains("rayline-router") => return Ok(true),
                        Ok(_) | Err(_) => {}
                    }
                }
            }
        }
        if let Some(begin) = start.filter(|_| has_escape) {
            let mut token = raw[begin..].to_vec();
            token.push(b'"');
            if serde_json::from_slice::<String>(&token)
                .is_ok_and(|text| text.contains("rayline-router"))
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

// Denying FILE_SHARE_DELETE on every directory component prevents a directory
// rename or junction replacement while path-based reads/replacement are active.
#[cfg(windows)]
struct WindowsDirectory {
    path: PathBuf,
    _ancestors: Vec<File>,
}

#[cfg(windows)]
impl WindowsDirectory {
    fn open(path: &Path) -> io::Result<Self> {
        use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
        use std::path::Component;
        let path = std::path::absolute(path)?;
        let mut current = PathBuf::new();
        let mut handles = Vec::new();
        for component in path.components() {
            current.push(component.as_os_str());
            if !matches!(component, Component::Normal(_)) {
                continue;
            }
            let file = OpenOptions::new()
                .read(true)
                .share_mode(3)
                .custom_flags(0x0200_0000 | 0x0020_0000) // BACKUP_SEMANTICS | OPEN_REPARSE_POINT
                .open(&current)?;
            let metadata = file.metadata()?;
            if !metadata.is_dir() || metadata.file_attributes() & 0x400 != 0 {
                return Err(invalid("session directory is not a plain directory"));
            }
            handles.push(file);
        }
        Ok(Self {
            path,
            _ancestors: handles,
        })
    }
}

#[cfg(windows)]
fn visit_windows(dir: &WindowsDirectory, models: &LaunchModels) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    for entry in fs::read_dir(&dir.path)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(invalid(
                "reparse point encountered in the sessions directory",
            ));
        }
        if metadata.is_dir() {
            let mut journal_name = entry.file_name();
            journal_name.push(".jsonl");
            if fs::symlink_metadata(dir.path.join(journal_name)).is_ok() {
                continue;
            }
            visit_windows(&WindowsDirectory::open(&entry.path())?, models)?;
        } else if metadata.is_file() && entry.path().extension().is_some_and(|ext| ext == "jsonl") {
            migrate_file(&entry.path(), &entry.path(), models)?;
        }
    }
    Ok(())
}

// Walk each component with O_NOFOLLOW. Keeping directory descriptors alive also
// prevents a concurrent directory rename/symlink swap from redirecting I/O.
#[cfg(unix)]
fn directory(path: &Path) -> io::Result<File> {
    use std::path::Component;
    let absolute = std::path::absolute(path)?;
    let mut dir = File::open("/")?;
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                dir = open_relative(&dir, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            }
            _ => return Err(invalid("unsafe session directory path")),
        }
    }
    Ok(dir)
}

#[cfg(unix)]
fn component(name: &std::ffi::OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        return Err(invalid("unsafe relative filename"));
    }
    std::ffi::CString::new(bytes).map_err(|_| invalid("filename contains NUL"))
}

#[cfg(unix)]
fn open_relative(
    dir: &File,
    name: &std::ffi::OsStr,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = component(name)?;
    // SAFETY: live directory descriptor and NUL-terminated single component.
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            libc::c_uint::from(mode),
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat transferred ownership of one new descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn relative_kind(dir: &File, name: &std::ffi::OsStr) -> io::Result<libc::mode_t> {
    use std::os::fd::AsRawFd;
    let name = component(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: valid descriptor, filename and writable stat storage.
    if unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstatat initialized the structure.
    Ok(unsafe { stat.assume_init() }.st_mode & libc::S_IFMT)
}

#[cfg(unix)]
fn directory_names(dir: &File) -> io::Result<Vec<std::ffi::OsString>> {
    use std::os::{fd::AsRawFd, unix::ffi::OsStrExt};
    // SAFETY: fcntl duplicates a live descriptor, owned by fdopendir on success.
    let fd = unsafe { libc::fcntl(dir.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd is a fresh descriptor referring to the held directory.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: fdopendir failed, so we retain ownership of fd.
        unsafe { libc::close(fd) };
        return Err(error);
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: this guard uniquely owns the successful fdopendir result.
            unsafe { libc::closedir(self.0) };
        }
    }
    let stream = Stream(stream);
    let mut names = Vec::new();
    loop {
        #[cfg(target_vendor = "apple")]
        // SAFETY: obtains this thread's errno storage.
        let errno = unsafe { libc::__error() };
        #[cfg(not(target_vendor = "apple"))]
        // SAFETY: obtains this thread's errno storage.
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: thread-local errno pointer is valid.
        unsafe { *errno = 0 };
        // SAFETY: stream is live; returned storage is consumed before next read.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            // SAFETY: readdir just set this thread's errno (or left it zero).
            let code = unsafe { *errno };
            if code != 0 {
                return Err(io::Error::from_raw_os_error(code));
            }
            break;
        }
        // SAFETY: readdir returns a NUL-terminated d_name in the live entry.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsStr::from_bytes(name).to_owned());
        }
    }
    Ok(names)
}

#[cfg(unix)]
fn visit(dir: &File, actual: &Path, models: &LaunchModels) -> io::Result<()> {
    for name in directory_names(dir)? {
        let kind = relative_kind(dir, &name)?;
        if kind == libc::S_IFLNK {
            return Err(invalid("symlink encountered in the sessions directory"));
        }
        if kind == libc::S_IFDIR {
            let mut journal = name.clone();
            journal.push(".jsonl");
            match relative_kind(dir, &journal) {
                Ok(_) => continue,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            let child = open_relative(dir, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            visit(&child, &actual.join(&name), models)?;
        } else if kind == libc::S_IFREG
            && Path::new(&name)
                .extension()
                .is_some_and(|ext| ext == "jsonl")
        {
            let path = actual.join(&name);
            migrate_file(&path, &path, dir, models)?;
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
struct State {
    explicit: bool,
    legacy: bool,
}
struct Node {
    state: State,
    line: usize,
    timestamp: String,
}
struct Plan {
    session_id: String,
    replacements: HashMap<usize, Value>,
    insertions: HashMap<usize, Value>,
}

fn plan(file: &File, models: &LaunchModels) -> io::Result<Plan> {
    let sage = format!("rayline/{}", models.main);
    let swift = format!("rayline/{}", models.background);
    let mut replacements = HashMap::new();
    let mut nodes: HashMap<String, Node> = HashMap::new();
    let mut parents = HashSet::new();
    let mut header: Option<(String, u64)> = None;
    let mut old_state = State::default();
    let mut old_last = None;
    let mut target = false;
    let mut reader = BufReader::new(file);
    let mut raw = Vec::new();
    let mut next_line = 0;
    loop {
        raw.clear();
        if reader.read_until(b'\n', &mut raw)? == 0 {
            break;
        }
        let line = next_line;
        next_line += 1;
        if raw.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let mut value: Value = serde_json::from_slice(&raw)
            .map_err(|_| invalid("malformed or partial JSONL record"))?;
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid("record has no type"))?
            .to_owned();
        if kind == "title" {
            continue;
        }
        if kind == "session" {
            if header.is_some() {
                return Err(invalid("multiple session headers"));
            }
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("header has no session id"))?
                .to_owned();
            let version = value.get("version").and_then(Value::as_u64).unwrap_or(1);
            header = Some((id, version));
            continue;
        }
        let version = header
            .as_ref()
            .ok_or_else(|| invalid("missing session header"))?
            .1;
        let parent = value.get("parentId").and_then(Value::as_str);
        let mut state = if version < 2 {
            old_state.clone()
        } else if let Some(parent) = parent {
            parents.insert(parent.to_owned());
            nodes
                .get(parent)
                .ok_or_else(|| invalid("missing or out-of-order parent entry"))?
                .state
                .clone()
        } else {
            State::default()
        };
        if kind == "model_change" {
            let old_form = value.get("model").is_none()
                && value.get("provider").and_then(Value::as_str) == Some("rayline")
                && value.get("modelId").and_then(Value::as_str) == Some("rayline-router");
            let legacy = value.get("model").and_then(Value::as_str) == Some(LEGACY) || old_form;
            let role = value
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if role == "default"
                && (value.get("model").and_then(Value::as_str).is_some() || old_form)
            {
                state.explicit = true;
                state.legacy = false;
            }
            if legacy {
                target = true;
                let model = if role == "smol" { &swift } else { &sage };
                value["model"] = json!(model);
                if old_form {
                    value["modelId"] = json!(model.strip_prefix("rayline/").unwrap());
                }
                replacements.insert(line, value.clone());
            }
        } else if kind == "message"
            && value.pointer("/message/role").and_then(Value::as_str) == Some("assistant")
            && !state.explicit
        {
            state.legacy = value.pointer("/message/provider").and_then(Value::as_str)
                == Some("rayline")
                && value.pointer("/message/model").and_then(Value::as_str)
                    == Some("rayline-router");
            target |= state.legacy;
        }
        let timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();
        if version < 2 {
            old_state = state;
            old_last = Some((line, timestamp));
        } else {
            let id = value
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("entry has no tree id"))?
                .to_owned();
            if nodes
                .insert(
                    id,
                    Node {
                        state,
                        line,
                        timestamp,
                    },
                )
                .is_some()
            {
                return Err(invalid("duplicate entry id"));
            }
        }
    }
    let (session_id, version) = header.ok_or_else(|| invalid("missing session header"))?;
    let mut insertions = HashMap::new();
    if target && version > 3 {
        return Err(invalid("unsupported session version"));
    }
    if version < 2 && old_state.legacy {
        if let Some((line, timestamp)) = old_last {
            insertions.insert(line, json!({"type":"model_change", "model":sage, "role":"default", "timestamp":timestamp}));
        }
    } else {
        for (id, node) in &nodes {
            if node.state.legacy && !parents.contains(id) {
                let mut new_id = super::new_conversation_id();
                while nodes.contains_key(&new_id) {
                    new_id = super::new_conversation_id();
                }
                insertions.insert(
                    node.line,
                    json!({"type":"model_change", "id":new_id, "parentId":id,
                    "timestamp":node.timestamp, "model":sage, "role":"default"}),
                );
            }
        }
    }
    Ok(Plan {
        session_id,
        replacements,
        insertions,
    })
}

#[cfg(any(unix, windows))]
fn migrate_file(
    path: &Path,
    actual: &Path,
    #[cfg(unix)] dir: &File,
    models: &LaunchModels,
) -> io::Result<()> {
    #[cfg(unix)]
    let name = path
        .file_name()
        .ok_or_else(|| invalid("missing journal filename"))?;
    #[cfg(unix)]
    let opened = omp_migration::open_at(dir, name)?;
    #[cfg(windows)]
    let opened = omp_migration::open(path)?;
    let Some(mut source) = opened else {
        return Err(invalid("journal disappeared during migration"));
    };
    if !candidate(&source.file)? {
        return Ok(());
    }
    source.file.rewind()?;
    let plan = plan(&source.file, models)?;
    if plan.replacements.is_empty() && plan.insertions.is_empty() {
        return Ok(());
    }
    let _lease = acquire_lease(&plan.session_id)?;
    let filename = actual
        .file_name()
        .ok_or_else(|| invalid("missing journal filename"))?
        .to_str()
        .ok_or_else(|| invalid("journal filename is not UTF-8"))?;
    let gate = actual.with_file_name(format!(".{filename}.lock.os"));
    #[cfg(all(unix, not(target_os = "linux")))]
    let _gate = native_lock_at(
        dir,
        gate.file_name()
            .ok_or_else(|| invalid("invalid gate filename"))?,
    )?;
    #[cfg(any(target_os = "linux", windows))]
    let _gate = native_lock(&gate)?;
    #[cfg(unix)]
    let _publish = PublishLockAt::acquire(dir, std::ffi::OsStr::new(&format!(".{filename}.lock")))?;
    #[cfg(windows)]
    let _publish = PublishLock::acquire(path.with_file_name(format!(".{filename}.lock")))?;
    source.file.rewind()?;
    let rewrite = |out: &mut File| {
        // read_until retains exact record bytes (including whitespace and CRLF).
        let mut reader = BufReader::new(&source.file);
        let mut raw = Vec::new();
        let mut line = 0;
        loop {
            raw.clear();
            if reader.read_until(b'\n', &mut raw)? == 0 {
                break;
            }
            if let Some(value) = plan.replacements.get(&line) {
                serde_json::to_writer(&mut *out, value)?;
                out.write_all(b"\n")?;
            } else {
                out.write_all(&raw)?;
            }
            if let Some(value) = plan.insertions.get(&line) {
                if !raw.ends_with(b"\n") {
                    out.write_all(b"\n")?;
                }
                serde_json::to_writer(&mut *out, value)?;
                out.write_all(b"\n")?;
            }
            line += 1;
        }
        source.preserve_modified_time(out)
    };
    #[cfg(unix)]
    {
        omp_migration::replace_at(dir, name, &source, rewrite)
    }
    #[cfg(windows)]
    {
        omp_migration::replace(path, &source, rewrite)
    }
}

#[cfg(unix)]
struct PublishLockAt<'a> {
    dir: &'a File,
    name: std::ffi::CString,
}
#[cfg(unix)]
impl<'a> PublishLockAt<'a> {
    fn acquire(dir: &'a File, name: &std::ffi::OsStr) -> io::Result<Self> {
        let mut file = open_relative(
            dir,
            name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(|_| invalid("journal publish lock exists or cannot be created"))?;
        let guard = Self {
            dir,
            name: component(name)?,
        };
        writeln!(file, "{}:0", std::process::id())?;
        Ok(guard)
    }
}
#[cfg(unix)]
impl Drop for PublishLockAt<'_> {
    fn drop(&mut self) {
        use std::os::fd::AsRawFd;
        // SAFETY: held directory descriptor and owned NUL-terminated filename.
        unsafe { libc::unlinkat(self.dir.as_raw_fd(), self.name.as_ptr(), 0) };
    }
}

#[cfg(windows)]
struct PublishLock(PathBuf);
#[cfg(windows)]
impl PublishLock {
    fn acquire(path: PathBuf) -> io::Result<Self> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|_| invalid("journal publish lock exists or cannot be created"))?;
        let guard = Self(path);
        writeln!(file, "{}:0", std::process::id())?;
        Ok(guard)
    }
}
#[cfg(windows)]
impl Drop for PublishLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(any(unix, windows))]
fn acquire_lease(id: &str) -> io::Result<NativeLock> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(invalid(
            "session id cannot safely identify an ownership lease",
        ));
    }
    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let home = std::env::var_os(home_key)
        .ok_or_else(|| invalid("home directory is unavailable for ownership lease"))?;
    let base = std::env::var_os("PI_TEST_SESSION_OWNERS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let xdg = if cfg!(any(target_os = "linux", target_os = "macos")) {
                std::env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("omp"))
            } else {
                None
            };
            let base = xdg.filter(|p| p.exists()).unwrap_or_else(|| {
                PathBuf::from(home).join(
                    std::env::var_os("PI_CONFIG_DIR")
                        .filter(|v| !v.is_empty())
                        .unwrap_or_else(|| ".omp".into()),
                )
            });
            base.join("run/session-owners")
        });
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        // The environment-supplied ownership root may include OS aliases.
        create_private_directory(&base)?;
        use std::os::unix::fs::MetadataExt;
        let metadata = directory(&fs::canonicalize(&base)?)?.metadata()?;
        // SAFETY: geteuid has no preconditions and does not access memory.
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
            return Err(invalid(
                "ownership directory is not private and owned by this user",
            ));
        }
    }
    native_lock(&std::path::absolute(base.join(format!("{id}.lock")))?)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn create_private_directory(path: &Path) -> io::Result<()> {
    match fs::canonicalize(path) {
        Ok(root) => return directory(&root).map(|_| ()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = path
        .parent()
        .ok_or_else(|| invalid("invalid ownership directory"))?;
    create_private_directory(parent)?;
    let dir = directory(&fs::canonicalize(parent)?)?;
    let name = path
        .file_name()
        .ok_or_else(|| invalid("invalid ownership directory"))?;
    use std::os::fd::AsRawFd;
    let name = component(name)?;
    // SAFETY: held parent descriptor and a validated NUL-terminated component.
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    }
    directory(&fs::canonicalize(path)?).map(|_| ())
}

/// omp's YAML writer uses the same native lease, keyed by `<config>.lock`.
/// Hold it from before reading role state until the atomic replacement ends.
#[cfg(any(unix, windows))]
pub(super) fn lock_settings(path: &Path) -> io::Result<NativeLock> {
    let mut lock = std::path::absolute(path)?.into_os_string();
    lock.push(".lock");
    native_lock(Path::new(&lock))
}

#[cfg(target_os = "linux")]
pub(super) struct NativeLock {
    _socket: std::os::unix::net::UnixDatagram,
}
#[cfg(all(unix, not(target_os = "linux")))]
pub(super) struct NativeLock {
    _file: File,
}

#[cfg(target_os = "linux")]
fn native_lock(path: &Path) -> io::Result<NativeLock> {
    use std::os::{
        linux::net::SocketAddrExt,
        unix::net::{SocketAddr, UnixDatagram},
    };
    use xxhash_rust::xxh64::xxh64;
    let path = path
        .to_str()
        .ok_or_else(|| invalid("ownership path is not UTF-8"))?;
    let name = format!(
        "omp-file-lock-{:016x}{:016x}",
        xxh64(path.as_bytes(), 0x4f4d_502d_4c4f_434b),
        xxh64(path.as_bytes(), 0x5049_2d46_494c_454c)
    );
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let socket = UnixDatagram::bind_addr(&addr).map_err(|_| {
        invalid("another process owns the session or its publish gate is unavailable")
    })?;
    Ok(NativeLock { _socket: socket })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn native_lock(path: &Path) -> io::Result<NativeLock> {
    let parent = fs::canonicalize(path.parent().ok_or_else(|| invalid("invalid lock path"))?)?;
    let dir = directory(&parent)?;
    native_lock_at(
        &dir,
        path.file_name()
            .ok_or_else(|| invalid("invalid lock filename"))?,
    )
}

#[cfg(all(unix, not(target_os = "linux")))]
fn native_lock_at(dir: &File, name: &std::ffi::OsStr) -> io::Result<NativeLock> {
    use std::os::fd::AsRawFd;
    let file = open_relative(dir, name, libc::O_RDWR | libc::O_CREAT, 0o600)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("ownership lock is not a regular file"));
    }
    // SAFETY: the file owns the descriptor through the lifetime of the lock.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(invalid(
            "another process owns the session or its publish gate is unavailable",
        ));
    }
    Ok(NativeLock { _file: file })
}

#[cfg(windows)]
pub(super) struct NativeLock {
    _handle: std::os::windows::io::OwnedHandle,
}

#[cfg(windows)]
fn native_lock(path: &Path) -> io::Result<NativeLock> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::{
        Foundation::{ERROR_ALREADY_EXISTS, GetLastError, SetLastError},
        System::Threading::CreateMutexW,
    };
    use xxhash_rust::xxh64::xxh64;
    let original = path
        .to_str()
        .ok_or_else(|| invalid("ownership path is not UTF-8"))?;
    // canonicalize() emits verbatim paths; Node path.resolve() does not.
    let unc;
    let normalized = if let Some(rest) = original.strip_prefix(r"\\?\UNC\") {
        unc = format!(r"\\{rest}");
        unc.as_str()
    } else {
        original.strip_prefix(r"\\?\").unwrap_or(original)
    };
    let name = format!(
        r"Global\omp-file-lock-{:016x}{:016x}",
        xxh64(normalized.as_bytes(), 0x4f4d_502d_4c4f_434b),
        xxh64(normalized.as_bytes(), 0x5049_2d46_494c_454c)
    );
    let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: resets the calling thread's last-error slot.
    unsafe { SetLastError(0) };
    // SAFETY: null attributes and a live NUL-terminated UTF-16 name.
    let raw = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the successful call returned an owned handle closed once by RAII.
    let handle = unsafe { OwnedHandle::from_raw_handle(raw) };
    // SAFETY: observes CreateMutexW's thread-local status before another OS call.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Err(invalid(
            "another process owns the session or its publish gate is unavailable",
        ));
    }
    Ok(NativeLock { _handle: handle })
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;

    const SAGE: &str = "rayline/arc-sage-1.0";
    const SWIFT: &str = "rayline/arc-swift-1.0";

    fn journal(entries: &[Value]) -> (PathBuf, PathBuf) {
        let root = fs::canonicalize(super::super::tests::temp_dir("omp-sessions")).unwrap();
        let sessions = root.join("sessions/project");
        fs::create_dir_all(&sessions).unwrap();
        let path = sessions.join("history.jsonl");
        let header = json!({"type":"session", "version":3, "id":super::super::new_conversation_id(),
            "timestamp":"2026-01-01T00:00:00Z", "cwd":"/project", "parentSession":"/old/session.jsonl"});
        let mut file = File::create(&path).unwrap();
        writeln!(file, "{header}").unwrap();
        for entry in entries {
            writeln!(file, "{entry}").unwrap();
        }
        (root, path)
    }

    fn read(path: &Path) -> Vec<Value> {
        BufReader::new(File::open(path).unwrap())
            .lines()
            .map(|line| serde_json::from_str(&line.unwrap()).unwrap())
            .collect()
    }

    // Mirrors buildSessionContext's root-to-leaf setting extraction, including
    // the explicit-default guard against historical assistant inference.
    fn resumed(entries: &[Value], leaf: &str) -> HashMap<String, String> {
        let by_id: HashMap<&str, &Value> = entries
            .iter()
            .filter_map(|e| e["id"].as_str().map(|id| (id, e)))
            .collect();
        let mut path = Vec::new();
        let mut current = by_id.get(leaf).copied();
        while let Some(entry) = current {
            path.push(entry);
            current = entry["parentId"]
                .as_str()
                .and_then(|id| by_id.get(id).copied());
        }
        let mut explicit = false;
        let mut models = HashMap::new();
        for entry in path.into_iter().rev() {
            if entry["type"] == "model_change" {
                if let Some(model) = entry["model"].as_str() {
                    let role = entry["role"].as_str().unwrap_or("default");
                    models.insert(role.to_owned(), model.to_owned());
                    explicit |= role == "default";
                }
            } else if entry["type"] == "message"
                && entry["message"]["role"] == "assistant"
                && !explicit
            {
                models.insert(
                    "default".into(),
                    format!(
                        "{}/{}",
                        entry["message"]["provider"].as_str().unwrap(),
                        entry["message"]["model"].as_str().unwrap()
                    ),
                );
            }
        }
        models
    }

    fn entry(id: &str, parent: Option<&str>, kind: &str) -> Value {
        json!({"type":kind,"id":id,"parentId":parent,"timestamp":"2026-01-01T00:00:01Z"})
    }

    #[test]
    fn migration_preserves_the_mtime_order_used_by_continue() {
        use std::time::{Duration, UNIX_EPOCH};

        let mut legacy = entry("legacy", None, "model_change");
        legacy["model"] = json!(LEGACY);
        let (root, old) = journal(&[legacy]);
        let mut arc = entry("arc", None, "model_change");
        arc["model"] = json!(SAGE);
        let (other, arc_file) = journal(&[arc]);
        let recent = old.with_file_name("recent.jsonl");
        fs::copy(arc_file, &recent).unwrap();
        let old_time = UNIX_EPOCH + Duration::from_secs(20);
        let recent_time = UNIX_EPOCH + Duration::from_secs(30);
        File::open(&old).unwrap().set_modified(old_time).unwrap();
        File::open(&recent)
            .unwrap()
            .set_modified(recent_time)
            .unwrap();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        assert_eq!(fs::metadata(&old).unwrap().modified().unwrap(), old_time);
        assert_eq!(
            fs::metadata(&recent).unwrap().modified().unwrap(),
            recent_time
        );
        assert!(
            fs::metadata(&old).unwrap().modified().unwrap()
                < fs::metadata(&recent).unwrap().modified().unwrap()
        );
        fs::remove_dir_all(root).unwrap();
        fs::remove_dir_all(other).unwrap();
    }

    #[test]
    fn explicit_roles_migrate_but_served_history_and_metadata_do_not() {
        let mut entries = Vec::new();
        let mut parent = None;
        for (id, role, model) in [
            ("a", "default", LEGACY),
            ("b", "smol", LEGACY),
            ("c", "plan", LEGACY),
            ("d", "slow", SAGE),
            ("e", "custom", "other/rayline-router"),
        ] {
            let mut e = entry(id, parent, "model_change");
            e["model"] = json!(model);
            e["role"] = json!(role);
            entries.push(e);
            parent = Some(id);
        }
        let mut assistant = entry("f", Some("e"), "message");
        assistant["message"] = json!({"role":"assistant","provider":"rayline","model":"rayline-router",
            "content":[{"type":"text","text":"private prompt result"}], "usage":{"input":123}});
        entries.push(assistant.clone());
        let (root, path) = journal(&entries);
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        let migrated = read(&path);
        let models = resumed(&migrated, "f");
        assert_eq!(models["default"], SAGE);
        assert_eq!(models["smol"], SWIFT);
        assert_eq!(models["plan"], SAGE);
        assert_eq!(models["slow"], SAGE);
        assert_eq!(models["custom"], "other/rayline-router");
        assert_eq!(migrated.last().unwrap(), &assistant);
        for (before, after) in entries.iter().zip(migrated.iter().skip(1)) {
            for key in ["id", "parentId", "timestamp"] {
                assert_eq!(before[key], after[key]);
            }
        }
        assert_eq!(migrated[0]["parentSession"], "/old/session.jsonl");
        let once = fs::read(&path).unwrap();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        assert_eq!(once, fs::read(&path).unwrap());
    }

    #[test]
    fn inferred_legacy_branches_get_explicit_selection_without_history_edits() {
        let mut assistant = entry("a", None, "message");
        assistant["message"] =
            json!({"role":"assistant","provider":"rayline","model":"rayline-router","content":[]});
        let mut other = entry("b", None, "model_change");
        other["model"] = json!("other/model");
        let (root, path) = journal(&[assistant.clone(), other.clone()]);
        let before = fs::read_to_string(&path).unwrap();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        let entries = read(&path);
        assert_eq!(&entries[1], &assistant);
        assert_eq!(&entries[3], &other);
        assert_eq!(entries[2]["parentId"], "a");
        assert_eq!(
            resumed(&entries, entries[2]["id"].as_str().unwrap())["default"],
            SAGE
        );
        assert_eq!(resumed(&entries, "b")["default"], "other/model");
        let after = fs::read_to_string(&path).unwrap();
        for line in before.lines() {
            assert!(after.lines().any(|new| new == line));
        }
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        assert_eq!(after, fs::read_to_string(&path).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn unrelated_history_is_byte_identical_and_not_replaced() {
        use std::os::unix::fs::MetadataExt;
        let mut assistant = entry("a", None, "message");
        assistant["message"] =
            json!({"role":"assistant","provider":"other","model":"rayline-router","content":[]});
        let (root, path) = journal(&[assistant]);
        let before = fs::read(&path).unwrap();
        let inode = fs::metadata(&path).unwrap().ino();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        assert_eq!(before, fs::read(&path).unwrap());
        assert_eq!(inode, fs::metadata(&path).unwrap().ino());
    }

    #[test]
    fn corrupt_and_partial_records_fail_without_exposing_or_modifying_payloads() {
        for suffix in [
            "{\"type\":\"message\",\"model\":\"rayline-router\",\"secret\":\"private",
            "not JSON rayline-router private\n",
        ] {
            let (root, path) = journal(&[]);
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(suffix.as_bytes())
                .unwrap();
            let before = fs::read(&path).unwrap();
            let error = migrate(&root, &[], &super::super::tests::hosted_models())
                .unwrap_err()
                .to_string();
            assert!(!error.contains("private"));
            assert_eq!(before, fs::read(&path).unwrap());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_files_and_directories_are_not_followed() {
        use std::os::unix::fs::symlink;
        let (root, path) = journal(&[]);
        let original = fs::read(&path).unwrap();
        let link = root.join("sessions/linked.jsonl");
        symlink(&path, &link).unwrap();
        assert!(migrate(&root, &[], &super::super::tests::hosted_models()).is_err());
        assert_eq!(original, fs::read(&path).unwrap());
        fs::remove_file(link).unwrap();
        symlink(path.parent().unwrap(), root.join("sessions/linked-dir")).unwrap();
        assert!(migrate(&root, &[], &super::super::tests::hosted_models()).is_err());
    }

    #[test]
    fn held_ownership_lease_and_publish_gate_prevent_replacement() {
        let mut selection = entry("a", None, "model_change");
        selection["model"] = json!(LEGACY);
        let (root, path) = journal(&[selection]);
        let before = fs::read(&path).unwrap();
        let id = read(&path)[0]["id"].as_str().unwrap().to_owned();
        // Prepare the directory, then emulate the real utils/file-lock.ts
        // wrapper independently: it appends ".lock" to owners-dir/session-id.
        drop(acquire_lease(&id).unwrap());
        let owners = std::env::var_os("PI_TEST_SESSION_OWNERS_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let xdg = if cfg!(any(target_os = "linux", target_os = "macos")) {
                    std::env::var_os("XDG_STATE_HOME").map(|p| PathBuf::from(p).join("omp"))
                } else {
                    None
                };
                let root = xdg.filter(|p| p.exists()).unwrap_or_else(|| {
                    let home_key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
                    PathBuf::from(std::env::var_os(home_key).unwrap()).join(
                        std::env::var_os("PI_CONFIG_DIR")
                            .filter(|p| !p.is_empty())
                            .unwrap_or_else(|| ".omp".into()),
                    )
                });
                root.join("run/session-owners")
            });
        let lease =
            native_lock(&std::path::absolute(owners.join(format!("{id}.lock"))).unwrap()).unwrap();
        assert!(migrate(&root, &[], &super::super::tests::hosted_models()).is_err());
        assert_eq!(before, fs::read(&path).unwrap());
        drop(lease);
        let gate = native_lock(&path.with_file_name(".history.jsonl.lock.os")).unwrap();
        assert!(migrate(&root, &[], &super::super::tests::hosted_models()).is_err());
        assert_eq!(before, fs::read(&path).unwrap());
        drop(gate);
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
    }

    #[test]
    fn explicit_external_file_and_directory_are_migrated() {
        let mut selection = entry("a", None, "model_change");
        selection["provider"] = json!("rayline");
        selection["modelId"] = json!("rayline-router");
        selection["role"] = json!("smol");
        let (root, path) = journal(&[selection]);
        let empty = root.join("different-config");
        fs::create_dir(&empty).unwrap();
        migrate(
            &empty,
            &[path.clone()],
            &super::super::tests::hosted_models(),
        )
        .unwrap();
        assert_eq!(read(&path)[1]["model"], SWIFT);
        assert_eq!(read(&path)[1]["modelId"], "arc-swift-1.0");
        migrate(
            &empty,
            &[path.parent().unwrap().to_owned()],
            &super::super::tests::hosted_models(),
        )
        .unwrap();
    }

    #[test]
    fn legacy_selections_move_to_the_keys_listed_ids() {
        let contributor = super::super::LaunchModels::from_listing(&super::super::tests::listing(
            Some("contributor"),
        ))
        .unwrap();
        let mut default = entry("a", None, "model_change");
        default["model"] = json!(LEGACY);
        let mut smol = entry("b", Some("a"), "model_change");
        smol["model"] = json!(LEGACY);
        smol["role"] = json!("smol");
        let (root, path) = journal(&[default, smol]);
        migrate(&root, &[], &contributor).unwrap();
        let migrated = read(&path);
        assert_eq!(migrated[1]["model"], "rayline/arc-sage-contributor-1.0");
        assert_eq!(migrated[2]["model"], "rayline/arc-swift-contributor-1.0");
    }

    #[test]
    fn journal_append_during_replacement_is_rejected() {
        let mut selection = entry("a", None, "model_change");
        selection["model"] = json!(LEGACY);
        let (_root, path) = journal(&[selection]);
        let source = omp_migration::open(&path).unwrap().unwrap();
        let planned = plan(&source.file, &super::super::tests::hosted_models()).unwrap();
        assert!(!planned.replacements.is_empty());
        let before = fs::read(&path).unwrap();
        let appended = b"{\"type\":\"custom\",\"id\":\"b\",\"parentId\":\"a\",\"timestamp\":\"now\",\"data\":\"private\"}\n";
        let result = omp_migration::replace(&path, &source, |out| {
            out.write_all(b"replacement")?;
            OpenOptions::new()
                .append(true)
                .open(&path)?
                .write_all(appended)
        });
        assert!(result.is_err());
        assert_eq!(
            fs::read(&path).unwrap(),
            [before.as_slice(), appended].concat()
        );
    }

    #[test]
    fn artifacts_are_not_scanned_as_session_journals() {
        let (root, path) = journal(&[]);
        let artifacts = path.with_extension("");
        fs::create_dir(&artifacts).unwrap();
        fs::write(
            artifacts.join("tool-output.jsonl"),
            "not a session or even JSON",
        )
        .unwrap();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
    }

    #[test]
    fn pre_tree_inferred_sessions_get_a_selection_without_changing_old_records() {
        let mut assistant = entry("a", None, "message");
        assistant["message"] =
            json!({"role":"assistant","provider":"rayline","model":"rayline-router","content":[]});
        let (root, path) = journal(&[assistant]);
        let mut records = read(&path);
        records[0].as_object_mut().unwrap().remove("version");
        records[1].as_object_mut().unwrap().remove("id");
        records[1].as_object_mut().unwrap().remove("parentId");
        let mut out = File::create(&path).unwrap();
        for record in &records {
            writeln!(out, "{record}").unwrap();
        }
        drop(out);
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        let migrated = read(&path);
        assert_eq!(&migrated[..2], &records);
        assert_eq!(migrated[2]["model"], SAGE);
        // omp's v1 migration assigns sequential ids/parents to every entry.
        let once = fs::read(&path).unwrap();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        assert_eq!(once, fs::read(&path).unwrap());
    }

    #[test]
    fn unrelated_partial_or_unknown_journals_are_not_parsed_or_replaced() {
        for suffix in [
            "{\"type\":\"message\",\"secret\":\"private",
            "{\"secret\":\"escaped\\nprivate",
            "unknown non-JSON format",
        ] {
            let (root, path) = journal(&[]);
            OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(suffix.as_bytes())
                .unwrap();
            let before = fs::read(&path).unwrap();
            migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
            assert_eq!(before, fs::read(&path).unwrap());
        }
    }

    #[test]
    fn unicode_escaped_legacy_ids_are_detected_and_migrated() {
        let mut selection = entry("a", None, "model_change");
        selection["model"] = json!(LEGACY);
        let (root, path) = journal(&[selection]);
        let escaped = fs::read_to_string(&path)
            .unwrap()
            .replace("rayline-router", r"rayline\u002drouter");
        fs::write(&path, escaped).unwrap();
        migrate(&root, &[], &super::super::tests::hosted_models()).unwrap();
        assert_eq!(read(&path)[1]["model"], SAGE);
    }

    #[cfg(unix)]
    #[test]
    fn managed_sessions_root_symlink_is_rejected() {
        use std::os::unix::fs::symlink;
        let (root, path) = journal(&[]);
        let before = fs::read(&path).unwrap();
        let outside = root.join("outside");
        fs::rename(root.join("sessions"), &outside).unwrap();
        symlink(&outside, root.join("sessions")).unwrap();
        assert!(migrate(&root, &[], &super::super::tests::hosted_models()).is_err());
        assert_eq!(
            before,
            fs::read(outside.join("project/history.jsonl")).unwrap()
        );
    }
}
