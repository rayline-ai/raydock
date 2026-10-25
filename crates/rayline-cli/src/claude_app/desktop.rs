//! Claude Desktop's documented local profile library. Keep the compatibility
//! boundary here: no app patching, managed preferences, or subscription cookies.
#[cfg(any(target_os = "macos", test))]
use super::{AppPaths, random_bytes};
use serde_json::Value;
#[cfg(any(target_os = "macos", test))]
use serde_json::json;
#[cfg(any(target_os = "macos", test))]
use std::fs;
use std::io;
use std::path::Path;
#[cfg(any(target_os = "macos", test))]
use std::path::PathBuf;

#[cfg(any(target_os = "macos", test))]
const STATE_VERSION: u64 = 1;
#[cfg(any(target_os = "macos", test))]
const MAX_JSON_BYTES: u64 = 1024 * 1024;

#[cfg(any(target_os = "macos", test))]
pub(super) struct ProfileStore {
    home: PathBuf,
    library: PathBuf,
    config: PathBuf,
    journal: PathBuf,
}

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

/// Refuse links, oversized files and unknown shapes rather than clobbering
/// another application's data. Absence is different from unreadable JSON.
#[cfg(any(target_os = "macos", test))]
fn read_object(path: &Path) -> io::Result<Option<Value>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if !metadata.file_type().is_file() || metadata.len() > MAX_JSON_BYTES {
        return Err(invalid(
            "Desktop configuration must be a regular JSON file under 1 MiB",
        ));
    }
    let value: Value = serde_json::from_slice(&fs::read(path)?)
        .map_err(|_| invalid("Desktop configuration contains invalid JSON"))?;
    if !value.is_object() {
        return Err(invalid("Desktop configuration must be a JSON object"));
    }
    Ok(Some(value))
}

#[cfg(any(target_os = "macos", test))]
fn uuid() -> io::Result<String> {
    let mut bytes = [0; 16];
    random_bytes(&mut bytes)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

#[cfg(any(target_os = "macos", test))]
fn valid_id(value: &Value) -> bool {
    value.as_str().is_some_and(|id| {
        id.len() == 36
            && id.bytes().enumerate().all(|(i, byte)| {
                if [8, 13, 18, 23].contains(&i) {
                    byte == b'-'
                } else {
                    byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                }
            })
    })
}

#[cfg(any(target_os = "macos", test))]
fn write_json(home: &Path, path: &Path, value: &Value) -> io::Result<()> {
    write_beneath(
        home,
        path,
        &serde_json::to_vec_pretty(value).map_err(io::Error::other)?,
    )
}

#[cfg(all(unix, any(target_os = "macos", test)))]
fn c_name(name: &std::ffi::OsStr) -> io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes()).map_err(|_| invalid("Invalid Desktop path component"))
}

#[cfg(all(unix, any(target_os = "macos", test)))]
fn owned_fd(fd: libc::c_int) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    if fd < 0 {
        let error = io::Error::last_os_error();
        return Err(if error.raw_os_error() == Some(libc::ELOOP) {
            invalid("Refusing a symlink in Desktop configuration paths")
        } else {
            error
        });
    }
    // SAFETY: `fd` is a freshly opened descriptor that nothing else owns.
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
}

