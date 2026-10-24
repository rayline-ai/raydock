//! Claude Desktop's documented local profile library. Keep the compatibility
//! boundary here: no app patching, managed preferences, or subscription cookies.
#[cfg(any(target_os = "macos", test))]
use super::{AppPaths, private_dir, random_bytes, write_private};
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
fn write_json(path: &Path, value: &Value) -> io::Result<()> {
    write_private(
        path,
        &serde_json::to_vec_pretty(value).map_err(io::Error::other)?,
    )
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

#[cfg(any(target_os = "macos", test))]
impl ProfileStore {
    pub(super) fn new(home: &Path) -> Self {
        let root = home.join("Library/Application Support/Claude-3p");
        Self {
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
        // Inspect every existing ancestor too: an absent leaf beneath a symlink
        // must not cause an atomic write outside the intended directory.
        for path in [&self.library, &self.config, &self.journal] {
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
                write_json(&self.library.join(format!("{id}.json")), &json!({}))?;
                let meta = json!({"appliedId": id, "entries": [{"id": id, "name": "Default"}]});
                write_json(&self.library.join("_meta.json"), &meta)?;
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
        write_json(&self.journal, &state)?;
        write_json(&self.library.join(format!("{id}.json")), desired)?;
        let entries = meta["entries"]
            .as_array_mut()
            .ok_or_else(|| invalid("Missing profile entries"))?;
        if !entries.iter().any(|entry| entry["id"] == id) {
            entries.push(json!({"id": id, "name": "Rayline ARC"}));
        }
        meta["appliedId"] = json!(id);
        restore_key(&mut meta, "hybridPointer", &Value::Null);
        write_json(&self.library.join("_meta.json"), &meta)?;
        config["deploymentMode"] = json!("3p");
        write_json(&self.config, &config)?;
        state["phase"] = json!("active");
        write_json(&self.journal, &state)
    }

    /// Restore only selection keys. Preferences and other saved profiles may
    /// have changed while Claude was running and must survive `off`.
    fn recovery(&self) -> io::Result<Option<(Value, Value, Value)>> {
        self.check_paths()?;
        let Some(state) = self.state()? else {
            return Ok(None);
        };
        if state["previous"].is_null() {
            return Ok(None);
        }
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
        write_json(&self.journal, &state)?;
        restore_key(&mut config, "deploymentMode", &state["previous"]["mode"]);
        write_json(&self.config, &config)?;
        meta["appliedId"] = state["previous"]["appliedId"].clone();
        restore_key(
            &mut meta,
            "hybridPointer",
            &state["previous"]["hybridPointer"],
        );
        write_json(&self.library.join("_meta.json"), &meta)?;
        state["previous"] = Value::Null;
        state["phase"] = json!("idle");
        write_json(&self.journal, &state)?;
        Ok(true)
    }
}

/// Serialize Rayline profile edits. Desktop itself is stopped before writes.
#[cfg(all(unix, any(target_os = "macos", test)))]
fn profile_lock(home: &Path) -> io::Result<fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = AppPaths::new(home).dir;
    private_dir(&dir)?;
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("desktop-profile.lock"))?;
    // SAFETY: flock uses a live file descriptor and does not retain pointers.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(invalid(
            "Another Rayline command is changing the Desktop profile",
        ));
    }
    Ok(file)
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
        let user = home
            .file_name()
            .ok_or_else(|| invalid("Cannot determine macOS user"))?;
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
        let output = Command::new("/bin/ps").args(["-axo", "comm="]).output()?;
        if !output.status.success() {
            return Err(invalid("Cannot check whether Claude Desktop is running"));
        }
        Ok(String::from_utf8_lossy(&output.stdout).lines().any(|line| {
            line.trim_end()
                .ends_with("/Claude.app/Contents/MacOS/Claude")
        }))
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
        let _lock = profile_lock(home)?;
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
        store.check_paths()?;
        let _lock = profile_lock(home)?;
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
            write_json(&self.store.library.join(format!("{id}.json")), &json!({"inferenceProvider":"gateway","inferenceGatewayBaseUrl":"https://example.test"})).expect("profile");
            write_json(&self.store.library.join("_meta.json"), &meta).expect("meta");
            let mut config = json!({"unrelatedPreference":true});
            if !mode.is_null() {
                config["deploymentMode"] = mode;
            }
            write_json(&self.store.config, &config).expect("config");
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
            write_json(&f.store.config, &config).expect("new preference");
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
                write_json(&f.store.journal, &state).expect("crash journal");
                if !selected_rayline {
                    write_json(&f.store.library.join("_meta.json"), &original)
                        .expect("original selection");
                }
                if !mode_changed {
                    write_json(&f.store.config, &json!({"deploymentMode":"1p"})).expect("old mode");
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
            write_json(&f.store.library.join("_meta.json"), &meta).expect("external selection");
            write_json(&f.store.config, &config).expect("external mode");
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

    #[cfg(unix)]
    #[test]
    fn concurrent_profile_changes_are_excluded_until_guard_drops() {
        let f = Fixture::new();
        let guard = profile_lock(&f.home).expect("first lock");
        assert!(profile_lock(&f.home).is_err());
        drop(guard);
        assert!(profile_lock(&f.home).is_ok());
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
        assert_eq!(fs::read_dir(&target).expect("target").count(), 0);
        assert!(!f.store.journal.exists());
    }
}