#[cfg(all(unix, any(target_os = "macos", test)))]
fn check(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Atomically write `contents` (0600) to `path`, which must lie beneath
/// `home`. Every directory below `home` is opened with `O_NOFOLLOW` and the
/// temporary file is created and renamed relative to that descriptor, so a
/// symlink planted after validation cannot redirect the write. The directory
/// is synced after the rename so the journal ordering survives a crash.
#[cfg(all(unix, any(target_os = "macos", test)))]
fn write_beneath(home: &Path, path: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::path::Component;
    let relative = path
        .strip_prefix(home)
        .map_err(|_| invalid("Desktop configuration path is outside the home directory"))?;
    let name = relative
        .file_name()
        .ok_or_else(|| invalid("Invalid Desktop configuration path"))?;
    let directory = c_name(home.as_os_str())?;
    // SAFETY: `directory` is a valid NUL-terminated path for the call's duration.
    let mut dir = owned_fd(unsafe {
        libc::open(
            directory.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })?;
    for component in relative.parent().into_iter().flat_map(Path::components) {
        let Component::Normal(part) = component else {
            return Err(invalid("Invalid Desktop configuration path"));
        };
        let part = c_name(part)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `dir` is a live directory descriptor and `part` is NUL-terminated.
        let mut fd = unsafe { libc::openat(dir.as_raw_fd(), part.as_ptr(), flags) };
        if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::NotFound {
            // SAFETY: as above; mkdirat does not retain the pointer.
            if let Err(error) =
                check(unsafe { libc::mkdirat(dir.as_raw_fd(), part.as_ptr(), 0o700) })
            {
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(error);
                }
            }
            // SAFETY: as above.
            fd = unsafe { libc::openat(dir.as_raw_fd(), part.as_ptr(), flags) };
        }
        dir = owned_fd(fd)?;
    }
    // Keep the containing directory owner-only, as `write_private` does.
    // SAFETY: `dir` is a live descriptor.
    check(unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) })?;
    let name = c_name(name)?;
    let tmp = c_name(std::ffi::OsStr::new(&format!(
        ".{}.tmp-{}",
        String::from_utf8_lossy(name.as_bytes()),
        std::process::id()
    )))?;
    // SAFETY: `dir` is live and `tmp` is NUL-terminated.
    if let Err(error) = check(unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) }) {
        if error.kind() != io::ErrorKind::NotFound {
            return Err(error);
        }
    }
    // SAFETY: as above; O_EXCL|O_NOFOLLOW never opens an existing file or link.
    let file = owned_fd(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    })?;
    let mut file = fs::File::from(file);
    let committed = file
        .write_all(contents)
        .and_then(|()| file.sync_all())
        .and_then(|()| {
            // SAFETY: `dir` is live; both names are NUL-terminated and in `dir`.
            check(unsafe {
                libc::renameat(
                    dir.as_raw_fd(),
                    tmp.as_ptr(),
                    dir.as_raw_fd(),
                    name.as_ptr(),
                )
            })
        });
    if let Err(error) = committed {
        // SAFETY: `dir` is live and `tmp` is NUL-terminated.
        unsafe { libc::unlinkat(dir.as_raw_fd(), tmp.as_ptr(), 0) };
        return Err(error);
    }
    drop(file);
    fs::File::from(dir).sync_all()
}

#[cfg(all(not(unix), any(target_os = "macos", test)))]
fn write_beneath(_home: &Path, _path: &Path, _contents: &[u8]) -> io::Result<()> {
    Err(invalid("Automatic Desktop setup requires macOS"))
}

#[cfg(any(target_os = "macos", test))]
fn restore_key(target: &mut Value, key: &str, previous: &Value) {
    if previous.is_null() {
        if let Some(object) = target.as_object_mut() {
            object.remove(key);
        }
    } else {
        target[key] = previous.clone();
    }
}

/// An absent leaf underneath a symlink must not redirect a later atomic write.
#[cfg(any(target_os = "macos", test))]
fn check_no_symlinks(path: &Path) -> io::Result<()> {
    for ancestor in path.ancestors() {
        match fs::symlink_metadata(ancestor) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(invalid("Refusing a symlink in Desktop configuration paths"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
impl ProfileStore {
    pub(super) fn new(home: &Path) -> Self {
        let root = home.join("Library/Application Support/Claude-3p");
        Self {
            home: home.to_path_buf(),
            library: root.join("configLibrary"),
            config: root.join("claude_desktop_config.json"),
            journal: AppPaths::new(home).dir.join("desktop-profile.json"),
        }
    }

    fn meta(&self) -> io::Result<Option<Value>> {
        let meta = read_object(&self.library.join("_meta.json"))?;
        if let Some(meta) = &meta {
            let entries = meta["entries"]
                .as_array()
                .ok_or_else(|| invalid("Unknown Desktop profile library format; use --manual"))?;
            if !valid_id(&meta["appliedId"])
                || !entries.iter().any(|entry| entry["id"] == meta["appliedId"])
                || entries
                    .iter()
                    .any(|entry| !valid_id(&entry["id"]) || !entry["name"].is_string())
            {
                return Err(invalid("Invalid Desktop profile library; use --manual"));
            }
        }
        Ok(meta)
    }

    fn state(&self) -> io::Result<Option<Value>> {
        match fs::symlink_metadata(&self.journal) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
            Ok(_) => check_no_symlinks(&self.journal)?,
        }
        let state = read_object(&self.journal)?;
        if let Some(state) = &state {
            if state["version"] != STATE_VERSION
                || !valid_id(&state["profileId"])
                || !(state["previous"].is_null() || state["previous"].is_object())
            {
                return Err(invalid("Unknown Rayline Desktop recovery format"));
            }
            if let Some(previous) = state.get("previous").filter(|value| !value.is_null()) {
                if !valid_id(&previous["appliedId"])
                    || (!previous["mode"].is_null()
                        && !matches!(previous["mode"].as_str(), Some("1p" | "3p")))
                    || !matches!(state["phase"].as_str(), Some("prepared" | "active"))
                {
                    return Err(invalid("Invalid Rayline Desktop recovery record"));
                }
            }
        }
        Ok(state)
    }

    fn check_paths(&self) -> io::Result<()> {
        for path in [&self.library, &self.config, &self.journal] {
            check_no_symlinks(path)?;
        }
        Ok(())
    }

    fn require_profile(&self, id: &Value) -> io::Result<()> {
        if !valid_id(id) {
            return Err(invalid("Invalid Desktop profile id"));
        }
        let path = self.library.join(format!(
            "{}.json",
            id.as_str().ok_or_else(|| invalid("Missing profile id"))?
        ));
        if read_object(&path)?.is_none() {
            return Err(invalid(
                "Previous Desktop profile file is missing; cannot guarantee recovery",
            ));
        }
        Ok(())
    }

    /// Preflight is read-only and runs before quitting Desktop.
    pub(super) fn matches(&self, desired: &Value) -> io::Result<bool> {
        self.check_paths()?;
        let meta = self.meta()?;
        let state = self.state()?;
        let config = read_object(&self.config)?.unwrap_or_else(|| json!({}));
        if let Some(state) = state.as_ref().filter(|state| !state["previous"].is_null()) {
            if meta.as_ref().is_none_or(|meta| {
                meta["appliedId"] != state["profileId"] || !meta["hybridPointer"].is_null()
            }) || (state["phase"] == "active" && config["deploymentMode"] != "3p")
            {
                return Err(invalid(
                    "Desktop selection changed or setup was interrupted. Run `rayline claude-desktop off` to recover; if it reports a selection conflict, select Rayline ARC in Desktop before retrying off",
                ));
            }
            self.require_profile(&state["previous"]["appliedId"])?;
        } else if let Some(meta) = &meta {
            self.require_profile(&meta["appliedId"])?;
        }
        if !config["deploymentMode"].is_null()
            && !matches!(config["deploymentMode"].as_str(), Some("1p" | "3p"))
        {
            return Err(invalid("Unknown Desktop deployment mode; use --manual"));
        }
        if let (Some(meta), Some(state)) = (meta, state) {
            let path = self.library.join(format!(
                "{}.json",
                state["profileId"]
                    .as_str()
                    .ok_or_else(|| invalid("Missing profile id"))?
            ));
            let saved = read_object(&path)?;
            return Ok(state["phase"] == "active"
                && !state["previous"].is_null()
                && meta["appliedId"] == state["profileId"]
                && meta["hybridPointer"].is_null()
                && config["deploymentMode"] == "3p"
                && saved.as_ref() == Some(desired));
        }
        Ok(false)
    }

    pub(super) fn activate(&self, desired: &Value) -> io::Result<()> {
        self.matches(desired)?;
        let mut meta = match self.meta()? {
            Some(meta) => meta,
            None => {
                let id = uuid()?;
                write_json(
                    &self.home,
                    &self.library.join(format!("{id}.json")),
                    &json!({}),
                )?;
                let meta = json!({"appliedId": id, "entries": [{"id": id, "name": "Default"}]});
                write_json(&self.home, &self.library.join("_meta.json"), &meta)?;
                meta
            }
        };
        let mut config = read_object(&self.config)?.unwrap_or_else(|| json!({}));
        let mut state = self
            .state()?
            .unwrap_or(json!({"version": STATE_VERSION, "profileId": uuid()?, "previous": null}));
        let id = state["profileId"]
            .as_str()
            .ok_or_else(|| invalid("Missing profile id"))?
            .to_owned();
        if state["previous"].is_null() {
            // The journal is committed before either selection changes. A failed
            // launch or interrupted write can be undone with `off`.
            state["previous"] = json!({"appliedId": meta["appliedId"], "hybridPointer": meta["hybridPointer"], "mode": config["deploymentMode"]});
        } else if meta["appliedId"] != id {
            return Err(invalid(
                "Desktop selection changed or setup was interrupted; run `rayline claude-desktop off` before retrying",
            ));
        }
        state["phase"] = json!("prepared");
        write_json(&self.home, &self.journal, &state)?;
        write_json(
            &self.home,
            &self.library.join(format!("{id}.json")),
            desired,
        )?;
        let entries = meta["entries"]
            .as_array_mut()
            .ok_or_else(|| invalid("Missing profile entries"))?;
        if !entries.iter().any(|entry| entry["id"] == id) {
            entries.push(json!({"id": id, "name": "Rayline ARC"}));
        }
        meta["appliedId"] = json!(id);
        restore_key(&mut meta, "hybridPointer", &Value::Null);
        write_json(&self.home, &self.library.join("_meta.json"), &meta)?;
        config["deploymentMode"] = json!("3p");
        write_json(&self.home, &self.config, &config)?;
        state["phase"] = json!("active");
        write_json(&self.home, &self.journal, &state)
    }

    /// Restore only selection keys. Preferences and other saved profiles may
    /// have changed while Claude was running and must survive `off`.
    fn recovery(&self) -> io::Result<Option<(Value, Value, Value)>> {
        let Some(state) = self.state()? else {
            return Ok(None);
        };
        if state["previous"].is_null() {
            return Ok(None);
        }
        self.check_paths()?;
        let previous = &state["previous"];
        let meta = self.meta()?.ok_or_else(|| {
            invalid("Desktop profile library disappeared; recovery record retained")
        })?;
        let config = read_object(&self.config)?.unwrap_or_else(|| json!({}));
        let own_selection =
            meta["appliedId"] == state["profileId"] && meta["hybridPointer"].is_null();
        let previous_selection = meta["appliedId"] == previous["appliedId"]
            && meta["hybridPointer"] == previous["hybridPointer"];
        let prepared = state["phase"] == "prepared";
        let mode_matches = config["deploymentMode"] == "3p"
            || (prepared && config["deploymentMode"] == previous["mode"]);
        if !(own_selection || (prepared && previous_selection)) || !mode_matches {
            return Err(invalid(
                "Desktop selection was changed outside Rayline; recovery record retained, no settings restored",
            ));
        }
        if !meta["entries"].as_array().is_some_and(|entries| {
            entries
                .iter()
                .any(|entry| entry["id"] == previous["appliedId"])
        }) {
            return Err(invalid(
                "Previous Desktop profile is missing; recovery record retained",
            ));
        }
        self.require_profile(&previous["appliedId"])?;
        Ok(Some((state, meta, config)))
    }

    pub(super) fn restore(&self) -> io::Result<bool> {
        let Some((mut state, mut meta, mut config)) = self.recovery()? else {
            return Ok(false);
        };
        // Leave the journal recoverable until both writes succeed.
        state["phase"] = json!("prepared");
        write_json(&self.home, &self.journal, &state)?;
        restore_key(&mut config, "deploymentMode", &state["previous"]["mode"]);
        write_json(&self.home, &self.config, &config)?;
        meta["appliedId"] = state["previous"]["appliedId"].clone();
        restore_key(
            &mut meta,
            "hybridPointer",
            &state["previous"]["hybridPointer"],
        );
        write_json(&self.home, &self.library.join("_meta.json"), &meta)?;
        state["previous"] = Value::Null;
        state["phase"] = json!("idle");
        write_json(&self.home, &self.journal, &state)?;
        Ok(true)
    }
}

#[cfg(any(target_os = "macos", test))]
fn desktop_running_for_user(processes: &str, uid: u32) -> bool {
    processes.lines().any(|line| {
        let Some((owner, command)) = line.trim().split_once(char::is_whitespace) else {
            return false;
        };
        owner.parse::<u32>().ok() == Some(uid)
            && command
                .trim()
                .ends_with("/Claude.app/Contents/MacOS/Claude")
    })
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn app_path(home: &Path) -> io::Result<PathBuf> {
        [PathBuf::from("/Applications/Claude.app"), home.join("Applications/Claude.app")]
            .into_iter().find(|path| path.join("Contents/MacOS/Claude").is_file())
            .ok_or_else(|| invalid("Claude.app is not installed in /Applications or ~/Applications; install it or use --manual"))
    }

    pub(super) fn preflight(home: &Path) -> io::Result<PathBuf> {
        let app = app_path(home)?;
        // Only inspect keys, never print policy values (which may be secrets).
        let account = Command::new("/usr/bin/id").arg("-un").output()?;
        if !account.status.success() {
            return Err(invalid("Cannot determine macOS account name"));
        }
        let user = std::str::from_utf8(&account.stdout)
            .map_err(|_| invalid("Invalid macOS account name"))?
            .trim();
        if user.is_empty() || user.contains('/') || matches!(user, "." | "..") {
            return Err(invalid("Invalid macOS account name"));
        }
        for path in [
            PathBuf::from("/Library/Managed Preferences/com.anthropic.claudefordesktop.plist"),
            PathBuf::from("/Library/Managed Preferences")
                .join(user)
                .join("com.anthropic.claudefordesktop.plist"),
        ] {
            if !path.try_exists()? {
                continue;
            }
            let output = Command::new("/usr/bin/plutil")
                .args(["-convert", "json", "-o", "-"])
                .arg(path)
                .output()?;
            if !output.status.success() {
                return Err(invalid(
                    "Cannot read Claude managed preferences; use --manual",
                ));
            }
            let policy: Value = serde_json::from_slice(&output.stdout)
                .map_err(|_| invalid("Cannot parse Claude managed preferences"))?;
            let behavior = [
                "disableAutoUpdates",
                "autoUpdaterEnforcementHours",
                "installedUpdateEnforcementEnabled",
                "installedUpdateEnforcementHours",
                "updateViaUpdatesHost",
                "releaseChannel",
                "relaunchEnforcementHours",
                "configRecheckIntervalMinutes",
                "egressProxyUrl",
                "egressProxyPacUrl",
            ];
            if policy
                .as_object()
                .is_none_or(|keys| keys.keys().any(|key| !behavior.contains(&key.as_str())))
            {
                return Err(invalid(
                    "Claude Desktop is managed by your organization; local profile automation is unavailable. Use --manual",
                ));
            }
        }
        Ok(app)
    }

    fn running() -> io::Result<bool> {
        let output = Command::new("/bin/ps")
            .args(["-axo", "uid=,comm="])
            .output()?;
        if !output.status.success() {
            return Err(invalid("Cannot check whether Claude Desktop is running"));
        }
        // SAFETY: geteuid takes no arguments and returns the calling process's uid.
        let uid = unsafe { libc::geteuid() };
        Ok(desktop_running_for_user(
            &String::from_utf8_lossy(&output.stdout),
            uid,
        ))
    }

    fn quit() -> io::Result<()> {
        if !running()? {
            return Ok(());
        }
        eprintln!("Asking Claude Desktop to quit so its inference profile can change.");
        let mut child = Command::new("/usr/bin/osascript")
            .args([
                "-e",
                "tell application id \"com.anthropic.claudefordesktop\" to quit",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    return Err(invalid(
                        "Claude declined to quit; no profile changes made. Quit it manually and retry",
                    ));
                }
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(invalid(
                    "Claude did not quit in 15 seconds; no profile changes made. Quit it manually and retry",
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        while running()? {
            if Instant::now() >= deadline {
                return Err(invalid(
                    "Claude is still running; no profile changes made. Quit all copies and retry",
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }

    fn open(app: &Path) -> io::Result<()> {
        let status = Command::new("/usr/bin/open")
            .arg("-a")
            .arg(app)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err(invalid("macOS could not open Claude Desktop"));
        }
        Ok(())
    }

    pub(super) fn launch(home: &Path, desired: &Value) -> io::Result<String> {
        let store = ProfileStore::new(home);
        store.check_paths()?;
        let app = preflight(home)?;
        let changed = !store.matches(desired)?;
        if changed {
            eprintln!(
                "Selecting Rayline ARC for the whole Claude Desktop app; Chat and Cowork are off in this profile. Your previous selection is saved for `rayline claude-desktop off`."
            );
            quit()?;
            if let Err(error) = store.activate(desired) {
                let recovery = store.restore();
                if recovery.is_ok() {
                    let _ = open(&app);
                }
                return Err(invalid(&format!(
                    "Could not apply Desktop profile: {error}. Recovery: {recovery:?}. Retry `off` if needed"
                )));
            }
        }
        if let Err(error) = open(&app) {
            if changed {
                let recovery = store.restore();
                return Err(invalid(&format!(
                    "{error}. Recovery: {recovery:?}. Retry `off` if needed"
                )));
            }
            return Err(error);
        }
        Ok("Selected the Rayline ARC profile and asked macOS to open Claude Desktop.\nOpen Code and select arc-sage or arc-swift. If prompted, continue with the configured gateway.\nUse `rayline claude-desktop off` to restore your previous profile.\n".to_owned())
    }

    pub(super) fn restore(home: &Path) -> io::Result<bool> {
        let store = ProfileStore::new(home);
        // Recovery must not depend on an installed app or today's managed
        // policy. Validate it before interrupting the app, and recheck after quit.
        if store.recovery()?.is_none() {
            return Ok(false);
        }
        quit()?;
        store.restore()?;
        match app_path(home) {
            Ok(app) => {
                if let Err(error) = open(&app) {
                    eprintln!(
                        "Desktop settings restored, but reopening failed: {error}. Open Claude manually when available."
                    );
                }
            }
            Err(_) => eprintln!(
                "Desktop settings restored; Claude.app is unavailable, so it was not reopened."
            ),
        }
        Ok(true)
    }
}

pub(super) fn preflight(home: &Path) -> io::Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos::preflight(home)?;
        ProfileStore::new(home).matches(&json!({}))?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        Err(invalid(
            "Automatic Desktop setup is macOS-only; use --manual for router-only setup",
        ))
    }
}

pub(super) fn launch(home: &Path, desired: &Value) -> io::Result<String> {
    #[cfg(target_os = "macos")]
    {
        macos::launch(home, desired)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (home, desired);
        Err(invalid(
            "Automatic Desktop setup is macOS-only; use --manual",
        ))
    }
}

pub(super) fn restore(home: &Path) -> io::Result<bool> {
    #[cfg(target_os = "macos")]
    {
        macos::restore(home)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = home;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        home: PathBuf,
        store: ProfileStore,
    }
    impl Fixture {
        fn new() -> Self {
            let home = std::env::temp_dir()
                .canonicalize()
                .expect("temp directory")
                .join(format!("rayline-desktop-{}", uuid().expect("random id")));
            fs::create_dir_all(&home).expect("create home");
            let store = ProfileStore::new(&home);
            Self { home, store }
        }
        fn seed(&self, mode: Value) -> Value {
            let id = "11111111-1111-4111-8111-111111111111";
            let meta = json!({"appliedId":id,"entries":[{"id":id,"name":"Existing gateway"}],"hybridPointer":{"bootstrapUrl":"https://example.test/bootstrap"},"futureField":7});
            write_json(&self.home, &self.store.library.join(format!("{id}.json")), &json!({"inferenceProvider":"gateway","inferenceGatewayBaseUrl":"https://example.test"})).expect("profile");
            write_json(&self.home, &self.store.library.join("_meta.json"), &meta).expect("meta");
            let mut config = json!({"unrelatedPreference":true});
            if !mode.is_null() {
                config["deploymentMode"] = mode;
            }
            write_json(&self.home, &self.store.config, &config).expect("config");
            meta
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    fn desired(token: &str) -> Value {
        super::super::desktop_import_json("http://127.0.0.1:20831", token)
    }

    #[test]
    fn selecting_and_restoring_preserves_profiles_and_new_preferences() {
        for mode in [Value::Null, json!("1p"), json!("3p")] {
            let f = Fixture::new();
            let original = f.seed(mode.clone());
            let other = fs::read(f.store.library.join(format!(
                "{}.json",
                original["appliedId"].as_str().expect("id")
            )))
            .expect("other profile");
            f.store.activate(&desired("test-token")).expect("activate");
            assert!(f.store.matches(&desired("test-token")).expect("matches"));
            let mut config = read_object(&f.store.config).expect("read").expect("config");
            config["preferenceAddedInDesktop"] = json!(42);
            write_json(&f.store.home, &f.store.config, &config).expect("new preference");
            assert!(f.store.restore().expect("restore"));
            let meta = f.store.meta().expect("read").expect("meta");
            assert_eq!(meta["appliedId"], original["appliedId"]);
            assert_eq!(meta["hybridPointer"], original["hybridPointer"]);
            assert_eq!(meta["futureField"], 7);
            assert_eq!(meta["entries"].as_array().expect("entries").len(), 2);
            let config = read_object(&f.store.config).expect("read").expect("config");
            assert_eq!(config["deploymentMode"], mode);
            assert_eq!(config["preferenceAddedInDesktop"], 42);
            assert_eq!(config["unrelatedPreference"], true);
            assert_eq!(
                fs::read(f.store.library.join(format!(
                    "{}.json",
                    original["appliedId"].as_str().expect("id")
                )))
                .expect("other profile"),
                other
            );
            assert!(!f.store.restore().expect("second off"));
        }
    }

    #[test]
    fn first_install_and_repeated_start_keep_one_profile_and_original_recovery() {
        let f = Fixture::new();
        f.store
            .activate(&desired("first-token"))
            .expect("first start");
        let initial = f.store.state().expect("state").expect("state");
        f.store
            .activate(&desired("rotated-token"))
            .expect("second start");
        let updated = f.store.state().expect("state").expect("state");
        assert_eq!(updated["previous"], initial["previous"]);
        assert_eq!(updated["profileId"], initial["profileId"]);
        assert_eq!(
            f.store.meta().expect("meta").expect("meta")["entries"]
                .as_array()
                .expect("entries")
                .len(),
            2
        );
        assert!(f.store.matches(&desired("rotated-token")).expect("matches"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [
                &f.store.journal,
                &f.store.config,
                &f.store.library.join(format!(
                    "{}.json",
                    updated["profileId"].as_str().expect("id")
                )),
            ] {
                assert_eq!(
                    fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                    0o600
                );
            }
        }
        assert!(f.store.restore().expect("restore"));
        assert_eq!(
            read_object(&f.store.config).expect("read").expect("config")["deploymentMode"],
            Value::Null
        );
        f.store
            .activate(&desired("rotated-token"))
            .expect("third start");
        assert_eq!(
            f.store.state().expect("state").expect("state")["profileId"],
            initial["profileId"]
        );
    }

    #[test]
    fn recovery_handles_interruption_between_selection_writes() {
        for selected_rayline in [false, true] {
            for mode_changed in [false, true] {
                let f = Fixture::new();
                let original = f.seed(json!("1p"));
                f.store.activate(&desired("test-token")).expect("activate");
                let mut state = f.store.state().expect("read").expect("state");
                state["phase"] = json!("prepared");
                write_json(&f.store.home, &f.store.journal, &state).expect("crash journal");
                if !selected_rayline {
                    write_json(
                        &f.store.home,
                        &f.store.library.join("_meta.json"),
                        &original,
                    )
                    .expect("original selection");
                }
                if !mode_changed {
                    write_json(
                        &f.store.home,
                        &f.store.config,
                        &json!({"deploymentMode":"1p"}),
                    )
                    .expect("old mode");
                }
                assert!(f.store.restore().expect("recover"));
                assert_eq!(
                    f.store.meta().expect("read").expect("meta")["appliedId"],
                    original["appliedId"]
                );
                assert_eq!(
                    read_object(&f.store.config).expect("read").expect("config")["deploymentMode"],
                    "1p"
                );
            }
        }
    }

    #[test]
    fn corrupt_or_unknown_configurations_are_not_replaced() {
        for bytes in [
            b"not json".as_slice(),
            b"[]",
            b"{\"appliedId\":\"../../escape\",\"entries\":[]}",
            b"{\"appliedId\":\"11111111-1111-4111-8111-111111111111\",\"entries\":[]}",
        ] {
            let f = Fixture::new();
            fs::create_dir_all(&f.store.library).expect("library");
            let path = f.store.library.join("_meta.json");
            fs::write(&path, bytes).expect("seed");
            assert!(f.store.activate(&desired("test-token")).is_err());
            assert_eq!(fs::read(path).expect("unchanged"), bytes);
            assert!(!f.store.journal.exists());
        }
    }

    #[test]
    fn external_selection_is_not_overwritten_by_off_or_retry() {
        for change in ["profile", "hybrid", "mode"] {
            let f = Fixture::new();
            let original = f.seed(json!("1p"));
            f.store.activate(&desired("test-token")).expect("activate");
            let mut meta = f.store.meta().expect("read").expect("meta");
            let mut config = read_object(&f.store.config).expect("read").expect("config");
            match change {
                "profile" => meta["appliedId"] = original["appliedId"].clone(),
                "hybrid" => {
                    meta["hybridPointer"] = json!({"bootstrapUrl":"https://changed.example.test"})
                }
                "mode" => config["deploymentMode"] = json!("1p"),
                _ => unreachable!(),
            }
            write_json(&f.store.home, &f.store.library.join("_meta.json"), &meta)
                .expect("external selection");
            write_json(&f.store.home, &f.store.config, &config).expect("external mode");
            let journal = fs::read(&f.store.journal).expect("journal");
            assert!(
                f.store.restore().is_err(),
                "off must reject changed {change}"
            );
            assert!(
                f.store.activate(&desired("new-token")).is_err(),
                "start must reject changed {change}"
            );
            assert_eq!(f.store.meta().expect("read").expect("meta"), meta);
            assert_eq!(
                read_object(&f.store.config).expect("read").expect("config"),
                config
            );
            assert_eq!(
                fs::read(&f.store.journal).expect("unchanged journal"),
                journal
            );
        }
    }

    #[test]
    fn activation_requires_a_recoverable_previous_profile() {
        for missing in [true, false] {
            let f = Fixture::new();
            let original = f.seed(json!("1p"));
            let path = f.store.library.join(format!(
                "{}.json",
                original["appliedId"].as_str().expect("id")
            ));
            if missing {
                fs::remove_file(path).expect("remove previous profile");
            } else {
                fs::write(path, b"invalid JSON").expect("corrupt previous profile");
            }
            let config = fs::read(&f.store.config).expect("original config");
            assert!(
                f.store.matches(&desired("test-token")).is_err(),
                "preflight must reject missing={missing}"
            );
            assert!(f.store.activate(&desired("test-token")).is_err());
            assert_eq!(f.store.meta().expect("read").expect("meta"), original);
            assert_eq!(fs::read(&f.store.config).expect("unchanged config"), config);
            assert!(!f.store.journal.exists());
        }
    }

    #[test]
    fn desktop_process_detection_ignores_other_users() {
        let other = "  502 /Applications/Claude.app/Contents/MacOS/Claude\n";
        assert!(!desktop_running_for_user(other, 501));
        assert!(desktop_running_for_user(other, 502));
        let own = "  501 /Volumes/Claude Installer/Claude.app/Contents/MacOS/Claude\n";
        assert!(desktop_running_for_user(own, 501));
        assert!(!desktop_running_for_user(
            "501 /Applications/Claude.app/Contents/Frameworks/Claude Helper",
            501
        ));
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_profile_changes_are_excluded_until_guard_drops() {
        let f = Fixture::new();
        let instance = AppPaths::new(&f.home).dir;
        fs::create_dir_all(&instance).expect("instance directory");
        let guard = super::super::lifecycle_lock(&f.home).expect("first lock");
        assert!(super::super::lifecycle_lock(&f.home).is_err());
        fs::remove_dir_all(instance).expect("uninstall instance files");
        assert!(
            super::super::lifecycle_lock(&f.home).is_err(),
            "uninstall must not replace the lock inode"
        );
        drop(guard);
        assert!(super::super::lifecycle_lock(&f.home).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn writes_never_follow_a_directory_swapped_for_a_symlink_after_validation() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let f = Fixture::new();
        let target = f.home.join("elsewhere");
        fs::create_dir_all(&target).expect("target");
        // The check passed earlier; the parent is now replaced by a link.
        let root = f.store.library.parent().expect("parent").to_path_buf();
        fs::create_dir_all(root.parent().expect("grandparent")).expect("grandparent");
        symlink(&target, &root).expect("swap in a link");
        let path = f.store.library.join("_meta.json");
        assert!(write_beneath(&f.home, &path, b"{}").is_err());
        assert_eq!(fs::read_dir(&target).expect("target").count(), 0);
        assert!(write_beneath(&f.home, &f.home.join("../outside.json"), b"{}").is_err());

        let fresh = f.home.join("a/b/c.json");
        write_beneath(&f.home, &fresh, b"{\"ok\":true}").expect("fresh write");
        assert_eq!(fs::read(&fresh).expect("read"), b"{\"ok\":true}");
        for (path, mode) in [(fresh.clone(), 0o600), (f.home.join("a/b"), 0o700)] {
            assert_eq!(
                fs::metadata(path).expect("metadata").permissions().mode() & 0o777,
                mode
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_configuration_paths_are_refused_without_modifying_the_target() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        let target = f.home.join("elsewhere");
        fs::create_dir_all(&target).expect("target");
        fs::create_dir_all(f.store.library.parent().expect("parent")).expect("parent");
        symlink(&target, &f.store.library).expect("link");
        assert!(f.store.activate(&desired("test-token")).is_err());
        assert!(
            f.store
                .recovery()
                .expect("manual setup needs no Desktop recovery")
                .is_none()
        );
        assert_eq!(fs::read_dir(&target).expect("target").count(), 0);
        assert!(!f.store.journal.exists());
    }
}
